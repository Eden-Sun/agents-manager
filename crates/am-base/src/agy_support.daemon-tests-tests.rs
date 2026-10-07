
    use super::*;

    #[test]
    fn trusted_workspaces_are_appended_once_and_everything_else_is_kept() {
        let existing = r#"{"colorScheme":"dark","trustedWorkspaces":["/a"],"permissions":{"allow":["command(git)"]}}"#;
        let out = trusted_workspaces_merge(existing, &["/a".into(), "/b".into()]).unwrap().expect("/b is new");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["trustedWorkspaces"], json!(["/a", "/b"]));
        assert_eq!(v["colorScheme"], "dark");
        assert_eq!(v["permissions"]["allow"], json!(["command(git)"]));
        assert_eq!(trusted_workspaces_merge(&out, &["/b".into()]).unwrap(), None, "already trusted: untouched");
        let fresh: Value = serde_json::from_str(&trusted_workspaces_merge("", &["/x".into()]).unwrap().unwrap()).unwrap();
        assert_eq!(fresh["trustedWorkspaces"], json!(["/x"]));
    }

    #[test]
    fn unreadable_or_wrong_shaped_settings_are_never_overwritten() {
        assert!(trusted_workspaces_merge("{not json", &["/a".into()]).is_err());
        assert!(trusted_workspaces_merge("[1,2]", &["/a".into()]).is_err());
        assert!(trusted_workspaces_merge(r#"{"trustedWorkspaces":"/a"}"#, &["/a".into()]).is_err());
        assert!(statusline_merge("{not json", "x").is_err());
        assert!(hooks_merge("[]", "agents-manager", "/d/agy-hook.sh").is_err());
    }

    #[test]
    fn statusline_is_ours_to_set_only_when_unset_or_already_ours() {
        let cmd = "/data/agy-hook.sh state";
        let out = statusline_merge(r#"{"colorScheme":"dark"}"#, cmd).unwrap().expect("unset: written");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["statusLine"], json!({"type": "command", "command": cmd, "stack_with_default": true}));
        assert_eq!(v["colorScheme"], "dark");
        assert_eq!(statusline_merge(&out, cmd).unwrap(), None, "idempotent");
        // 資料目錄換了：我們的舊指令對齊成新的。
        let moved = statusline_merge(&out, "/new/agy-hook.sh state").unwrap().expect("ours, stale path");
        assert!(moved.contains("/new/agy-hook.sh"), "{moved}");
        // 使用者自己的 statusLine 不碰。
        let mine = r#"{"statusLine":{"type":"command","command":"/home/me/bar.sh"}}"#;
        assert_eq!(statusline_merge(mine, cmd).unwrap(), None);
    }

    #[test]
    fn our_named_hook_is_flat_for_the_three_events_and_other_hooks_survive() {
        let existing = r#"{"other-tool":{"enabled":true,"Stop":[{"type":"command","command":"x"}]},"version":1}"#;
        let out = hooks_merge(existing, "agents-manager", "/my data/agy-hook.sh").unwrap().expect("new entry");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["other-tool"], serde_json::from_str::<Value>(existing).unwrap()["other-tool"]);
        assert_eq!(v["version"], 1);
        let ours = &v["agents-manager"];
        assert_eq!(ours["enabled"], true);
        for event in HOOK_EVENTS {
            // 扁平：陣列元素直接是 handler，不是 `{matcher, hooks:[…]}`——形狀錯了 agy 會整個丟掉這個檔。
            let h = &ours[event][0];
            assert_eq!(h["type"], "command", "{event}");
            assert_eq!(h["timeout"], 5);
            assert_eq!(h["command"], format!("'/my data/agy-hook.sh' {event}"));
            assert!(h.get("hooks").is_none() && h.get("matcher").is_none(), "{event}: flat handler");
        }
        assert_eq!(ours.as_object().unwrap().len(), 1 + HOOK_EVENTS.len(), "nothing but enabled + the three events");
        assert_eq!(hooks_merge(&out, "agents-manager", "/my data/agy-hook.sh").unwrap(), None, "idempotent");
        let repointed = hooks_merge(&out, "agents-manager", "/elsewhere/agy-hook.sh").unwrap().expect("dispatcher moved");
        assert!(repointed.contains("/elsewhere/agy-hook.sh") && !repointed.contains("/my data/"));
    }

    #[test]
    fn instances_get_their_own_hook_name() {
        assert_eq!(hook_name(None), "agents-manager");
        assert_eq!(hook_name(Some("dev")), "agents-manager-dev");
    }

    const USER: &str = r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-10-04T00:00:00Z","content":"<USER_REQUEST>\nreply with OK\n</USER_REQUEST>\n<ADDITIONAL_METADATA>cwd=/x</ADDITIONAL_METADATA>"}"#;

    #[test]
    fn user_text_strips_the_request_tags_and_the_metadata() {
        assert_eq!(user_text(USER).as_deref(), Some("reply with OK"));
        assert_eq!(user_text(r#"{"type":"RUN_COMMAND","content":"<USER_REQUEST>x</USER_REQUEST>"}"#), None, "only USER_INPUT");
        assert_eq!(user_text(r#"{"type":"USER_INPUT","content":"no tags"}"#), None);
        assert_eq!(user_text("not json"), None);
    }

    #[test]
    fn the_last_exchange_is_the_last_user_input_and_the_last_model_text_after_it() {
        let old = r#"{"step_index":1,"type":"PLANNER_RESPONSE","content":"old answer"}"#;
        let tool = r#"{"step_index":3,"type":"RUN_COMMAND","content":"ls"}"#;
        let mid = r#"{"step_index":2,"type":"PLANNER_RESPONSE","content":"looking"}"#;
        let fin = r#"{"step_index":4,"type":"PLANNER_RESPONSE","content":"  OK  "}"#;
        let text = [USER, old, USER.replace("reply with OK", "again").as_str(), mid, tool, fin, "garbage line"].join("\n");
        assert_eq!(last_exchange(&text), Exchange { user: Some("again".into()), assistant: Some("OK".into()) });
        assert_eq!(last_exchange(USER), Exchange { user: Some("reply with OK".into()), assistant: None });
        assert_eq!(last_exchange(""), Exchange::default());
    }

    #[test]
    fn model_text_is_read_from_a_string_or_a_text_field_and_tool_steps_are_skipped() {
        assert_eq!(assistant_text(r#"{"type":"CORTEX_STEP_TYPE_NOTIFY_USER","content":{"message":"done"}}"#).as_deref(), Some("done"));
        assert_eq!(assistant_text(r#"{"type":"PLANNER_RESPONSE","content":""}"#), None);
        assert_eq!(assistant_text(r#"{"type":"RUN_COMMAND","content":"cat file"}"#), None);
        assert_eq!(assistant_text(r#"{"type":"PLANNER_RESPONSE","content":[1]}"#), None);
    }

    #[test]
    fn the_tail_read_keeps_only_the_end_of_a_big_file() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-agy-tail-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("t.jsonl");
        std::fs::write(&f, format!("{}\n{USER}\n", "x".repeat(5000))).unwrap();
        let tail = read_tail(&f, 1000).unwrap();
        assert!(tail.len() <= 1000);
        assert_eq!(last_exchange(&tail).user.as_deref(), Some("reply with OK"));
        assert!(read_tail(&dir.join("missing"), 10).is_err());
    }

    fn step(i: u64, ty: &str, content: &str) -> String {
        json!({"step_index": i, "source": if ty == "USER_INPUT" { "USER_EXPLICIT" } else { "MODEL" }, "type": ty, "status": "DONE", "content": content}).to_string()
    }
    fn user_step(i: u64, text: &str) -> String {
        step(i, "USER_INPUT", &format!("<USER_REQUEST>\n{text}\n</USER_REQUEST>\n<ADDITIONAL_METADATA>x</ADDITIONAL_METADATA>"))
    }

    #[test]
    fn turns_are_one_question_one_final_answer_and_a_trailing_tool_step_means_still_running() {
        let text = [
            user_step(0, "first"),
            step(1, "PLANNER_RESPONSE", "looking"),
            step(2, "RUN_COMMAND", "ls"),
            step(3, "PLANNER_RESPONSE", "the answer"),
            user_step(4, "second"),
            step(5, "PLANNER_RESPONSE", "thinking out loud"),
            step(6, "RUN_COMMAND", "git status"),
        ]
        .join("\n");
        let turns = parse_turns(&text);
        assert_eq!(turns.len(), 2);
        assert_eq!((turns[0].step_index, turns[0].prompt.as_str(), turns[0].reply.as_deref(), turns[0].closed), (0, "first", Some("the answer"), true));
        assert_eq!((turns[1].step_index, turns[1].reply.as_deref(), turns[1].closed), (4, Some("thinking out loud"), false), "最後是工具步驟＝還在跑");
        // 最後一則就是回覆：結束了。
        let done = [user_step(0, "q"), step(1, "PLANNER_RESPONSE", "OK")].join("\n");
        assert!(parse_turns(&done)[0].closed);
        // 沒有回覆的一問（被下一問打斷）也算結束；最後一問沒回覆還沒結束。
        let interrupted = [user_step(0, "a"), user_step(1, "b")].join("\n");
        let t = parse_turns(&interrupted);
        assert_eq!((t[0].closed, t[0].reply.clone(), t[1].closed), (true, None, false));
        // 還在產生的回覆（status 不是 DONE）、壞行、第一行被截斷都不致命。
        let partial = format!("{{\"half\n{}\n{}", user_step(0, "q"), json!({"step_index":1,"type":"PLANNER_RESPONSE","status":"RUNNING","content":"half an ans"}));
        let p = parse_turns(&partial);
        assert_eq!((p.len(), p[0].reply.clone(), p[0].closed), (1, None, false));
        assert!(parse_turns("").is_empty());
    }

    #[test]
    fn the_context_size_is_the_last_replys_input_tokens() {
        let text = [
            user_step(0, "q"),
            json!({"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","content":"a","input_tokens":11824,"output_tokens":27}).to_string(),
            json!({"step_index":2,"type":"PLANNER_RESPONSE","status":"DONE","content":"b","input_tokens":20000}).to_string(),
        ]
        .join("\n");
        assert_eq!(last_input_tokens(&text), Some(20000));
        assert_eq!(last_input_tokens(&user_step(0, "q")), None);
        assert_eq!(status_json(None, None), None);
        let v: Value = serde_json::from_str(&status_json(Some("gemini-3.8-flash-medium"), Some(20000)).unwrap()).unwrap();
        assert_eq!(v["model"]["id"], "gemini-3.8-flash-medium");
        assert_eq!(v["context_window"]["total_input_tokens"], 20000);
        assert!(v["context_window"].get("used_percentage").is_none(), "視窗大小不知道：不填百分比");
    }

    #[test]
    fn an_open_conversation_is_read_off_the_agy_processs_file_descriptors() {
        assert_eq!(
            conversation_from_link("/home/u/.gemini/antigravity-cli/conversations/1dd2eb9a-b927-4407-afc2-159d15d03138.db-wal").as_deref(),
            Some("1dd2eb9a-b927-4407-afc2-159d15d03138")
        );
        for bad in ["/home/u/.gemini/antigravity-cli/conversation_summaries.db", "/tmp/conversations/x.db", "/home/u/.gemini/antigravity-cli/conversations/a b.db", "/home/u/.gemini/antigravity-cli/conversations/.db"] {
            assert_eq!(conversation_from_link(bad), None, "{bad}");
        }
        let proc_root = crate::testing::scratch_dir("am-agy-proc");
        let fd = proc_root.join("4242").join("fd");
        std::fs::create_dir_all(&fd).unwrap();
        for (n, target) in [("3", "/home/u/.gemini/antigravity-cli/conversations/c-1.db"), ("4", "/home/u/.gemini/antigravity-cli/conversations/c-1.db-wal"), ("5", "/dev/null")] {
            std::os::unix::fs::symlink(target, fd.join(n)).unwrap();
        }
        assert_eq!(open_conversations(&proc_root, &[4242, 9999]), ["c-1"], "去重、略過不是對話的 fd、沒有的 pid 不致命");
        assert_eq!(
            transcript_path(Path::new("/h"), "c-1"),
            PathBuf::from("/h/.gemini/antigravity-cli/brain/c-1/.system_generated/logs/transcript_full.jsonl")
        );
    }
