
    use crate::runners::quota_agy::*;
    use super::*;
    use serde_json::json;

    /// 新版 command.data fixture：Gemini 與 Claude/GPT 組都有 weekly 與 5h 桶；只使用 Gemini。
    fn real() -> String {
        json!({"conversation_id": "", "status": "SUCCESS", "command": {"data": {"groups": [
            {"name": "Gemini Models", "buckets": [
                {"id": "gemini-weekly", "window": "weekly", "remaining_fraction": 0.98, "reset_time": "2026-10-11T15:39:29Z"},
                {"id": "gemini-5h", "name": "Five Hour Limit Remaining", "window": "five_hour", "remaining_fraction": 0.23, "reset_time": "2026-10-05T15:00:00Z"}
            ]},
            {"name": "Claude and GPT models", "buckets": [
                {"id": "claude-gpt-weekly", "window": "weekly", "remaining_fraction": 1.0, "reset_time": "2026-10-11T15:56:55Z"},
                {"id": "claude-gpt-5h", "name": "Five Hour Limit Remaining", "window": "five_hour", "remaining_fraction": 0.67, "reset_time": "2026-10-05T15:30:00Z"}
            ]}
        ]}}})
        .to_string()
    }

    /// 本機探測的結果分類（issue #870）：逾時前已印出的 `Authentication required` 要認得出來，其他逾時／失敗不能被當成沒登入。
    #[test]
    fn a_local_probe_that_printed_the_login_wall_and_hung_is_auth_required() {
        assert_eq!(read_local_run("local", true, "Authentication required\nPlease visit the URL to log in\n", "", 40).unwrap_err(), ProbeFail::AuthRequired);
        assert_eq!(read_local_run("local", true, "", "Authentication required", 40).unwrap_err(), ProbeFail::AuthRequired, "stderr 也看");
        // 逾時而且什麼辨識得出的都沒有：只是逾時，不改登入旗標。
        match read_local_run("local", true, "starting…", "", 40).unwrap_err() {
            ProbeFail::Other { reason, message } => {
                assert_eq!(reason, "timeout");
                assert!(message.contains("did not finish within 40s") && message.contains("starting"), "{message}");
            }
            other => panic!("{other:?}"),
        }
        match read_local_run("local", true, "", "", 40).unwrap_err() {
            ProbeFail::Other { reason, .. } => assert_eq!(reason, "timeout"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_finished_local_probe_reads_usage_from_stdout_and_only_stderr_can_add_the_login_wall() {
        assert!(read_local_run("local", false, &real(), "", 40).is_ok());
        assert_eq!(read_local_run("local", false, "", "Authentication required", 40).unwrap_err(), ProbeFail::AuthRequired);
        // 讀得到額度就不看 stderr 的警告。
        assert!(read_local_run("local", false, &real(), "Authentication required (refreshing)", 40).is_ok());
        match read_local_run("local", false, "", "dial tcp: connection refused", 40).unwrap_err() {
            ProbeFail::Other { reason, .. } => assert_eq!(reason, "unreadable"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_real_output_keeps_both_gemini_windows_and_ignores_claude_gpt() {
        let got = parse_usage(&real()).expect("parsed");
        assert_eq!(got.iter().map(|(k, _)| *k).collect::<Vec<_>>(), ["agy"]);
        let (_, gemini) = &got[0];
        let week = gemini.seven_day.as_ref().unwrap();
        assert!((week.used_pct - 2.0).abs() < 1e-9, "98% remaining = 2% used");
        assert_eq!(week.resets_at.as_deref(), Some("2026-10-11T15:39:29.000Z"));
        let five = gemini.five_hour.as_ref().unwrap();
        assert!((five.used_pct - 77.0).abs() < 1e-9, "remaining_fraction .23 = 77% used");
        assert_eq!(five.resets_at.as_deref(), Some("2026-10-05T15:00:00.000Z"));
        assert!(gemini.fable.is_none());
        assert_eq!(gemini.source, "agy-usage");
    }

    #[test]
    fn the_legacy_response_format_still_populates_its_weekly_window() {
        let legacy = json!({"response": "Gemini Models\tWeekly Limit Remaining\t98%\t2026-10-11T15:39:29Z\nClaude and GPT models\tWeekly Limit Remaining\t100%\t2026-10-11T15:56:55Z\n"}).to_string();
        let got = parse_usage(&legacy).expect("legacy output remains readable");
        assert_eq!(got.len(), 1, "legacy Claude/GPT row is ignored");
        assert_eq!(got[0].1.seven_day.as_ref().unwrap().used_pct, 2.0);
        assert!(got[0].1.five_hour.is_none());
    }

    #[test]
    fn a_format_change_is_none_never_a_made_up_number() {
        for bad in [
            "",
            "not json",
            r#"{"response": 5}"#,
            r#"{"status":"SUCCESS"}"#,
            r#"{"response": "nothing useful here\n"}"#,
            r#"{"response": "Gemini Models\tWeekly Limit Remaining\tunknown\t2026-10-11T15:39:29Z"}"#,
            r#"{"response": "Gemini Models\tDaily Limit Remaining\t90%\t2026-10-11T15:39:29Z"}"#,
            r#"{"command":{"data":{"groups":[{"name":"Gemini Models","buckets":[{"id":"gemini-5h","window":"five_hour","remaining_fraction":1.4}]}]}}}"#,
            r#"{"command":{"data":{"groups":[{"name":"Claude and GPT models","buckets":[{"id":"claude-gpt-5h","window":"five_hour","remaining_fraction":0.5}]}]}}}"#,
            r#"{"response": "Some Other Models\tWeekly Limit Remaining\t90%\t2026-10-11T15:39:29Z"}"#,
        ] {
            assert!(parse_usage(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn extra_columns_a_missing_reset_and_out_of_range_percentages_are_tolerated() {
        let r = json!({"response": "Gemini Models\tWeekly Limit Remaining\t120%\nClaude and GPT models\tWeekly Limit Remaining\t40%\tnote\t2026-10-11T00:00:00+00:00\n"}).to_string();
        let got = parse_usage(&r).unwrap();
        let g = got[0].1.seven_day.as_ref().unwrap();
        assert_eq!((g.used_pct, g.resets_at.as_deref()), (0.0, None), "超過 100% 夾進範圍、沒有重置時間就是 None");
        assert_eq!(got.len(), 1, "Claude/GPT weekly row is ignored");
        // 只有一桶也算（另一桶之後再說）。
        let one = json!({"response": "Gemini Models\tWeekly Limit Remaining\t50%\t2026-10-11T00:00:00Z"}).to_string();
        assert_eq!(parse_usage(&one).unwrap().len(), 1);
    }

    #[test]
    fn the_probe_runs_in_a_throwaway_dir_without_updates_or_stdin_and_cleans_up() {
        let s = probe_script(Some("/home/u/.local/bin/agy"));
        assert!(s.contains("mktemp -d") && s.contains("rm -rf \"$d\""), "{s}");
        assert!(s.contains("AGY_CLI_DISABLE_AUTO_UPDATE=true") && s.contains("'/home/u/.local/bin/agy' -p /usage --output-format json </dev/null"), "{s}");
        assert!(probe_script(Some("/tmp/a b/agy")).contains("'/tmp/a b/agy'"), "路徑要 quote");
        assert!(probe_script(None).contains(" agy -p /usage") || probe_script(None).contains("'agy' -p /usage") || probe_script(None).contains("agy -p /usage"));
    }

    /// 真的跑一次那段 shell：用假 `agy`（輸出真機的 JSON），確認在別的目錄跑、目錄用完就刪、環境變數有帶、exit code 傳得出來。
    #[test]
    fn the_script_runs_a_fake_agy_in_a_temp_dir_that_is_gone_afterwards() {
        let dir = crate::testing::scratch_dir("am-agy-quota");
        let fake = dir.join("agy");
        let seen = dir.join("seen.txt");
        crate::testing::write_exec(
            &fake,
            format!("#!/bin/sh\npwd > {s}\necho \"$AGY_CLI_DISABLE_AUTO_UPDATE $*\" >> {s}\nprintf '%s' '{j}'\n", s = seen.display(), j = real()),
        );
        let out = crate::exec_retry::output(std::process::Command::new("/bin/sh").arg("-c").arg(probe_script(fake.to_str()))).unwrap();
        assert!(out.status.success());
        assert!(parse_usage(&String::from_utf8_lossy(&out.stdout)).is_some());
        let seen = std::fs::read_to_string(&seen).unwrap();
        let mut lines = seen.lines();
        let cwd = lines.next().unwrap();
        assert!(!cwd.starts_with(dir.to_str().unwrap()) && !std::path::Path::new(cwd).exists(), "拋棄式 cwd 用完刪掉：{cwd}");
        assert_eq!(lines.next().unwrap(), "true -p /usage --output-format json");
    }

    /// macOS 的 agy 憑證在 Keychain、沒有檔案（m4p 實測）：用假 `security`（PATH 前面）跑真的 sh，確認「有 Keychain 項目」算已登入、
    /// 登出會刪它；沒有 `security`（Linux）時只看檔案。假 HOME 底下沒有憑證檔。
    #[test]
    fn a_keychain_item_counts_as_logged_in_and_logout_deletes_it() {
        let dir = crate::testing::scratch_dir("am-agy-keychain");
        let home = dir.join("home");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let state = dir.join("item");
        crate::testing::write_exec(
            &bin.join("security"),
            format!(
                "#!/bin/sh\ncase \"$1 $2 $3 $4 $5\" in\n\"find-generic-password -s gemini -a antigravity\"*) [ -e {s} ] ;;\n\"delete-generic-password -s gemini -a antigravity\"*) rm -f {s} ;;\n*) exit 9 ;;\nesac\n",
                s = state.display()
            ),
        );
        // macOS 的 /usr/bin 有真的 `security`（會去問真的 Keychain）：沒有 security 的情境 PATH 只放一個空目錄，sh 內建的 `[`、`command` 夠用。
        let empty = dir.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let run = |script: &str, with_security: bool| {
            let path = if with_security { format!("{}:/usr/bin:/bin", bin.display()) } else { empty.display().to_string() };
            let out = crate::exec_retry::output(std::process::Command::new("/bin/sh").arg("-c").arg(script).env("HOME", &home).env("PATH", path)).unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        assert_eq!(run(REMOTE_PRESENT_SCRIPT, true), "AM_NO", "沒檔也沒 Keychain 項目");
        std::fs::write(&state, "x").unwrap();
        assert_eq!(run(REMOTE_PRESENT_SCRIPT, true), "AM_YES", "只有 Keychain 項目也算已登入");
        assert_eq!(run(REMOTE_PRESENT_SCRIPT, false), "AM_NO", "沒有 security（Linux）時不猜");
        assert_eq!(run(REMOTE_LOGOUT_SCRIPT, true), "AM_REMOVED");
        assert!(!state.exists(), "Keychain 項目被刪");
        assert_eq!(run(REMOTE_LOGOUT_SCRIPT, true), "AM_ABSENT");
        // 檔案與 Keychain 都在：兩個都清。
        let tok = home.join(TOKEN_FILE);
        std::fs::create_dir_all(tok.parent().unwrap()).unwrap();
        std::fs::write(&tok, "t").unwrap();
        std::fs::write(&state, "x").unwrap();
        assert_eq!(run(REMOTE_PRESENT_SCRIPT, false), "AM_YES", "檔案照舊算");
        assert_eq!(run(REMOTE_LOGOUT_SCRIPT, true), "AM_REMOVED");
        assert!(!tok.exists() && !state.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_host_without_agy_is_not_probed_and_not_an_error() {
        let e = crate::testing::env().await;
        let tools = |agy: bool| crate::tools::HostTools {
            tools: agy
                .then(|| ("agy".to_string(), crate::tools::ToolInfo { installed: true, path: Some("/opt/agy".into()), version: None, logged_in: None }))
                .into_iter()
                .collect(),
            identities: Default::default(),
            shell_identities: vec![],
            utc_offset_secs: None,
            herdr_cli: None,
            checked_at: crate::db::now(),
        };
        e.app.tools.lock().await.insert(LOCAL_HOST.into(), tools(false));
        assert!(!refresh_agy(&e.app, LOCAL_HOST).await.expect("no agy: Ok(false), no error"));
        assert!(e.app.quotas.lock().await.get("agy").is_none());
    }

    #[tokio::test]
    async fn gemini_five_hour_and_weekly_windows_share_one_key_and_failed_probe_keeps_the_old_reading() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        for (key, q) in parse_usage(&real()).unwrap() {
            crate::quota::set(&app, LOCAL_HOST, key, q).await;
        }
        // 兩個窗口都落在唯一的 Gemini key 上。
        let again = parse_usage(&real()).unwrap().remove(0);
        crate::quota::set(&app, LOCAL_HOST, again.0, again.1).await;
        let q = app.quotas.lock().await.clone();
        assert!(q.contains_key("agy"), "{:?}", q.keys().collect::<Vec<_>>());
        assert!(!q.contains_key("agy:claude-gpt"), "不建立舊的 Claude/GPT key");
        let before = q["agy"].seven_day.clone();
        // 探測壞掉（沒有 agy 輸出）：解析回 None，呼叫端不寫任何東西。
        assert!(parse_usage("garbage").is_none());
        assert_eq!(app.quotas.lock().await["agy"].seven_day, before);
        let snap = crate::quota::snapshot(&app).await;
        assert!(snap["kinds"]["agy"]["five_hour"]["used_pct"].is_number() && snap["kinds"]["agy"]["seven_day"]["used_pct"].is_number(), "{snap}");
        assert!(snap["kinds"].get("agy:claude-gpt").is_none(), "{snap}");
    }

    #[tokio::test]
    async fn retired_agy_claude_gpt_cache_is_purged_on_load_and_cannot_be_saved_again() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let q = quota_row("agy", &crate::db::now()).1;
        let raw = serde_json::to_string(&q).unwrap();
        for key in ["agy:claude-gpt", "agy-remote/agy:claude-gpt"] {
            sqlx::query("INSERT INTO quota_cache (key, quota_json, updated_at) VALUES (?, ?, ?)")
                .bind(key)
                .bind(&raw)
                .bind(&q.updated_at)
                .execute(&app.db)
                .await
                .unwrap();
        }

        assert_eq!(crate::quota::load_cache(&app).await.unwrap(), 0);
        assert!(!app.quotas.lock().await.contains_key("agy:claude-gpt"));
        let old_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM quota_cache WHERE key LIKE '%agy:claude-gpt'")
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(old_rows, 0, "startup purges local and remote old snapshots");

        crate::quota::set(&app, LOCAL_HOST, "agy:claude-gpt", q).await;
        assert!(!app.quotas.lock().await.contains_key("agy:claude-gpt"));
        let old_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM quota_cache WHERE key LIKE '%agy:claude-gpt'")
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(old_rows, 0, "legacy callers cannot restore it");
    }
