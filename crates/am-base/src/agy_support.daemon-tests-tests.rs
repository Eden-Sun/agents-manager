
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
        let moved = statusline_merge(&out, "/new/agy-hook.sh state").unwrap().expect("ours, stale path");
        assert!(moved.contains("/new/agy-hook.sh"), "{moved}");
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

    /// #1884：本機版多訂 `PreToolUse`，而且是**包 `{matcher, hooks}`** 的形狀（扁平的 agy 不認、整個檔會被丟掉）；遠端版沒有。
    #[test]
    fn the_local_hook_also_subscribes_to_a_wrapped_pre_tool_use_the_remote_one_does_not() {
        let out = hooks_merge_guarded("{}", "agents-manager", "/d/agy-hook.sh").unwrap().expect("new entry");
        let v: Value = serde_json::from_str(&out).unwrap();
        let g = &v["agents-manager"]["PreToolUse"][0];
        assert_eq!(g["matcher"], "*");
        assert_eq!(g["hooks"][0]["type"], "command");
        assert_eq!(g["hooks"][0]["command"], "'/d/agy-hook.sh' PreToolUse");
        assert_eq!(g["hooks"][0]["timeout"], 5);
        assert!(g.get("command").is_none(), "不是扁平 handler");
        assert_eq!(hooks_merge_guarded(&out, "agents-manager", "/d/agy-hook.sh").unwrap(), None, "idempotent");
        let old = hooks_merge("{}", "agents-manager", "/d/agy-hook.sh").unwrap().unwrap();
        assert!(!old.contains("PreToolUse"));
        assert!(hooks_merge_guarded(&old, "agents-manager", "/d/agy-hook.sh").unwrap().expect("upgraded").contains("PreToolUse"));
    }

    /// 2026-11-27 m4p 事故：遠端版的 `PreToolUse` 直接指向 `kill-guard.sh agy`，不經 dispatcher（它每個事件都回 `{}`，1.4.7 當拒絕）。
    #[test]
    fn the_remote_hook_subscribes_pre_tool_use_to_the_sh_kill_guard() {
        let guard = "'/h/.config/agents-manager/kill-guard.sh' agy";
        let out = hooks_merge_remote("{}", "agents-manager", "/h/.config/agents-manager/agy-hook.sh", guard).unwrap().expect("new entry");
        let v: Value = serde_json::from_str(&out).unwrap();
        let g = &v["agents-manager"]["PreToolUse"][0];
        assert_eq!(g["matcher"], "*");
        assert_eq!(g["hooks"][0]["command"], guard);
        assert_eq!(g["hooks"][0]["timeout"], 5);
        assert_eq!(v["agents-manager"]["Stop"][0]["command"], "'/h/.config/agents-manager/agy-hook.sh' Stop");
        assert_eq!(hooks_merge_remote(&out, "agents-manager", "/h/.config/agents-manager/agy-hook.sh", guard).unwrap(), None, "idempotent");
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
        let done = [user_step(0, "q"), step(1, "PLANNER_RESPONSE", "OK")].join("\n");
        assert!(parse_turns(&done)[0].closed);
        let interrupted = [user_step(0, "a"), user_step(1, "b")].join("\n");
        let t = parse_turns(&interrupted);
        assert_eq!((t[0].closed, t[0].reply.clone(), t[1].closed), (true, None, false));
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
        let cached = [
            json!({"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","content":"a","input_tokens":3000,"cache_read_tokens":100000}).to_string(),
            json!({"step_index":2,"type":"PLANNER_RESPONSE","status":"DONE","content":"b","input_tokens":3415,"cache_read_tokens":140185}).to_string(),
        ]
        .join("\n");
        assert_eq!(last_input_tokens(&cached), Some(143_600));
        let no_cache_field = json!({"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","content":"a","input_tokens":5000}).to_string();
        assert_eq!(last_input_tokens(&no_cache_field), Some(5000));
        let only_cache = json!({"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","content":"a","cache_read_tokens":7000}).to_string();
        assert_eq!(last_input_tokens(&only_cache), Some(7000));
        assert_eq!(status_json(None, None, 400_000), None);
        let v: Value = serde_json::from_str(&status_json(Some("gemini-3.8-flash-medium"), Some(20000), 400_000).unwrap()).unwrap();
        assert_eq!(v["model"]["id"], "gemini-3.8-flash-medium");
        assert_eq!(v["context_window"]["total_input_tokens"], 20000);
        assert_eq!(v["context_window"]["context_window_size"], 400_000);
        assert_eq!(v["context_window"]["used_percentage"], 5.0);
    }

    #[test]
    fn agy_status_json_percentage_uses_the_configured_window_and_caps_at_100() {
        let pct = |tokens: i64, window: u64| -> Value {
            let v: Value = serde_json::from_str(&status_json(None, Some(tokens), window).unwrap()).unwrap();
            v["context_window"].clone()
        };
        assert_eq!(pct(20_000, 1_048_576)["used_percentage"], 1.9);
        assert_eq!(pct(20_000, 400_000)["used_percentage"], 5.0);
        let one_m = pct(20_000, 1_000_000);
        assert_eq!(one_m["context_window_size"], 1_000_000);
        assert_eq!(one_m["used_percentage"], 2.0);
        assert_eq!(pct(1, 3)["used_percentage"], 33.3);
        assert_eq!(pct(500_000, 400_000)["used_percentage"], 100.0);
        assert_eq!(pct(400_000, 400_000)["used_percentage"], 100.0);
        assert_eq!(pct(0, 400_000)["used_percentage"], 0.0);
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

    #[test]
    fn files_with_utf8_bom_are_supported() {
        let bom_settings = "\u{feff}{\"colorScheme\":\"dark\"}";
        let out = statusline_merge(bom_settings, "/d/agy-hook.sh state").unwrap().unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["colorScheme"], "dark");

        let bom_hooks = "\u{feff}{\"other\":{\"enabled\":true}}";
        let out = hooks_merge(bom_hooks, "agents-manager", "/d/agy-hook.sh").unwrap().unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["other"]["enabled"], true);
    
    #[test]
    fn last_assistant_time_is_the_newest_planner_reply() {
        let t = concat!(
            r#"{"type":"PLANNER_RESPONSE","created_at":"2026-11-20T01:00:00Z","content":"first"}"#, "\n",
            r#"{"type":"USER_INPUT","created_at":"2026-11-20T01:05:00Z","content":"hi"}"#, "\n",
            r#"{"type":"PLANNER_RESPONSE","created_at":"2026-11-20T01:06:00Z","content":"second"}"#, "\n",
            r#"{"type":"TOOL_RESULT","created_at":"2026-11-20T01:07:00Z","content":"x"}"#,
        );
        assert_eq!(last_assistant_at(t).as_deref(), Some("2026-11-20T01:06:00Z"), "工具輸出不算回覆");
        assert_eq!(last_assistant_at(r#"{"type":"USER_INPUT","created_at":"2026-11-20T01:00:00Z","content":"x"}"#), None);
    }

    #[test]
    fn official_statusline_payload_populates_context_cache_model_state_tasks_and_drops_transcript_path() {
        let payload = json!({
            "hookEventName": "state",
            "conversation_id": "conv-official-1",
            "transcript_path": "/home/u/.gemini/antigravity/brain/conv-official-1/.system_generated/logs/transcript.jsonl",
            "model": {
                "id": "gemini-4-argon",
                "display_name": "Gemini 4 Argon (Medium)",
                "effort": "medium"
            },
            "context_window": {
                "total_input_tokens": 143_600,
                "total_output_tokens": 4_200,
                "context_window_size": 1_048_576,
                "used_percentage": 13.7,
                "remaining_percentage": 86.3,
                "current_usage": {
                    "input_tokens": 3_415,
                    "output_tokens": 512,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 140_185
                }
            },
            "agent_state": "working",
            "tool_confirmation_pending": true,
            "task_count": 2,
            "pending_input_count": 1,
            "quota": {
                "gemini": {"remaining_fraction": 0.82}
            }
        });
        assert_eq!(model_slug_from_statusline(&payload).as_deref(), Some("gemini-4-argon-medium"));
        let orch = json!({"model": {"id": "gemini-pro-orchestrator-g1-pro", "display_name": "Orchestrator (Argon)", "effort": "xhigh"}});
        assert_eq!(model_slug_from_statusline(&orch).as_deref(), Some("gemini-pro-orchestrator-g1-pro"));
        let neon = json!({"model": {"id": "gemini-pro-orchestrator-neon", "effort": "xhigh"}});
        assert_eq!(model_slug_from_statusline(&neon).as_deref(), Some("gemini-pro-orchestrator-neon"));
        let display_flash = json!({"model": {"id": "Gemini 3.8 Flash (High)", "display_name": "Gemini 3.8 Flash (High)", "effort": "high"}});
        assert_eq!(model_slug_from_statusline(&display_flash).as_deref(), Some("gemini-3.8-flash-high"));
        let display_orch_argon = json!({"model": {"id": "Orchestrator (Argon)", "display_name": "Orchestrator (Argon)"}});
        assert_eq!(model_slug_from_statusline(&display_orch_argon).as_deref(), Some("gemini-pro-orchestrator-g1-pro"));
        let display_orch_neon = json!({"model": {"id": "Orchestrator (Neon)", "display_name": "Orchestrator (Neon)"}});
        assert_eq!(model_slug_from_statusline(&display_orch_neon).as_deref(), Some("gemini-pro-orchestrator-neon"));
        let raw = status_json_from_statusline(&payload, None, Some("gemini-3.8-flash-medium"), 400_000).expect("status_json");
        assert!(has_statusline(Some(&raw)));
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["model"]["id"], "gemini-4-argon-medium");
        assert_eq!(v["model"]["display_name"], "Gemini 4 Argon (Medium)");
        assert_eq!(v["model"]["effort"], "medium");
        assert_eq!(v["effort"]["level"], "medium");
        assert_eq!(v["context_window"]["context_window_size"], 1_048_576);
        assert_eq!(v["context_window"]["total_input_tokens"], 143_600);
        assert_eq!(v["context_window"]["total_output_tokens"], 4_200);
        assert_eq!(v["context_window"]["used_percentage"], 13.7);
        assert_eq!(v["context_window"]["remaining_percentage"], 86.3);
        assert_eq!(v["context_window"]["current_usage"]["input_tokens"], 3_415);
        assert_eq!(v["context_window"]["current_usage"]["cache_read_input_tokens"], 140_185);
        assert_eq!(v["agent_state"], "working");
        assert_eq!(v["tool_confirmation_pending"], true);
        assert_eq!(v["task_count"], 2);
        assert_eq!(v["pending_input_count"], 1);
        assert_eq!(v["quota"]["gemini"]["remaining_fraction"], 0.82);
        assert!(v.get("transcript_path").is_none() && v.get("transcriptPath").is_none());
    }

    #[test]
    fn the_compaction_line_gives_before_and_after_in_tokens() {
        assert_eq!(parse_compaction_line("440 steps summarized · context 404.9k → ~23.0k (3.3k summary)"), Some((404_900, 23_000)));
        assert_eq!(parse_compaction_line("Conversation compacted\n12 steps summarized · context 19.7k → ~22.5k (1k summary)"), Some((19_700, 22_500)));
        assert_eq!(parse_compaction_line("context 1.2M → ~80k"), Some((1_200_000, 80_000)));
    }

    #[test]
    fn a_malformed_or_missing_compaction_line_changes_nothing() {
        assert_eq!(parse_compaction_line("context 404.9k → 23.0k"), None, "no tilde");
        assert_eq!(parse_compaction_line("context abc → ~1k"), None);
        assert_eq!(parse_compaction_line("context 404.9x → ~23k"), None, "unknown unit");
        assert_eq!(parse_compaction_line("plain screen with no compaction"), None);
    }

    #[test]
    fn a_compaction_sets_the_reading_once_and_remembers_the_stale_one() {
        let run = "run-compact-once";
        let before = serde_json::json!({
            "_agm_statusline": true,
            "context_window": { "total_input_tokens": 403_489, "context_window_size": 1_048_576, "used_percentage": 38.5 },
            "model": { "id": "gemini-3.8-flash-medium" }
        })
        .to_string();
        let screen = "Conversation compacted\n440 steps summarized · context 404.9k → ~23.0k (3.3k summary)";
        let next = compaction_update(run, Some(&before), screen, 1_048_576).expect("compaction applies");
        let v: Value = serde_json::from_str(&next).unwrap();
        assert_eq!(v["context_window"]["total_input_tokens"], 23_000);
        assert_eq!(v["context_window"]["context_window_size"], 1_048_576);
        assert_eq!(v["model"]["id"], "gemini-3.8-flash-medium", "other fields kept");
        assert_eq!(pre_compact_tokens(run), Some(404_900));
        assert_eq!(compaction_update(run, Some(&next), screen, 1_048_576), None, "the same line is not applied twice");
        forget_pre_compact(run);
        assert_eq!(pre_compact_tokens(run), None);
    }

    #[test]
    fn a_stale_statusline_reading_is_held_until_the_number_changes() {
        let run = "run-compact-hold";
        let prev = serde_json::json!({"context_window": {"total_input_tokens": 23_000, "context_window_size": 1_048_576, "used_percentage": 2.2}}).to_string();
        let stale = serde_json::json!({"_agm_statusline": true, "context_window": {"total_input_tokens": 403_489, "context_window_size": 1_048_576, "used_percentage": 38.5}}).to_string();
        let held = stale_held_context(&stale, Some(&prev), 404_900).expect("stale reading is held");
        let v: Value = serde_json::from_str(&held).unwrap();
        assert_eq!(v["context_window"]["total_input_tokens"], 23_000, "held at the compacted reading");
        let fresh = serde_json::json!({"_agm_statusline": true, "context_window": {"total_input_tokens": 23_500, "context_window_size": 1_048_576, "used_percentage": 2.2}}).to_string();
        assert_eq!(stale_held_context(&fresh, Some(&prev), 404_900), None, "a changed reading is written as usual");
        assert_eq!(pre_compact_tokens(run), None);
    }
}
