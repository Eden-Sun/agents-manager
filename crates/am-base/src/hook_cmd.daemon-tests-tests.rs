
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
