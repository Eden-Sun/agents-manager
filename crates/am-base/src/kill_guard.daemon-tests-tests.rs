    use super::*;

    fn denied(cmd: &str) -> bool {
        matches!(check_command(cmd), Verdict::Deny(_))
    }
    fn reason(cmd: &str) -> String {
        match check_command(cmd) {
            Verdict::Deny(r) => r,
            Verdict::Allow => panic!("應該擋：{cmd}"),
        }
    }

    #[test]
    fn the_1884_incident_command_is_denied_with_the_bsd_explanation() {
        let cmd = r#"agent-browser --session x close; pkill -f "next start -p 3917"; pkill -f "next-server" -P 1 2>/dev/null; true"#;
        let r = reason(cmd);
        assert!(r.contains("-P") && r.contains("BSD") && r.contains("lsof -ti :PORT"), "{r}");
        assert!(denied(r#"pkill -f "next-server" -P 1"#));
    }

    #[test]
    fn options_after_the_pattern_are_misordered_but_before_it_is_fine_on_both_parsers() {
        // GNU（Linux）永遠把 `-P 1` 當選項、BSD（macOS）當 pattern：寫在 pattern 之前兩邊語意才一致。
        assert!(denied("pkill next-server -P 1"));
        assert!(denied("pkill -f next-server -9"));
        assert!(denied("pkill -f next-server -u eddie"));
        assert!(!denied("pkill -P 1 -f next-server"));
        assert!(!denied("pkill -9 -f next-server"));
        assert!(!denied("pkill -f next-server"));
        assert!(!denied("pkill -f -- -weird-name"), "`--` 之後的 `-` 開頭字是 pattern，兩邊一致");
        assert!(!denied("pkill -f 'vite --port 5173'"), "引號裡的 `--port` 是 pattern 的一部分");
    }

    #[test]
    fn bsd_stops_at_the_first_non_option_but_gnu_permutes() {
        let a: Vec<String> = ["-f", "x", "-P", "1"].iter().map(|s| s.to_string()).collect();
        let bsd = parse_pkill(&a, true);
        let gnu = parse_pkill(&a, false);
        assert_eq!(bsd.patterns, ["x", "-P", "1"], "BSD：第一個非選項之後全是 pattern");
        assert_eq!(gnu.patterns, ["x"], "GNU：-P 1 是選項");
        assert_eq!(gnu.value(&["P"]), Some("1"));
    }

    #[test]
    fn explicit_pids_and_port_based_kills_pass() {
        assert!(!denied("kill 12345"));
        assert!(!denied("kill -9 12345 67890"));
        assert!(!denied("kill -TERM 4242"));
        assert!(!denied("kill -1 4242"), "對明確 pid 送 SIGHUP（reload）不是廣域");
        assert!(!denied("kill -l"));
        assert!(!denied("lsof -ti :3917 | xargs kill"));
        assert!(!denied("lsof -ti :3917 | xargs kill -9"));
        assert!(!denied("kill $(lsof -ti :3917)"));
        assert!(!denied("kill -9 $(pgrep -f next-server)"), "動態展開的 PID 判不出來，放行");
        assert!(!denied("echo hello && ls -la"));
        assert!(!denied(""));
    }

    #[test]
    fn global_kill_targets_are_denied() {
        for cmd in ["kill -9 -1", "kill -1", "kill -- -1", "kill -KILL -1", "kill -s KILL -1", "kill 0", "kill -9 0", "/bin/kill -9 -1", "sudo kill -9 -1"] {
            assert!(denied(cmd), "{cmd}");
        }
        assert!(reason("kill -9 -1").contains("-1"));
    }

    #[test]
    fn protected_agent_names_are_denied_for_pkill_and_killall() {
        assert!(denied("pkill -f claude"));
        assert!(denied("pkill claude"));
        assert!(denied("pkill -f codex"));
        assert!(denied("pkill -f 'agents-managerd serve'"));
        assert!(denied("pkill herdr"));
        assert!(denied("pkill -f 'cla.de'"), "regex 也算");
        assert!(denied("pkill -f '^claude'"));
        assert!(denied("pkill -f bots/01M"), "bot 的指示檔路徑在每個 agent 的命令列上");
        assert!(denied("killall node"));
        assert!(denied("killall claude"));
        assert!(denied("killall -9 codex"));
        assert!(denied("killall -KILL node"));
        assert!(denied("pkill -f node"), "npm 安裝的 claude／codex 行程就是 node");
        assert!(denied("pkill -x bash"));
    }

    #[test]
    fn narrow_patterns_pass() {
        assert!(!denied("pkill -f next-server"));
        assert!(!denied("pkill -f 'next start -p 3917'"));
        assert!(!denied("pkill -f 'vite --port 5173'"));
        assert!(!denied("pkill -f 'bun run dev'"));
        assert!(!denied("pkill -x vite"));
        assert!(!denied("pkill -P 4242"));
        assert!(!denied("pkill -f 'python3 -m http.server 8000'"));
        assert!(!denied("killall vite"));
        assert!(!denied("killall -s TERM next-server"));
        assert!(!denied("killall -9 chromedriver"));
        assert!(!denied("pgrep -f claude"), "pgrep 只列不殺");
    }

    #[test]
    fn too_short_or_ownerless_kills_are_denied() {
        assert!(denied("pkill -f 1"));
        assert!(denied("pkill -f ."));
        assert!(denied("pkill ''"));
        assert!(denied("pkill -u eddie"));
        assert!(denied("pkill -P 1"));
        assert!(denied("killall -u eddie"));
        assert!(!denied("pkill -x ab"), "-x 是整串相等，短也只命中同名");
    }

    #[test]
    fn it_looks_inside_wrappers_substitutions_and_shell_dash_c() {
        assert!(denied("sudo pkill -f claude"));
        assert!(denied("nohup pkill -f claude &"));
        assert!(denied("env FOO=1 pkill -f claude"));
        assert!(denied("timeout 5 pkill -f claude"));
        assert!(denied("lsof -ti :1 | xargs pkill -f claude"));
        assert!(denied("echo $(pkill -f claude)"));
        assert!(denied("echo \"$(pkill -f claude)\""));
        assert!(denied("echo `pkill -f claude`"));
        assert!(denied("bash -c 'pkill -f \"next-server\" -P 1'"));
        assert!(denied("sh -lc \"cd /tmp && killall node\""));
        assert!(denied("eval 'pkill -f claude'"));
        assert!(denied("FOO=1 /usr/bin/pkill -f claude"));
        assert!(denied("true; pkill -f claude; true"));
        assert!(denied("make test && pkill -f claude || echo failed"));
        assert!(!denied("echo 'pkill -f claude'"), "字串引數不是指令");
        assert!(!denied("grep pkill notes.txt"));
        assert!(!denied("# pkill -f claude\nls"));
        assert!(denied("if true; then pkill -f claude; fi"));
        assert!(denied("{ pkill -f claude; }"));
        // 重導向不是 pattern：`2>&1`、`> /dev/null` 不會讓窄的指令變成「太短」。
        assert!(!denied("pkill -f next-server 2>&1"));
        assert!(!denied("pkill -f next-server >/dev/null 2>&1 &"));
        assert!(!denied("pkill -f next-server > /dev/null"));
        assert!(denied("pkill -f claude 2>/dev/null"));
    }

    #[test]
    fn the_mini_regex_agrees_with_ere_on_what_matters() {
        assert!(regex_match("cla.de", "claude --x", false, false));
        assert!(regex_match("^claude", "claude --x", false, false));
        assert!(!regex_match("^claude", "node claude", false, false));
        assert!(regex_match("(codex|grok)$", "grok", false, false));
        assert!(regex_match("cl[a-c]ude+", "claudeee", false, false));
        assert!(regex_match("CLAUDE", "claude", true, false));
        assert!(!regex_match("CLAUDE", "claude", false, false));
        assert!(regex_match("claude", "claude", false, true));
        assert!(!regex_match("claud", "claude", false, true), "-x 要整串相等");
        assert!(regex_match("a*b", "xaab", false, false));
        assert!(regex_match("(", "a(b", false, false), "壞掉的 pattern 退回子字串");
    }

    fn deny_json(provider: &str, event: &str, text: &str) -> Option<Value> {
        match guard(provider, event, text) {
            Guard::Deny(out) => Some(serde_json::from_str(&out).unwrap()),
            _ => None,
        }
    }

    #[test]
    fn hook_payloads_for_each_cli_shape_are_read() {
        let claude = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"pkill -f claude"}}"#;
        let v = deny_json("claude", "", claude).expect("擋");
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert!(v["hookSpecificOutput"]["permissionDecisionReason"].as_str().unwrap().contains("lsof -ti :PORT"));
        assert!(v.get("decision").is_none());

        let grok = r#"{"hookEventName":"pre_tool_use","toolName":"run_terminal_command","toolInput":{"command":"killall node"}}"#;
        assert!(deny_json("grok", "", grok).is_some());

        // agy 的 payload 沒有事件名，事件名由 dispatcher 參數帶；shell 指令欄位叫 CommandLine。
        let agy = r#"{"conversationId":"c","toolCall":{"name":"run_command","args":{"CommandLine":"kill -9 -1"}}}"#;
        let v = deny_json("agy", "PreToolUse", agy).expect("擋");
        assert_eq!(v["decision"], "deny");
        assert!(v["reason"].as_str().unwrap().contains("-1"));

        let ok = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"lsof -ti :3917 | xargs kill"}}"#;
        assert_eq!(guard("claude", "", ok), Guard::Pass);
    }

    #[test]
    fn anything_unreadable_or_not_a_shell_call_is_let_through() {
        assert_eq!(guard("claude", "", ""), Guard::NotPreToolUse);
        assert_eq!(guard("claude", "", "not json"), Guard::NotPreToolUse);
        assert_eq!(guard("agy", "PreToolUse", "not json"), Guard::Pass, "agy 的事件名在參數裡：讀不懂也是 PreToolUse，放行");
        assert_eq!(guard("claude", "", "[1,2]"), Guard::NotPreToolUse);
        // 不是 PreToolUse（同樣的 payload 在 PostToolUse／Stop 是 daemon 的事件，不能被攔、也不能被吞）。
        let post = r#"{"hook_event_name":"PostToolUse","tool_name":"Bash","tool_input":{"command":"pkill -f claude"}}"#;
        assert_eq!(guard("claude", "", post), Guard::NotPreToolUse);
        let stop = r#"{"hook_event_name":"Stop","last_assistant_message":"PreToolUse hook says pkill -f claude"}"#;
        assert_eq!(guard("claude", "", stop), Guard::NotPreToolUse);
        // 沒有 shell 指令的工具（Read／Edit）。
        let read = r#"{"hook_event_name":"PreToolUse","tool_name":"Read","tool_input":{"file_path":"/etc/hosts"}}"#;
        assert_eq!(guard("claude", "", read), Guard::Pass);
        // 巨大、亂引號的指令也不 panic。
        let nasty = format!(r#"{{"hook_event_name":"PreToolUse","tool_input":{{"command":{}}}}}"#, serde_json::to_string(&format!("echo '{}\"$(`", "x".repeat(100_000))).unwrap());
        assert_eq!(guard("claude", "", &nasty), Guard::Pass);
    }
