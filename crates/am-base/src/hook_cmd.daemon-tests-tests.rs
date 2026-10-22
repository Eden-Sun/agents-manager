
    use super::*;

    /// 隔離跑的 daemon 會把自己的資料目錄注入 pane env；spool 一定要落在那裡，
    /// 否則隔離的 daemon 不會重播、正式 daemon 反而吃到它（2026-09-14 事故）。
    #[test]
    fn the_spool_follows_the_injected_data_dir() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-hook-spool-{}", std::process::id())));
        let _ = std::fs::remove_dir_all(&dir);
        spool_to(&dir, "b1", &serde_json::json!({"event": "Stop"}));

        let line = std::fs::read_to_string(dir.join("bots/b1/hook-spool.jsonl")).unwrap();
        assert!(line.contains("\"event\":\"Stop\""), "{line}");
        let home_spool = data_dir_from(None).join("bots/b1/hook-spool.jsonl");
        assert!(!home_spool.starts_with(&dir), "沒注入時才回到預設目錄");

        assert_eq!(data_dir_from(Some("/tmp/am-iso".into())), PathBuf::from("/tmp/am-iso"));
        assert_eq!(data_dir_from(Some("".into())), data_dir_from(None), "空字串當沒設");

        // argv 贏過 env：daemon 重啟前就開著的 pane 換不掉 env，但 hook.sh 每次啟動都重寫。
        assert_eq!(resolve_data_dir("/tmp/am-iso"), PathBuf::from("/tmp/am-iso"));
        assert_eq!(resolve_data_dir("  "), data_dir_from(std::env::var_os("AM_DATA_DIR")), "沒給才看 env");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #1004：子 agent 的 pane 帶 `AM_CHILD_OF`，它繼承的 bot 身分是母 bot 的——hook 子行程看到就不送。
    #[test]
    fn a_child_pane_never_reports_as_its_parent() {
        assert!(is_child_pane(Some("proj-abc123".into())));
        assert!(!is_child_pane(None));
        assert!(!is_child_pane(Some("".into())));
        assert!(!is_child_pane(Some("  ".into())));
    }

    /// #1005：409（provider 不符）與 410（bot 已刪）是 daemon 明確的永久拒絕，重放也一樣丟，不進 spool。
    #[test]
    fn permanent_rejections_are_not_spooled() {
        assert!(!worth_spooling(409));
        assert!(!worth_spooling(410));
        assert!(worth_spooling(401));
        assert!(worth_spooling(503));
        assert!(worth_spooling(500));
        assert!(worth_spooling(413));
    }

    #[test]
    fn explicit_token_wins_over_env() {
        assert_eq!(hook_token("cli"), "cli");
    }

    #[test]
    fn empty_token_reads_env_or_stays_empty() {
        // Env-dependent, so only the empty branch is asserted deterministically.
        let from_env = std::env::var("AM_HOOK_TOKEN").unwrap_or_default();
        assert_eq!(hook_token(""), from_env);
    }

    /// issue #92：body 帶上這個行程自己的 run id，daemon 才分得出「同一個 session、不同行程」的遲到 hook。
    /// 沒有值（舊 pane、手動跑）就整個不帶，不送一個空字串進去。
    #[test]
    fn the_body_names_the_run_its_process_was_started_for() {
        let with = hook_body("b1", "claude", serde_json::json!({"a": 1}), "2026-09-18T00:00:00.000Z", false, Some("01RUN"));
        assert_eq!(with["run_id"], "01RUN");
        assert_eq!((with["bot_id"].as_str(), with["payload"]["a"].as_i64()), (Some("b1"), Some(1)));
        let parsed: crate::hookrecv::HookBody = serde_json::from_value(with).unwrap();
        assert_eq!(parsed.run_id.as_deref(), Some("01RUN"), "daemon 那一側讀得回來");
        for none in [None, Some(""), Some("  ")] {
            let without = hook_body("b1", "claude", serde_json::json!({}), "t", false, none);
            assert!(without.get("run_id").is_none(), "{none:?} → {without}");
        }
    }

    #[test]
    fn object_payload_passes_through() {
        let v = parse_payload(r#"{"a":1}"#);
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn non_object_json_becomes_raw() {
        assert_eq!(parse_payload("[1,2]")["raw"], "[1,2]");
        assert_eq!(parse_payload("not json")["raw"], "not json");
        assert_eq!(parse_payload("")["raw"], "");
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        let s = "aa\u{4f60}\u{597d}"; // 2 + 3 + 3 bytes
        assert_eq!(truncate_utf8(s, 4), "aa");
        assert_eq!(truncate_utf8(s, 5), "aa\u{4f60}");
        assert_eq!(truncate_utf8(s, 99), s);
    }

    /// agy：payload 沒有事件名、`Stop` 沒有回覆文字——hook 子行程補事件名，並從 transcript 讀最近一回合的問答放進 payload。
    #[test]
    fn an_agy_stop_gets_its_event_name_and_the_reply_read_from_the_transcript() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-hook-agy-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let t = dir.join("transcript_full.jsonl");
        std::fs::write(
            &t,
            concat!(
                r#"{"step_index":0,"type":"USER_INPUT","content":"<USER_REQUEST>\nsay OK\n</USER_REQUEST>\n<ADDITIONAL_METADATA>x</ADDITIONAL_METADATA>"}"#, "\n",
                r#"{"step_index":1,"type":"PLANNER_RESPONSE","content":"OK","input_tokens":11824}"#, "\n",
            ),
        )
        .unwrap();
        let mut p = serde_json::json!({"conversationId": "c-1", "transcriptPath": t, "fullyIdle": true});
        enrich_agy_payload(&mut p, "Stop");
        assert_eq!(p["hookEventName"], "Stop");
        assert_eq!(p["lastAssistantMessage"], "OK");
        assert_eq!(p["lastUserMessage"], "say OK");
        assert_eq!(p["lastInputTokens"], 11824, "context 大小＝最後一筆回覆讀進去的 token 數");

        // 其他事件只補事件名，不碰 transcript。
        let mut p = serde_json::json!({"conversationId": "c-1", "transcriptPath": t});
        enrich_agy_payload(&mut p, "PreInvocation");
        assert_eq!(p["hookEventName"], "PreInvocation");
        assert!(p.get("lastAssistantMessage").is_none());

        // 讀不到（路徑是占位、不是 .jsonl）：事件照樣送，只是沒有回覆文字。
        let mut p = serde_json::json!({"transcriptPath": "/no/such/transcript_full.jsonl"});
        enrich_agy_payload(&mut p, "Stop");
        assert_eq!(p["hookEventName"], "Stop");
        assert!(p.get("lastAssistantMessage").is_none());
        let mut p = serde_json::json!({"transcriptPath": "/etc/passwd"});
        enrich_agy_payload(&mut p, "Stop");
        assert!(p.get("lastAssistantMessage").is_none(), "只讀 .jsonl");
        // 非物件 payload（`{"raw": …}` 以外的怪東西）不會 panic。
        let mut p = serde_json::json!([1, 2]);
        enrich_agy_payload(&mut p, "Stop");
    }

    const MIB: u64 = 1024 * 1024;

    /// 一個全新的資料目錄（測試之間互不相干）。
    fn fresh_data_dir(name: &str) -> PathBuf {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-spool-cap-{name}-{}", std::process::id())));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 稀疏檔：總長 `len`，尾巴是 `tail`。不真的寫滿磁碟，但大小與換行都是真的。
    fn sparse(path: &std::path::Path, len: u64, tail: &[u8]) {
        let f = std::fs::File::create(path).unwrap();
        f.set_len(len - tail.len() as u64).unwrap();
        drop(f);
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(tail).unwrap();
    }

    /// #1256：一顆 bot 的 spool（`.replaying`、`.claim`、`hook-spool.jsonl`）加起來不能超過 64 MiB。
    /// 超過時從最舊的檔丟起：先 `.replaying`，再 `.claim`，最新的那個最後才碰。這裡丟掉 `.replaying` 就夠了，`.claim` 留著。
    #[test]
    fn the_spool_drops_its_oldest_file_first_when_the_total_would_pass_the_cap() {
        let data = fresh_data_dir("oldest-first");
        let bot = data.join("bots/b1");
        std::fs::create_dir_all(&bot).unwrap();
        sparse(&bot.join("hook-spool.jsonl.replaying"), 40 * MIB, b"old-a\n");
        sparse(&bot.join("hook-spool.jsonl.claim"), 30 * MIB, b"mid-b\n");
        std::fs::write(bot.join("hook-spool.jsonl"), "new-c\n").unwrap();

        spool_to(&data, "b1", &serde_json::json!({"event": "Stop", "tag": "fresh"}));

        assert!(!bot.join("hook-spool.jsonl.replaying").exists(), "最舊的先丟");
        assert!(bot.join("hook-spool.jsonl.claim").exists(), "丟到夠了就停，中間那個不動");
        let active = std::fs::read_to_string(bot.join("hook-spool.jsonl")).unwrap();
        assert!(active.contains("new-c") && active.contains("\"tag\":\"fresh\""), "最新的留著，這一行也寫進去了：{active}");
        let _ = std::fs::remove_dir_all(&data);
    }

    /// #1256：只剩最新那個檔還超過上限：從它的最前面丟（行邊界對齊），最新的幾行留著，檔案大小回到上限以下。
    #[test]
    fn when_only_the_newest_file_is_over_the_cap_its_oldest_lines_go_and_the_newest_stay() {
        let data = fresh_data_dir("newest-trim");
        let bot = data.join("bots/b1");
        std::fs::create_dir_all(&bot).unwrap();
        let tail: Vec<u8> = (0..20_000).flat_map(|i| format!("{{\"n\":{i}}}\n").into_bytes()).collect();
        sparse(&bot.join("hook-spool.jsonl"), 64 * MIB, &tail);

        spool_to(&data, "b1", &serde_json::json!({"event": "Stop", "tag": "fresh"}));

        let len = std::fs::metadata(bot.join("hook-spool.jsonl")).unwrap().len();
        assert!(len <= 64 * MIB, "寫完之後不能超過上限：{len}");
        let body = std::fs::read(bot.join("hook-spool.jsonl")).unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.ends_with("\"tag\":\"fresh\"}\n"), "新的那行一定在最後");
        assert!(text.contains("{\"n\":19999}\n"), "最新的舊行要留著");
        assert!(!text.contains("{\"n\":0}\n"), "最舊的舊行要丟掉");
        assert!(text.starts_with("{\"n\":"), "從行邊界開始，不留半行");
        let _ = std::fs::remove_dir_all(&data);
    }

    /// #1256：每次丟棄記一行 warn 到 bot 的 hook.log，丟掉的行數累加到 `hook-spool.dropped`（重啟也還在）。
    #[test]
    fn dropped_records_are_counted_cumulatively_and_logged() {
        let data = fresh_data_dir("counted");
        let bot = data.join("bots/b1");
        std::fs::create_dir_all(&bot).unwrap();
        sparse(&bot.join("hook-spool.jsonl.replaying"), 40 * MIB, b"a\nb\nc\n");
        sparse(&bot.join("hook-spool.jsonl.claim"), 30 * MIB, b"d\n");

        spool_to(&data, "b1", &serde_json::json!({"event": "Stop"}));

        let total: u64 = std::fs::read_to_string(bot.join("hook-spool.dropped")).unwrap().trim().parse().unwrap();
        assert_eq!(total, 3, "丟掉的 `.replaying` 有三行");
        let log = std::fs::read_to_string(bot.join("hook.log")).unwrap();
        assert!(log.contains("spool over cap") && log.contains("hook-spool.jsonl.replaying"), "{log}");
        let _ = std::fs::remove_dir_all(&data);
    }

    /// #1884：PreToolUse 守門「判不出來所以放行」的原因要進 bot 的 hook.log（agy 的 dispatcher 把 hook 的 stderr 丟掉，只剩這份看得到）；
    /// 乾淨的放行不寫；不 POST、不 spool。
    #[test]
    fn a_kill_guard_pass_with_a_reason_is_logged_to_hook_log_and_a_clean_one_is_not() {
        let data = fresh_data_dir("killguard-log");
        let args = |command: &str| HookArgs {
            provider: "agy".into(),
            bot: "b1".into(),
            token: "t".into(),
            port: 1, // 連不上：若真的去 POST 會留下 spool／log
            data_dir: data.to_string_lossy().into_owned(),
            event: "PreToolUse".into(),
            payload_arg: Some(serde_json::json!({"toolCall": {"args": {"CommandLine": command}}}).to_string()),
        };
        run(args("pkill -f next-server"));
        assert!(!data.join("bots/b1/hook.log").exists(), "乾淨的放行不寫 log");
        run(args("pkill -f claude 'unterminated"));
        let log = std::fs::read_to_string(data.join("bots/b1/hook.log")).unwrap();
        assert!(log.contains("kill_guard:") && log.contains("解析失敗"), "{log}");
        assert!(!data.join("bots/b1/hook-spool.jsonl").exists(), "守門不 spool");
        let _ = std::fs::remove_dir_all(&data);
    }
