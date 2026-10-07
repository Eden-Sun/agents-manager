
    use super::*;
    use serde_json::json;
    use crate::runners::quota_claude::{force_probe, refresh_claude};

    /// #408：假 herdr 上跑 [`run_probe_pane`]；回傳 herdr 此刻還開著的 workspace label（Drop 的關閉是 spawn 出去的，等它一下）。
    mod probe_workspace {
        use super::*;
        use crate::testing::{Fault, MockHerdr};

        fn herdr() -> (MockHerdr, HerdrClient, std::path::PathBuf) {
            let dir = crate::testing::track(std::env::temp_dir().join(format!("am-qc-{}", crate::db::ulid())));
            std::fs::create_dir_all(&dir).unwrap();
            let sock = dir.join("herdr.sock");
            (MockHerdr::start(sock.clone()), HerdrClient::new(sock), dir)
        }

        async fn open_labels(h: &MockHerdr, want: usize) -> Vec<String> {
            for _ in 0..40 {
                if h.workspaces.lock().unwrap().len() <= want {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            h.workspaces.lock().unwrap().values().cloned().collect()
        }

        fn finished_screen() -> String {
            format!("{AUTH_BEGIN}\n{{\"loggedIn\":true}}\n{AUTH_END}\nTotal cost: $0\n{USAGE_DONE}0\n")
        }

        const LABEL: &str = "am-quota-claude-zz92la";

        #[tokio::test]
        async fn a_finished_probe_closes_its_workspace() {
            let (h, c, dir) = herdr();
            h.set_screen("*", &finished_screen());
            let out = run_probe_pane(&c, "/tmp", LABEL, json!({}), "true", Duration::from_secs(5)).await.unwrap();
            assert!(matches!(out, PaneRun::Done(ref a, _) if a.contains("loggedIn")), "{out:?}");
            assert_eq!(open_labels(&h, 0).await, Vec::<String>::new(), "成功也要關");
            std::fs::remove_dir_all(dir).ok();
        }

        #[tokio::test]
        async fn a_failed_rpc_still_closes_its_workspace() {
            let (h, c, dir) = herdr();
            h.fail_next("pane.send_text", Fault::Refuse);
            assert!(run_probe_pane(&c, "/tmp", LABEL, json!({}), "true", Duration::from_secs(5)).await.is_err());
            assert_eq!(open_labels(&h, 0).await, Vec::<String>::new(), "打字失敗（`?` 提早返回）也要關");
            std::fs::remove_dir_all(dir).ok();
        }

        #[tokio::test]
        async fn a_timed_out_probe_closes_its_workspace() {
            let (h, c, dir) = herdr();
            let out = run_probe_pane(&c, "/tmp", LABEL, json!({}), "true", Duration::from_millis(800)).await.unwrap();
            assert!(matches!(out, PaneRun::TimedOut(_)), "{out:?}");
            assert_eq!(open_labels(&h, 0).await, Vec::<String>::new(), "逾時也要關");
            std::fs::remove_dir_all(dir).ok();
        }

        #[tokio::test]
        async fn an_unreadable_probe_screen_does_not_retype_the_command() {
            let (h, c, dir) = herdr();
            h.set_screen("*", "__READ_ERROR__");
            let result = run_probe_pane(&c, "/tmp", LABEL, json!({}), "true", Duration::from_secs(6)).await;
            assert!(result.is_err(), "an unreadable pane is not evidence the command was swallowed");
            assert_eq!(h.calls_to("pane.send_text").len(), 1, "a read failure must not authorize another command");
            assert_eq!(h.calls_to("pane.send_keys").len(), 1, "no second Enter after an unreadable screen");
            assert_eq!(open_labels(&h, 0).await, Vec::<String>::new(), "read failure still closes the throwaway workspace");
            std::fs::remove_dir_all(dir).ok();
        }

        /// 外層（`force_probe` 的逾時、整個輪詢被取消）把 future 丟掉：`Drop` 補關。
        #[tokio::test]
        async fn a_dropped_probe_closes_its_workspace() {
            let (h, c, dir) = herdr();
            let cut = tokio::time::timeout(
                Duration::from_millis(300),
                run_probe_pane(&c, "/tmp", LABEL, json!({}), "true", Duration::from_secs(5)),
            )
            .await;
            assert!(cut.is_err(), "還在等殼起來就被丟掉");
            assert_eq!(open_labels(&h, 0).await, Vec::<String>::new(), "被丟掉的 future 也要關");
            std::fs::remove_dir_all(dir).ok();
        }

        /// 上一輪 `workspace.close` 失敗（ssh 斷一下）留下的殘留，不能等 daemon 重啟才收：下一輪開新的之前先收。
        /// 別人的 workspace（使用者的、grok 探測的）不碰。
        #[tokio::test]
        async fn a_leftover_from_a_failed_close_is_swept_by_the_next_probe() {
            let (h, c, dir) = herdr();
            c.workspace_create("/tmp", "proj", json!({})).await.unwrap();
            c.workspace_create("/tmp", "am-quota-grok", json!({})).await.unwrap();
            h.set_screen("*", &finished_screen());
            h.fail_next("workspace.close", Fault::Refuse);
            run_probe_pane(&c, "/tmp", LABEL, json!({}), "true", Duration::from_secs(5)).await.unwrap();
            let mut left = open_labels(&h, 3).await;
            left.sort();
            assert_eq!(left, vec!["am-quota-claude-zz92la", "am-quota-grok", "proj"], "前提：這一輪的 close 被拒");

            run_probe_pane(&c, "/tmp", "am-quota-claude", json!({}), "true", Duration::from_secs(5)).await.unwrap();
            let mut left = open_labels(&h, 2).await;
            left.sort();
            assert_eq!(left, vec!["am-quota-grok", "proj"], "殘留與這一輪的都收掉，別人的不動");
            std::fs::remove_dir_all(dir).ok();
        }

        /// #709：這一輪的 label 帶 `@<標記>`（共用 session 的主機）：只清同一個標記的殘留，別顆 daemon 的不動。
        #[tokio::test]
        async fn a_tagged_probe_sweeps_only_leftovers_with_its_own_tag() {
            let (h, c, dir) = herdr();
            c.workspace_create("/tmp", "am-quota-claude@mine", json!({})).await.unwrap();
            c.workspace_create("/tmp", "am-quota-claude@other", json!({})).await.unwrap();
            h.set_screen("*", &finished_screen());
            run_probe_pane(&c, "/tmp", "am-quota-claude-cc1@mine", json!({}), "true", Duration::from_secs(5)).await.unwrap();
            assert_eq!(open_labels(&h, 1).await, vec!["am-quota-claude@other"]);
            std::fs::remove_dir_all(dir).ok();
        }

        #[test]
        fn the_probe_labels_are_the_ones_the_pane_scan_leaves_out() {
            assert!(crate::panes::is_daemon_probe_workspace(Some(PROBE_LABEL_PREFIX)));
            assert!(crate::panes::is_daemon_probe_workspace(Some(&format!("{PROBE_LABEL_PREFIX}-cc1"))));
        }
    }

    /// 停用的身份不再探測，但**還在跑的例外**：停用是「別再挑它」，不是把正在用的額度弄瞎。
    #[test]
    fn a_disabled_identity_is_only_skipped_while_nothing_is_running_on_it() {
        let off = vec!["cc2".to_string()];
        let idle: std::collections::BTreeSet<String> = Default::default();
        let busy: std::collections::BTreeSet<String> = ["cc2".to_string()].into_iter().collect();
        assert!(skip_disabled(&off, &idle, "cc2"));
        assert!(!skip_disabled(&off, &busy, "cc2"), "還有 run 在跑就照探");
        assert!(!skip_disabled(&off, &idle, "cc1"), "沒被停用的不受影響");
        assert!(!skip_disabled(&[], &idle, "cc2"), "沒人被停用時什麼都不跳");
    }

    const SCREEN: &str = r#"
▎ Using Opus 5 (1M context) (from .claude/settings.json) · /model
   Settings  Status   Config   Usage   Stats

   Session

   Total cost:            $0.0000
   Usage:                 0 input, 0 output, 0 cache read, 0 cache write

   Current session
   ███████████████████████████████████████            78% used
   Resets 1:20pm (Asia/Taipei)

   Current week (all models)
   ███████████████▌                                   31% used
   Resets Sep 11 at 2pm (Asia/Taipei)
   +50% weekly limits promo through Sep 13 · clau.de/cc-50-promo

   Current week (Fable)
   ███████████████████▌                               39% used
   Resets Sep 11 at 2pm (Asia/Taipei)

   What's contributing to your limits usage?
"#;

    /// `claude -p "/usage"` 的純文字版（本機實測 2026-09-07）。
    const PLAIN: &str = r#"
You are currently using your subscription to power your Claude Code usage

Current session: 47% used · resets Sep 7 at 9:59pm (Asia/Taipei)
Current week (all models): 15% used · resets Sep 14 at 11:59am (Asia/Taipei)
Current week (Fable): 23% used · resets Sep 14 at 11:59am (Asia/Taipei)

What's contributing to your limits usage?
Last 24h · 2950 requests · 43 sessions
  83% of your usage was at >150k context
"#;

    fn at(s: &str) -> DateTime<Local> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Local)
    }

    /// claude 2.1.273 的 `/usage --output-format stream-json`：分桶看 `kind`、時間直接用 ISO，
    /// 不再解析「Sep 16 at 7:50am」。實機抓到的那一行（只留這段會用到的欄位）。
    #[test]
    fn the_structured_usage_report_fills_every_window() {
        let line = r#"{"type":"assistant","local_command_source":"<local-command-stdout>Current session: 30% used</local-command-stdout>","usage_report":{"session":{"total_cost_usd":0},"rate_limits":{"limits":[
          {"kind":"session","group":"session","percent":30,"resets_at":"2026-09-15T23:50:00.594923+00:00","scope":null,"severity":"normal","is_active":false},
          {"kind":"weekly_all","group":"weekly","percent":66,"resets_at":"2026-09-21T04:00:00.594948+00:00","scope":null,"severity":"normal","is_active":true},
          {"kind":"weekly_scoped","group":"weekly","percent":24,"resets_at":"2026-09-21T03:59:59.595145+00:00","scope":{"model":{"display_name":"Fable"},"surface":null},"severity":"normal","is_active":false},
          {"kind":"weekly_scoped","group":"weekly","percent":9,"resets_at":"2026-09-21T03:59:59.595145+00:00","scope":{"model":{"display_name":"Opus"},"surface":null},"severity":"normal","is_active":false}]}}}"#;
        let screen = format!("$ claude -p '/usage' --output-format stream-json
{}
AM_USAGE_DONE=0
", line.replace('\n', " "));
        let q = parse_claude_usage_report(&screen, Some("cc1")).expect("structured report");
        assert_eq!(q.five_hour.as_ref().unwrap().used_pct, 30.0);
        // 毫秒不再被截掉：以前 `Secs` 會把 `.594` 無條件捨去（總是往**早**的方向偏）。
        assert_eq!(q.five_hour.as_ref().unwrap().resets_at.as_deref(), Some("2026-09-15T23:50:00.594Z"));
        assert_eq!(q.seven_day.as_ref().unwrap().used_pct, 66.0);
        assert_eq!(q.fable.as_ref().unwrap().used_pct, 24.0, "weekly_scoped 的 Fable 列");
        assert_eq!(q.fable.as_ref().unwrap().resets_at.as_deref(), Some("2026-09-21T03:59:59.595Z"));
        assert_eq!(q.account.as_deref(), Some("cc1"));
        assert_eq!(q.source, "claude-usage");
    }

    /// Fable 列的 `display_name` 帶版號（`Fable 5.1`）也要認；別的模型（`Opus`）不能被當成 Fable。
    #[test]
    fn a_versioned_fable_name_still_fills_the_fable_window() {
        let row = |name: &str| format!(
            r#"{{"usage_report":{{"rate_limits":{{"limits":[{{"kind":"weekly_scoped","percent":41,"resets_at":"2026-09-21T03:59:59+00:00","scope":{{"model":{{"display_name":"{name}"}}}}}}]}}}}}}"#
        );
        assert_eq!(parse_claude_usage_report(&row("Fable 5.1"), None).and_then(|q| q.fable).map(|w| w.used_pct), Some(41.0));
        assert_eq!(parse_claude_usage_report(&row("fable"), None).and_then(|q| q.fable).map(|w| w.used_pct), Some(41.0));
        assert!(parse_claude_usage_report(&row("Opus 5"), None).is_none(), "別的模型的週列沒有對應的桶");
        assert!(parse_claude_usage_report(&row("Fabled"), None).is_none());
    }

    /// 舊 CLI 沒有這個欄位（`grep` 沒抓到 → 跑純文字版）：JSON 解析回 None，交給原本的文字解析。
    #[test]
    fn a_plain_text_usage_screen_is_left_to_the_text_parser() {
        let screen = "Current session: 30% used · resets Sep 16 at 7:50am
AM_USAGE_DONE=0
";
        assert!(parse_claude_usage_report(screen, None).is_none());
        assert!(parse_claude_usage(screen, at("2026-09-15T20:00:00+08:00"), None).is_some());
        // 有那一行但壞掉（截斷）也不能當成有資料。
        assert!(parse_claude_usage_report("{\"usage_report\":{\"rate_limits\":{\"limi", None).is_none());
    }

    #[test]
    fn session_and_week_all_models_map() {
        let q = parse_claude_usage(SCREEN, at("2026-09-06T10:00:00+08:00"), Some("cc0")).unwrap();
        assert_eq!(q.five_hour.as_ref().unwrap().used_pct, 78.0);
        assert_eq!(q.seven_day.as_ref().unwrap().used_pct, 31.0);
        assert_ne!(q.seven_day.as_ref().unwrap().used_pct, 39.0);
        assert_eq!(q.fable.as_ref().unwrap().used_pct, 39.0);
        let fable_reset = q.fable.as_ref().unwrap().resets_at.as_deref().unwrap();
        assert!(fable_reset.starts_with("2026-09-11"), "{fable_reset}");
        assert_eq!(q.source, "claude-usage");
        assert_eq!(q.account.as_deref(), Some("cc0"));
        let five_reset = q.five_hour.as_ref().unwrap().resets_at.as_deref().unwrap();
        assert!(five_reset.contains("T05:20:00") || five_reset.contains("T13:20:00"), "{five_reset}");
        let week_reset = q.seven_day.as_ref().unwrap().resets_at.as_deref().unwrap();
        assert!(week_reset.starts_with("2026-09-11"), "{week_reset}");
    }

    /// 2026-09-16 使用者：「怎麼不 show fable 用量了」。cc0 底下一直有 bot 在講話，狀態列每 30 秒
    /// 就刷一次，於是「狀態列很新就別開 pane」那條捷徑每一輪都成立，`/usage` 永遠輪不到——而 Fable
    /// 週窗與方案名**只有** `/usage` 讀得到。重啟前看得到只是因為 `quota::set` 會沿用舊的 `fable`。
    #[test]
    fn a_chatty_account_still_gets_its_usage_probe() {
        // 從沒問過 `/usage`（或已超過十分鐘）：狀態列再新也要開一次 pane，否則 Fable 那格永遠是空的。
        assert!(!skip_usage_probe(true, false));
        // 問過而且還新：不用再開 pane。
        assert!(skip_usage_probe(true, true));
        // 還不知道這個帳號登入了沒：登入答案跟 `/usage` 同一趟，值得一個 pane。
        assert!(!skip_usage_probe(false, true));
    }

    /// 2026-09-25 使用者：「m4p 的 cc1 一直被登出」。安靜的帳號（狀態列不新）以前每 60 秒就開一個
    /// `claude -p`，跟同 config dir 的 bot 搶 OAuth refresh；現在跟有講話的帳號一樣十分鐘一次。
    #[test]
    fn a_quiet_account_is_not_probed_every_poll() {
        let key = format!("claude:test-{}", crate::db::ulid());
        mark_usage_seen(&key);
        assert!(skip_usage_probe(true, usage_fresh(&key)), "十分鐘內問過就不開 pane，不管狀態列");
    }

    /// 記帳本身：沒問過就是不新，問過之後才在十分鐘內算數。
    #[test]
    fn the_usage_bookkeeping_starts_empty_and_expires() {
        let key = format!("claude:test-{}", crate::db::ulid());
        assert!(!usage_fresh(&key), "沒問過的 key 不能算新");
        mark_usage_seen(&key);
        assert!(usage_fresh(&key));
        // 十分鐘是上限：往後推時間就該重新問一次。不從 `Instant::now()` 往回減——開機不到十分鐘時那會 panic。
        assert!(!usage_fresh_at(&key, std::time::Instant::now() + USAGE_REFRESH), "超過 USAGE_REFRESH 就要再問一次");
    }

    #[test]
    fn no_fable_row_leaves_the_window_empty() {
        const NO_FABLE: &str = r#"
   Current session
   ███████████████████████████████████████            78% used
   Resets 1:20pm (Asia/Taipei)

   Current week (all models)
   ███████████████▌                                   31% used
   Resets Sep 11 at 2pm (Asia/Taipei)
"#;
        let q = parse_claude_usage(NO_FABLE, at("2026-09-06T10:00:00+08:00"), None).unwrap();
        assert_eq!(q.seven_day.unwrap().used_pct, 31.0);
        assert!(q.fable.is_none());
    }

    #[test]
    fn fable_header_is_case_insensitive() {
        const LOUD: &str = r#"
   Current session
   ███████████████████████████████████████            78% used
   Resets 1:20pm (Asia/Taipei)

   CURRENT WEEK (FABLE)
   ███████████████████▌                               39% used
   Resets Sep 11 at 2pm (Asia/Taipei)
"#;
        let q = parse_claude_usage(LOUD, at("2026-09-06T10:00:00+08:00"), None).unwrap();
        assert_eq!(q.fable.unwrap().used_pct, 39.0);
    }

    #[test]
    fn time_only_reset_rolls_to_tomorrow_when_past() {
        let r = parse_claude_reset("Resets 3:00pm (Asia/Taipei)", at("2026-09-06T16:00:00+08:00")).unwrap();
        assert!(r.starts_with("2026-09-07T"), "{r}");
        let r = parse_claude_reset("Resets 3:00pm (Asia/Taipei)", at("2026-09-06T10:00:00+08:00")).unwrap();
        assert!(r.starts_with("2026-09-06T"), "{r}");
    }

    #[test]
    fn month_day_reset_parses() {
        let r = parse_claude_reset("Resets Sep 11 at 2pm (Asia/Taipei)", at("2026-09-06T10:00:00+08:00")).unwrap();
        assert!(r.starts_with("2026-09-11T06:00:00") || r.starts_with("2026-09-11T14:00:00"), "{r}");
    }

    /// #59：橫幅標的時區（America/New_York，9 月是 EDT -04:00）跟本機時區（sandbox 是 Asia/Taipei
    /// +08:00）不一樣時，一定要照橫幅標的時區換算，不能悄悄套用本機時區——那會整整差 12 小時。
    #[test]
    fn reset_timezone_differs_from_host_local() {
        let r = parse_claude_reset("Resets 3:00pm (America/New_York)", at("2026-09-06T10:00:00+08:00")).unwrap();
        assert_eq!(r, "2026-09-06T19:00:00.000Z", "{r}");
    }

    /// 認不出來的時區要有明確的退路：回 `None`，不要拿本機時區頂上去湊一個看似合理、其實可能錯的時間。
    #[test]
    fn unrecognized_timezone_returns_none_instead_of_guessing() {
        assert!(parse_claude_reset("Resets 3:00pm (Mars/OlympusMons)", at("2026-09-06T10:00:00+08:00")).is_none());
    }

    /// 橫幅裡完全沒有時區括號（不是目前 CLI 會產生的格式，但要防呆）：同樣不猜，回 `None`。
    #[test]
    fn missing_timezone_parenthetical_returns_none() {
        assert!(parse_claude_reset("Resets 3:00pm", at("2026-09-06T10:00:00+08:00")).is_none());
    }

    #[test]
    fn loading_screen_is_not_a_quota() {
        assert!(parse_claude_usage("Settings  Status   Config   Usage\n  Loading…", Local::now(), None).is_none());
    }

    #[test]
    fn plain_text_usage_parses() {
        let q = parse_claude_usage(PLAIN, at("2026-09-07T12:00:00+08:00"), Some("cc1")).unwrap();
        assert_eq!(q.five_hour.as_ref().unwrap().used_pct, 47.0);
        assert_eq!(q.seven_day.as_ref().unwrap().used_pct, 15.0);
        assert_eq!(q.fable.as_ref().unwrap().used_pct, 23.0);
        assert_eq!(q.account.as_deref(), Some("cc1"));
        assert_eq!(q.source, "claude-usage");
        assert!(q.five_hour.as_ref().unwrap().resets_at.as_deref().unwrap().starts_with("2026-09-07"));
        assert!(q.seven_day.as_ref().unwrap().resets_at.as_deref().unwrap().starts_with("2026-09-14"));
        assert!(q.fable.as_ref().unwrap().resets_at.as_deref().unwrap().starts_with("2026-09-14"));
        assert_ne!(q.seven_day.as_ref().unwrap().used_pct, 83.0);
    }

    #[test]
    fn a_logged_out_run_is_not_a_quota() {
        const OUT: &str = "Total cost:            $0.0000\nTotal duration (API):  0s\nUsage: 0 input, 0 output\n";
        assert!(parse_claude_usage(OUT, Local::now(), None).is_none());
    }

    #[test]
    fn the_probe_command_carries_the_identity_env() {
        let mut env = BTreeMap::new();
        env.insert("CLAUDE_CONFIG_DIR".to_string(), "/home/u/.claude-cc1".to_string());
        env.insert("bad name".to_string(), "x".to_string());
        let cmd = probe_command("/opt/homebrew/bin/claude", &env, true);
        assert!(cmd.contains("CLAUDE_CONFIG_DIR='/home/u/.claude-cc1'"), "{cmd}");
        assert!(!cmd.contains("bad name"), "{cmd}");
        assert!(cmd.contains("auth status --json"), "{cmd}");
        assert!(cmd.contains("-p '/usage'"), "{cmd}");
        assert!(!cmd.contains(AUTH_BEGIN) && !cmd.contains(AUTH_END) && !cmd.contains(USAGE_DONE), "{cmd}");
    }

    /// #80：quota probe 只問登入狀態跟 `/usage`，從不用工具，MCP server 起得慢或掛掉不該拖慢探測、
    /// 甚至把探測拖到 timeout。claude 2.1.274 起 `CLAUDE_CODE_MCP_STARTUP_WAIT_MS=0` 讓第一個
    /// non-interactive turn 跳過等 MCP 連線；三個會跑 `claude` 的地方（auth status、結構化
    /// `/usage`、退回的純文字 `/usage`）都要吃得到，因為它跟身分自己的 env 一樣放進共用的 `pfx`。
    #[test]
    fn the_probe_command_skips_mcp_startup_wait() {
        let cmd = probe_command("/bin/claude", &BTreeMap::new(), true);
        assert_eq!(cmd.matches("CLAUDE_CODE_MCP_STARTUP_WAIT_MS=0").count(), 3, "{cmd}");

        // login-only target（不跑 /usage）：登入探測一樣不該被 MCP 拖住。
        let login_only = probe_command("/bin/claude", &BTreeMap::new(), false);
        assert_eq!(login_only.matches("CLAUDE_CODE_MCP_STARTUP_WAIT_MS=0").count(), 1, "{login_only}");
    }

    #[test]
    fn the_echoed_command_does_not_look_like_the_markers() {
        let cmd = probe_command("/bin/claude", &BTreeMap::new(), true);
        let screen = format!(
            "u@host ~ % {cmd}\n\n{AUTH_BEGIN}\n{{\"loggedIn\":true,\"email\":\"a@b.c\"}}\n\n{AUTH_END}\n{PLAIN}\n{USAGE_DONE}0\n"
        );
        let (auth, usage) = split_probe_output(&screen).expect("markers found");
        assert!(auth.contains("\"loggedIn\":true"), "{auth}");
        assert!(usage.contains("Current week (Fable)"), "{usage}");
        assert!(!usage.contains("loggedIn"), "{usage}");
        assert_eq!(crate::tools::read_login_answer("claude", &auth).1.as_deref(), Some("a@b.c"));
    }

    #[test]
    fn only_the_last_run_on_screen_counts() {
        let screen = format!(
            "{AUTH_BEGIN}\nzsh: no such file or directory\n{AUTH_END}\nzsh: no such file or directory\n{USAGE_DONE}127\n\
             {AUTH_BEGIN}\n{{\"loggedIn\":true}}\n{AUTH_END}\n{PLAIN}\n{USAGE_DONE}0\n"
        );
        let (auth, usage) = split_probe_output(&screen).expect("markers found");
        assert!(auth.contains("loggedIn"), "{auth}");
        assert!(!auth.contains("no such file"), "{auth}");
        assert_eq!(parse_claude_usage(&usage, at("2026-09-07T12:00:00+08:00"), None).unwrap().fable.unwrap().used_pct, 23.0);
    }

    /// 半截的 `/usage` 會少一條桶子。
    #[test]
    fn an_unfinished_run_has_no_answer_yet() {
        let screen = format!("{AUTH_BEGIN}\n{{\"loggedIn\":true}}\n{AUTH_END}\nCurrent session: 47% used\n");
        assert!(split_probe_output(&screen).is_none());
    }

    /// m4p's `cc1` right after a daemon restart: no statusLine yet, but a live run is proof enough.
    #[test]
    fn a_live_bot_run_beats_the_login_answer() {
        let e = ProbeEvidence { cli_says_logged_out: true, has_live_run: true, ..Default::default() };
        assert!(should_probe_identity(e));
        // 有 run 在跑，沒登入的 30 分鐘縮成 5 分鐘——但不是完全不退避（M4）。
        let k = format!("test/claude:{}", ulid::Ulid::new());
        let t0 = std::time::Instant::now();
        park(&k, true);
        let ten_min = t0 + Duration::from_secs(10 * 60);
        assert!(cooling_down_at(&k, false, false, ten_min), "沒有任何證據：沒登入的帳號等滿 30 分鐘");
        assert!(!cooling_down_at(&k, false, true, ten_min), "失敗之後開始有 bot 在用：5 分鐘就再問");
    }

    /// M4（review 2026-09-16）：`/usage` 壞掉（CLI 改格式、pane 卡住）時，一直有 bot 在講話的帳號以前無視退避，
    /// 每 60 秒開一個 pane、佔住 `probe_lock` 40 秒。現在一樣退避 5 分鐘。
    #[test]
    fn a_busy_account_still_backs_off_after_a_failed_probe() {
        let k = format!("test/claude:{}", ulid::Ulid::new());
        let t0 = std::time::Instant::now();
        park(&k, false);
        assert!(cooling_down_at(&k, true, true, t0 + Duration::from_secs(60)), "下一輪（60 秒後）不能又開 pane");
        let e = ProbeEvidence { reported_statusline: true, has_live_run: true, cooling_down: true, ..Default::default() };
        assert!(!should_probe_identity(e), "證據不再蓋過退避");
        assert!(!cooling_down_at(&k, true, true, t0 + RETRY_AFTER_FAILURE + Duration::from_secs(1)), "5 分鐘到了照樣再問");
    }

    fn ident(name: &str, env: &[(&str, &str)]) -> crate::config::IdentityCfg {
        crate::config::IdentityCfg {
            name: name.into(),
            kind: "claude".into(),
            host: None,
            env: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            args: vec![],
        }
    }

    /// M4：裸的預設帳號以前「永不 park」——`/usage` 一壞，cc0 這種一直有 bot 在講話的帳號每 60 秒開一個 pane。
    /// 現在它跟其他身分一樣吃退避，證據只影響退避長度。
    #[test]
    fn the_default_account_target_backs_off_like_any_other() {
        let identities = vec![ident("cc0", &[]), ident("cc1", &[("CLAUDE_CONFIG_DIR", "$HOME/.claude-cc1")])];
        let logins = BTreeMap::new();
        let live: std::collections::BTreeSet<String> = ["cc0".to_string(), "cc1".to_string()].into_iter().collect();
        let statusline: std::collections::BTreeSet<String> = ["claude".to_string(), "claude:cc1".to_string()].into_iter().collect();
        let input = PlanInput {
            host: "local",
            home: "/home/me",
            identities: &identities,
            logins: &logins,
            live: &live,
            off: &[],
            unnamed_running: false,
            statusline_keys: &statusline,
        };
        let keys = |ts: Vec<Target>| ts.into_iter().map(|t| t.key).collect::<Vec<_>>();
        assert_eq!(keys(plan_targets(&input, |_, _, _| false)), ["claude", "claude:cc1"]);
        assert_eq!(keys(plan_targets(&input, |k, _, _| k == "claude")), ["claude:cc1"], "預設帳號剛失敗：這一輪不開 pane");
        let bare = plan_targets(&input, |_, _, _| false).into_iter().next().unwrap();
        assert_eq!(bare.names, ["cc0"]);
        assert!(bare.evidence.has_live_run && bare.evidence.reported_statusline);
    }

    /// L2（review 2026-09-16）：`api` 只帶 `ANTHROPIC_API_KEY`、沒有自己的 `CLAUDE_CONFIG_DIR`。額度規則上它跟預設帳號
    /// 同一格（裸 `claude`），但以前連探測也併進裸 target、用**空 env** 問，預設帳號的 email／方案被記到它名下。
    #[test]
    fn an_identity_with_env_but_no_config_dir_is_asked_about_its_login_with_its_own_env() {
        let identities = vec![ident("cc0", &[]), ident("api", &[("ANTHROPIC_API_KEY", "sk-test")])];
        let logins = BTreeMap::new();
        let live = Default::default();
        let statusline = Default::default();
        let input = PlanInput { host: "local", home: "/h", identities: &identities, logins: &logins, live: &live, off: &[], unnamed_running: false, statusline_keys: &statusline };
        let ts = plan_targets(&input, |_, _, _| false);
        assert_eq!(ts.len(), 2);
        assert_eq!((ts[0].key.as_str(), ts[0].names.clone(), ts[0].login_only), ("claude", vec!["cc0".to_string()], false), "裸 target 只代表 env 真的是空的身分");
        assert_eq!((ts[1].key.as_str(), ts[1].login_only), ("claude:api", true));
        assert_eq!(ts[1].env.get("ANTHROPIC_API_KEY").map(String::as_str), Some("sk-test"), "登入答案要帶它自己的 env 問");
        let cmd = probe_command("/bin/claude", &ts[1].env, false);
        assert!(cmd.contains("auth status --json") && !cmd.contains("/usage"), "只問登入，不跑 /usage：{cmd}");
        assert_eq!(crate::quota::quota_base_default_aware("claude", Some("api"), crate::quota::identity_shares_default("claude", &identities[1].env)), "claude", "額度落點規則不變");
    }

    /// #404：`POST /api/quota/probe` 探的是那個身分的**額度落點**：cc0／只帶 API key 的身分都記在裸 `claude`，
    /// 有自己 `CLAUDE_CONFIG_DIR` 的探自己那一格；沒這個身分就沒有 target（API 回 404）。
    #[test]
    fn a_forced_probe_targets_where_that_accounts_quota_is_stored() {
        let identities = vec![
            ident("cc0", &[]),
            ident("cc1", &[("CLAUDE_CONFIG_DIR", "$HOME/.claude-cc1")]),
            ident("api", &[("ANTHROPIC_API_KEY", "sk-test")]),
        ];
        let (logins, live, statusline) = (BTreeMap::new(), Default::default(), Default::default());
        let input = PlanInput { host: "local", home: "/h", identities: &identities, logins: &logins, live: &live, off: &[], unnamed_running: true, statusline_keys: &statusline };
        let pick = |account: Option<&str>| forced_target(plan_targets(&input, |_, _, _| false), account).map(|t| (t.key, t.login_only, t.env.len()));
        assert_eq!(pick(None), Some(("claude".into(), false, 0)));
        assert_eq!(pick(Some("cc0")), Some(("claude".into(), false, 0)));
        assert_eq!(pick(Some("cc1")), Some(("claude:cc1".into(), false, 1)));
        assert_eq!(pick(Some("api")), Some(("claude".into(), false, 0)), "額度在裸 claude：探預設帳號的 /usage，不是只問登入");
        assert_eq!(pick(Some("cc9")), None);
    }

    /// #347: a completed Claude probe on A must not update the same-named identity after a repoint to B.
    #[tokio::test]
    async fn a_superseded_claude_probe_cannot_overwrite_the_new_hosts_identity() {
        let app = crate::testing::env().await.app.clone();
        let host = "claude-identity-347";
        let cfg = |ssh: &str| crate::config::HostCfg {
            shared_session: false,
            name: host.into(),
            ssh: ssh.into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        };
        app.hosts.insert_remote_for_test(cfg("target-a")).await;
        let old_fence = app.hosts.fence(host).await.unwrap();
        app.hosts.replace_remote_for_test(&app, cfg("target-b")).await;

        let identity = crate::tools::IdentityInfo {
            name: "cc1".into(),
            kind: "claude".into(),
            logged_in: Some(true),
            reason: None,
            account: Some("b@example.test".into()),
            plan: Some("B plan".into()),
            source: crate::tools::SOURCE_CONFIG,
            config_dir: None,
        };
        app.tools.lock().await.insert(
            host.into(),
            crate::tools::HostTools {
                tools: Default::default(),
                identities: [("cc1".into(), identity)].into_iter().collect(),
                shell_identities: vec![],
                utc_offset_secs: None,
                herdr_cli: None,
                checked_at: crate::db::now(),
            },
        );

        let stale = ProbeOutcome { logged_in: Some(false), email: Some("a@example.test".into()), plan: Some("A plan".into()), quota: None };
        assert!(!record_probe_identity(&app, host, "cc1", &stale, &old_fence).await, "舊 probe 不應有任何寫入");
        let tools = app.tools.lock().await;
        let current = &tools[host].identities["cc1"];
        assert_eq!(current.logged_in, Some(true));
        assert_eq!(current.account.as_deref(), Some("b@example.test"));
        assert_eq!(current.plan.as_deref(), Some("B plan"));
    }

    #[tokio::test]
    async fn an_unreadable_remote_home_skips_claude_probe_and_recovers_next_poll() {
        use crate::testing::MockHerdr;
        use std::sync::atomic::Ordering;

        let app = crate::testing::env().await.app.clone();
        let local_fence = app.hosts.fence(LOCAL_HOST).await.unwrap();
        let local_home = dirs::home_dir().map(|p| p.display().to_string()).unwrap_or_else(|| "/tmp".into());
        assert_eq!(crate::hosts::home_for_fence(&local_fence).await.unwrap(), local_home, "local HOME behavior stays unchanged");
        let host = format!("claude-home-595-{}", crate::db::ulid().to_ascii_lowercase());
        let conn = app
            .hosts
            .insert_remote_for_test(crate::config::HostCfg {
                shared_session: false,
                name: host.clone(),
                ssh: "unused".into(),
                ssh_port: 22,
                ssh_opts: vec![],
                herdr_session: "agents-manager".into(),
                remote_path: String::new(),
            })
            .await;
        conn.connected.store(true, Ordering::SeqCst);
        let socket = crate::hosts::short_dir(None).join(format!("{host}.sock"));
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let h = MockHerdr::start(socket.clone());
        h.set_screen(
            "*",
            &format!("{AUTH_BEGIN}\n{{\"loggedIn\":true,\"email\":\"remote@example.test\",\"subscriptionType\":\"max\"}}\n{AUTH_END}\nTotal cost: $0\n{USAGE_DONE}0\n"),
        );

        let mut env = BTreeMap::new();
        env.insert("CLAUDE_CONFIG_DIR".into(), "~/.claude-cc1".into());
        let identity_cfg = crate::config::IdentityCfg {
            name: "cc1".into(),
            kind: "claude".into(),
            host: Some(host.clone()),
            env,
            args: vec![],
        };
        let mut identity = crate::tools::IdentityInfo::shell("cc1", "claude", Some("/previous/config".into()));
        identity.logged_in = Some(true);
        identity.account = Some("previous@example.test".into());
        identity.plan = Some("previous plan".into());
        app.tools.lock().await.insert(
            host.clone(),
            crate::tools::HostTools {
                tools: [("claude".into(), crate::tools::ToolInfo { installed: true, path: Some("/usr/bin/claude".into()), version: None, logged_in: Some(true) })]
                    .into_iter()
                    .collect(),
                identities: [("cc1".into(), identity)].into_iter().collect(),
                shell_identities: vec![identity_cfg],
                utc_offset_secs: None,
                herdr_cli: None,
                checked_at: crate::db::now(),
            },
        );

        let ssh_calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let ssh_calls2 = ssh_calls.clone();
        crate::hosts::set_ssh_fake(&host, move |script| {
            ssh_calls2.lock().unwrap().push(script.to_string());
            Err(anyhow!("injected remote HOME read failure"))
        });

        let key = crate::quota::quota_key(&host, "claude:cc1");
        let err = refresh_claude(&app, &host).await.expect_err("unreadable HOME must skip this host's probe");
        assert!(err.to_string().contains("HOME"), "retain the reason for retry/logging: {err:#}");
        assert!(h.calls_to("workspace.create").is_empty(), "do not launch a probe with a daemon-local path");
        assert!(!cooling_down(&key, false, false), "HOME authority failure must not park the identity as a CLI probe failure");
        {
            let tools = app.tools.lock().await;
            let previous = &tools[&host].identities["cc1"];
            assert_eq!(previous.account.as_deref(), Some("previous@example.test"), "no login answer was published");
            assert_eq!(previous.plan.as_deref(), Some("previous plan"));
        }
        assert!(!app.quotas.lock().await.contains_key(&key), "no quota observation was published");
        assert_eq!(ssh_calls.lock().unwrap().len(), 1, "stop immediately after the failed HOME authority read");
        let forced = force_probe(&app, &host, Some("cc1")).await;
        assert!(matches!(forced, Err(ForceProbeError::Failed(ref e)) if e.contains("HOME")), "forced probes retain the HOME failure reason: {forced:?}");
        assert!(h.calls_to("workspace.create").is_empty(), "forced probing also stops before launching a pane");
        assert_eq!(ssh_calls.lock().unwrap().len(), 2, "both poll and forced refresh saw the HOME read failure");

        *conn.remote_home.lock().await = Some("/home/remote".into());
        assert!(refresh_claude(&app, &host).await.unwrap(), "the next poll retries naturally when HOME is readable");
        let creates = h.calls_to("workspace.create");
        assert!(!creates.is_empty(), "the recovered poll should start its expected identity probe");
        assert!(creates.iter().all(|params| params["cwd"] == "/home/remote"), "workspace also belongs under the remote home: {creates:?}");
        assert!(creates.iter().any(|params| params["env"]["CLAUDE_CONFIG_DIR"] == "/home/remote/.claude-cc1"), "identity env uses the remote HOME: {creates:?}");
        assert_eq!(app.tools.lock().await[&host].identities["cc1"].account.as_deref(), Some("remote@example.test"));

        drop(h);
        let _ = std::fs::remove_file(socket);
    }

    /// 沒這個身分：什麼都不探、不開 pane，直接回 `UnknownAccount`（API 404）。
    #[tokio::test]
    async fn a_forced_probe_of_an_unknown_account_opens_nothing() {
        let app = crate::testing::env().await.app.clone();
        let got = force_probe(&app, LOCAL_HOST, Some("cc9")).await;
        assert!(matches!(got, Err(ForceProbeError::UnknownAccount)), "{got:?}");
        assert!(app.quotas.lock().await.is_empty());
    }

    /// L4（review 2026-09-16）：共用預設帳號的身分（cc0）停用了、沒有 run，也沒有不帶身分的 claude bot 在跑，
    /// 裸 `claude` 就不再探測（SPEC §16.3b）。沒有任何身分指到預設帳號時照探——沒有東西可以停用。
    #[test]
    fn the_default_account_is_skipped_only_when_everything_on_it_is_disabled_and_idle() {
        let off = vec!["cc0".to_string()];
        let idle: std::collections::BTreeSet<String> = Default::default();
        let busy: std::collections::BTreeSet<String> = ["cc0".to_string()].into_iter().collect();
        let names = vec!["cc0".to_string()];
        assert!(skip_disabled_default(&off, &idle, &names, false));
        assert!(!skip_disabled_default(&off, &busy, &names, false), "cc0 還有 run 在跑");
        assert!(!skip_disabled_default(&off, &idle, &names, true), "不帶身分的 claude bot 也吃預設帳號");
        assert!(!skip_disabled_default(&[], &idle, &names, false), "沒停用");
        assert!(!skip_disabled_default(&off, &idle, &[], false), "沒有身分指到預設帳號");
        let two = vec!["cc0".to_string(), "main".to_string()];
        assert!(!skip_disabled_default(&off, &idle, &two, false), "還有一個共用預設帳號的身分沒停用");
    }

    #[test]
    fn a_statusline_beats_the_login_answer() {
        let e = ProbeEvidence { cli_says_logged_out: true, reported_statusline: true, ..Default::default() };
        assert!(should_probe_identity(e));
    }

    /// Regression: a previous `loggedIn: false` must NOT park the identity forever.
    #[test]
    fn the_login_answer_alone_never_blocks_a_probe() {
        let e = ProbeEvidence { cli_says_logged_out: true, ..Default::default() };
        assert!(should_probe_identity(e));
    }

    #[test]
    fn a_failed_probe_parks_the_identity_until_its_cooldown_expires() {
        assert!(!should_probe_identity(ProbeEvidence { cooling_down: true, ..Default::default() }));
        assert!(should_probe_identity(ProbeEvidence { cooling_down: false, ..Default::default() }));
    }

    /// m4p's `cc2`: a genuinely logged-out account costs one probe per half hour.
    #[test]
    fn backoff_is_longer_when_the_cli_also_says_logged_out() {
        let out = ProbeEvidence { cli_says_logged_out: true, ..Default::default() };
        assert_eq!(failure_backoff(out), RETRY_WHEN_LOGGED_OUT);
        assert_eq!(failure_backoff(ProbeEvidence::default()), RETRY_AFTER_FAILURE);
        assert_eq!(failure_backoff(ProbeEvidence { has_live_run: true, ..out }), RETRY_AFTER_FAILURE);
        assert_eq!(failure_backoff(ProbeEvidence { reported_statusline: true, ..out }), RETRY_AFTER_FAILURE);
    }

    #[test]
    fn parking_expires_and_a_success_clears_it() {
        let k = format!("test/claude:{}", ulid::Ulid::new());
        assert!(!cooling_down(&k, false, false));
        park(&k, false);
        assert!(cooling_down(&k, false, false));
        unpark(&k);
        assert!(!cooling_down(&k, false, false));
        park(&k, false);
        assert!(!cooling_down_at(&k, false, false, std::time::Instant::now() + RETRY_AFTER_FAILURE), "a cool-down in the past is over");
    }

    /// 重新驗證登入成功要把退避收掉。共用預設帳號的身分（`cc0`）的額度與退避都記在**裸 `claude`** 那把 key，
    /// 只收 `claude:cc0` 的話，使用者剛重新登入、那一格還要再被「沒登入」的 30 分鐘退避擋著。
    #[test]
    fn a_login_recheck_unparks_the_bare_default_account() {
        let host = format!("h-{}", ulid::Ulid::new());
        let bare = crate::quota::quota_key(&host, "claude");
        park(&bare, true);
        assert!(cooling_down(&bare, false, false));
        unpark_identity(&host, "cc0", true);
        assert!(!cooling_down(&bare, false, false), "共用預設帳號的身分重驗成功要收掉裸 key 的退避");
    }
