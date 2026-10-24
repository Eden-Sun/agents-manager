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
        assert!(regex_match("cl[a-z]ude+", "claudeee", false, false));
        assert!(regex_match("CLAUDE", "claude", true, false));
        assert!(!regex_match("CLAUDE", "claude", false, false));
        assert!(regex_match("claude", "claude", false, true));
        assert!(!regex_match("claud", "claude", false, true), "-x 要整串相等");
        assert!(regex_match("a*b", "xaab", false, false));
        // 可判斷語法以外（壞語法、`{n,m}`、`\d`、`[a-c]`…）是「判不出來」，不是「不命中」，也不退回子字串。
        for bad in ["(", "claude**", "claude{0}", "cla\\dde", "cl[a-c]ude", "[[:alpha:]]", "claude|", "a$b", "x^y", "éé"] {
            assert!(regex_match_limited(bad, "claude", false, false).is_err(), "{bad}");
        }
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

    /// agy 1.4.7：`hookSpecificOutput` 是未知欄位，protojson 會拒絕整個結果、理由也被蓋掉（實測）。agy 的決策只能是 `decision`／`reason`。
    #[test]
    fn agy_deny_is_only_decision_and_reason_so_the_reason_reaches_the_model() {
        let agy = r#"{"conversationId":"c","toolCall":{"name":"run_command","args":{"CommandLine":"pkill -f claude -P 1"}}}"#;
        let Guard::Deny(out) = guard("agy", "PreToolUse", agy) else { panic!("要擋") };
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v.get("hookSpecificOutput").is_none(), "agy 不能有 hookSpecificOutput：{out}");
        assert_eq!(v.as_object().unwrap().len(), 2, "只有 decision 與 reason：{out}");
        assert_eq!(v["decision"], "deny");
        assert!(v["reason"].as_str().unwrap().contains("lsof -ti :PORT"), "{out}");

        // 其他 provider 照舊：hookSpecificOutput 形狀。
        let claude = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"killall node"}}"#;
        let Guard::Deny(out) = guard("claude", "PreToolUse", claude) else { panic!("要擋") };
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v.get("hookSpecificOutput").is_some() && v.get("decision").is_none(), "{out}");
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

    // ───────────── review 修正（#1884 rv2）─────────────

    fn notes(cmd: &str) -> Vec<String> {
        check_command_logged(cmd).1
    }

    #[test]
    fn hostile_regexes_are_bounded_and_let_through_with_a_log() {
        let started = std::time::Instant::now();
        // 災難性回溯的寫法：NFA 線性時間，照常判斷（沒命中就放行，不需要記 log）。
        for cmd in ["pkill -f '(.*)*Z'", "pkill -f '(a*)*(b*)*(c*)*Z'", "pkill -f '(.|.)(.|.)(.|.)(.|.)(.|.)(.|.)(.|.)(.|.)Z'"] {
            let v = check_command_logged(cmd);
            assert_eq!(v.0, Verdict::Allow, "{cmd}");
        }
        // 會命中的也照擋（不會因為「太複雜」而漏掉）。
        assert!(denied("pkill -f '(.*)*claude'"));
        assert!(denied("pkill -f '(.|.)(.|.)(.|.)(.|.)(.|.)(.|.)(.|.)(.|.).*claude'"));
        // 兩萬層括號：長度上限，直接放行並記 log（不 stack overflow）。
        let deep = format!("pkill -f '{}x{}'", "(".repeat(20_000), ")".repeat(20_000));
        let v = check_command_logged(&deep);
        assert_eq!(v.0, Verdict::Allow);
        assert!(v.1.iter().any(|n| n.contains("可判斷")), "{:?}", v.1);
        // 長度在上限內、但巢狀超過 8 層：放行並記 log。
        for n in [9, 40] {
            let nested = format!("pkill -f '{}claude{}'", "(".repeat(n), ")".repeat(n));
            let v = check_command_logged(&nested);
            assert_eq!(v.0, Verdict::Allow, "{n} 層");
            assert!(v.1.iter().any(|x| x.contains("可判斷")), "{:?}", v.1);
        }
        assert!(denied("pkill -f '((((((((claude))))))))'"), "8 層還在範圍內");
        // 超過一條指令最多比對的 pattern 數：多出來的不比對、放行並記 log。
        let many = format!("pkill -f {} claude", (0..MAX_REGEX_PATTERNS).map(|i| format!("zz{i}x")).collect::<Vec<_>>().join(" "));
        let v = check_command_logged(&many);
        assert_eq!(v.0, Verdict::Allow);
        assert!(v.1.iter().any(|n| n.contains("最多比對")), "{:?}", v.1);
        assert!(denied(&format!("pkill -f claude {}", (0..MAX_REGEX_PATTERNS).map(|i| format!("zz{i}x")).collect::<Vec<_>>().join(" "))));
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "hook 不能慢：{:?}", started.elapsed());
        assert!(denied("pkill -f 'cla(u|a)de'"));
        assert!(regex_match_limited("(.*)*Z", "claude --settings /x/y/z", false, false) == Ok(false));
        assert_eq!(regex_match_limited("cla.de", "claude", false, false), Ok(true));
    }

    #[test]
    fn heredoc_bodies_comments_and_unterminated_quotes_are_not_commands() {
        // 內文只是文字（引號終止字）：不是指令。
        assert!(!denied("cat <<'EOF'\npkill -f claude\nEOF"));
        assert!(!denied("cat > notes.md <<\"EOF\"\nrun pkill -f claude then\n$(pkill -f claude)\nEOF\necho done"));
        assert!(!denied("cat <<-EOF\n\tpkill -f claude\n\tEOF\nls"));
        assert!(!denied("cat << EOF\npkill -f claude\nEOF"));
        // 註解：連 `$(…)` 都不執行。
        assert!(!denied("# $(pkill -f claude)\nls"));
        assert!(!denied("ls # `pkill -f claude`"));
        // 未閉合引號／括號：shell 自己也語法錯誤，放行並記 log。
        for cmd in ["pkill -f claude 'unterminated", "pkill -f claude \"unterminated", "echo $(pkill -f claude", "echo `pkill -f claude"] {
            let (v, n) = check_command_logged(cmd);
            assert_eq!(v, Verdict::Allow, "{cmd}");
            assert!(n.iter().any(|x| x.contains("解析失敗")), "{cmd}: {n:?}");
        }
        // 之前已經完整的行還是會跑，照擋。
        assert!(denied("pkill -f claude\necho 'unterminated"));
        assert!(!denied("echo ok\npkill -f claude 'unterminated"));
        // 但會被執行的 heredoc 還是要看：未加引號內文的 `$(…)`、餵給 shell 的 script。
        assert!(denied("cat <<EOF\n$(pkill -f claude)\nEOF"));
        assert!(denied("cat <<EOF\nx `killall node` y\nEOF"));
        assert!(denied("bash <<'EOF'\npkill -f claude\nEOF"));
        assert!(denied("sudo sh -s <<EOF\nkill -9 -1\nEOF"));
        assert!(denied("bash <<< 'pkill -f claude'"));
        assert!(!denied("bash -c 'echo hi' <<'EOF'\npkill -f claude\nEOF"), "-c 時 heredoc 是 stdin，不是 script");
        assert!(!denied("grep pkill <<< 'pkill -f claude'"));
        // heredoc 結束後同一行之後的指令、之後的行照看。
        assert!(denied("cat <<'EOF' ; pkill -f claude\nbody\nEOF"));
        assert!(denied("cat <<'EOF'\nbody\nEOF\npkill -f claude"));
        // 沒有終止行：內文讀到結尾（bash 也是）。
        assert!(!denied("cat <<'EOF'\npkill -f claude"));
    }

    #[test]
    fn owner_conditions_are_denied_including_capital_u_and_wrapped() {
        assert!(denied("pkill -U 1000"));
        assert!(denied("pkill -U eddie"));
        assert!(denied("sudo pkill -U 1000"));
        assert!(denied("env FOO=1 pkill -U 1000"));
        assert!(denied("timeout 5 pkill -U 1000"));
        assert!(denied("nohup pkill -G staff &"));
        assert!(!denied("pkill -U 1000 -f next-server"), "有窄 pattern 的不擋");
        assert!(denied("pkill -f next-server -U 1000"), "選項寫在 pattern 之後");
    }

    #[test]
    fn bsd_killall_dash_m_is_a_regex_like_dash_r() {
        assert!(denied("killall -m '.*'"));
        assert!(denied("killall -m 'cla.de'"));
        assert!(denied("killall -r '.*'"));
        assert!(denied("sudo killall -m node"));
        assert!(!denied("killall -m 'next-server'"));
        assert!(!denied("killall -m 'vite.*'"));
    }

    #[test]
    fn substitutions_inside_double_quotes_are_found_whatever_quotes_surround_them() {
        assert!(denied(r#"echo "'$(pkill -f claude)'""#), "雙引號裡的單引號只是字元，`$(…)` 照樣執行");
        assert!(denied(r#"echo "a 'b' $(pkill -f claude) 'c'""#));
        assert!(denied(r#"echo "$(echo "$(pkill -f claude)")""#), "巢狀");
        assert!(denied(r#"x="$(kill -9 -1)""#));
        assert!(!denied(r#"echo '$(pkill -f claude)'"#), "單引號裡不展開");
        assert!(!denied(r#"echo "\$(pkill -f claude)""#), "跳脫的 `$` 不展開");
        assert!(!denied(r#"echo "$(echo ')')""#), "引號裡的 `)` 不結束 `$(`");
        assert!(!denied("kill $(cat /tmp/pid)"));
        assert!(!denied("pkill -f $(cat /tmp/name)"), "動態 pattern 判不出來，放行");
    }

    #[test]
    fn env_dash_s_split_string_is_unwrapped() {
        assert!(denied(r#"env -S "pkill -f claude""#));
        assert!(denied(r#"env -S 'FOO=1 pkill -f claude'"#));
        assert!(denied(r#"env -iS "pkill -f claude""#));
        assert!(denied(r#"env -S"pkill -f claude""#));
        assert!(denied(r#"env --split-string="pkill -f claude""#));
        assert!(denied(r#"env --split-string 'killall node'"#));
        assert!(denied(r#"sudo env -S "kill -9 -1""#));
        assert!(denied(r#"env -S "env -S 'pkill -f claude'""#));
        assert!(!denied(r#"env -S "pkill -f next-server""#));
        assert!(!denied(r#"env -S "echo pkill -f claude""#));
        assert!(!denied("env -u FOO pkill -f next-server"));
        assert!(denied("env -u FOO pkill -f claude"));
    }

    fn logged(provider: &str, event: &str, text: &str) -> (Guard, Vec<String>) {
        let mut lines: Vec<String> = Vec::new();
        let g = guard_logged(provider, event, text, &mut |m| lines.push(m.to_string()));
        (g, lines)
    }

    #[test]
    fn unreadable_payloads_are_let_through_and_logged() {
        // agy：事件名在參數裡，壞 JSON 也是 PreToolUse。
        let (g, l) = logged("agy", "PreToolUse", "not json");
        assert_eq!(g, Guard::Pass);
        assert!(l.len() == 1 && l[0].contains("JSON"), "{l:?}");
        // 看得出是 PreToolUse、但被截斷的 JSON：交回事件流程，但留一行。
        let (g, l) = logged("claude", "", r#"{"hook_event_name":"PreToolUse","tool_input":{"command":"pk"#);
        assert_eq!(g, Guard::NotPreToolUse);
        assert!(l.len() == 1 && l[0].contains("PreToolUse"), "{l:?}");
        // 其他事件的壞 payload 是 daemon 的事，不吵。
        let (g, l) = logged("claude", "", "not json");
        assert_eq!(g, Guard::NotPreToolUse);
        assert!(l.is_empty(), "{l:?}");
        // 判不出來的指令：放行，原因進 log。
        let unterminated = r#"{"hook_event_name":"PreToolUse","tool_input":{"command":"pkill -f claude 'oops"}}"#;
        let (g, l) = logged("claude", "", unterminated);
        assert_eq!(g, Guard::Pass);
        assert!(l.iter().any(|x| x.contains("解析失敗")), "{l:?}");
        // 乾淨的放行不寫 log。
        let ok = r#"{"hook_event_name":"PreToolUse","tool_input":{"command":"pkill -f next-server"}}"#;
        let (g, l) = logged("claude", "", ok);
        assert_eq!(g, Guard::Pass);
        assert!(l.is_empty(), "{l:?}");
        assert!(notes("pkill -f next-server").is_empty());
    }

    #[test]
    fn env_dash_s_expansion_is_bounded() {
        let started = std::time::Instant::now();
        // 一長串 `env -S env -S …`：每層都要複製剩餘參數，不能變成二次方；超限放行並記 log。
        let flat = format!("{}echo hello", "env -S ".repeat(6000));
        let (v, n) = check_command_logged(&flat);
        assert_eq!(v, Verdict::Allow);
        assert!(n.iter().any(|x| x.contains("env -S") && x.contains("上限")), "{n:?}");
        // 同一串放進 `-S` 的值裡（一層引號、內容是長鏈）。
        let quoted = format!("env -S '{}echo hello'", "env -S ".repeat(6000));
        let (v, n) = check_command_logged(&quoted);
        assert_eq!(v, Verdict::Allow);
        assert!(n.iter().any(|x| x.contains("env -S")), "{n:?}");
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "{:?}", started.elapsed());
        // 正常深度（含多層）照擋。
        assert!(denied(r#"env -S "env -S 'pkill -f claude'""#));
    }

    #[test]
    fn shells_that_only_check_syntax_never_run_their_script() {
        assert!(!denied("bash -n <<'EOF'\npkill -f claude\nEOF"));
        assert!(!denied("bash -nv <<'EOF'\npkill -f claude\nEOF"));
        assert!(!denied("sh -n <<< 'pkill -f claude'"));
        assert!(!denied("bash -n -c 'pkill -f claude'"));
        assert!(!denied("sudo bash -vn -c 'pkill -f claude'"));
        // 不是 -n 的照擋；`-c` 的字串裡出現 n 不算。
        assert!(denied("bash -c 'pkill -f claude'"));
        assert!(denied("bash -lc 'pkill -f claude'"));
        assert!(denied("bash -x <<'EOF'\npkill -f claude\nEOF"));
        // 後面的 `+n`／`+o noexec` 會再打開執行：照選項順序算最後的狀態。
        assert!(denied("bash -n +n -c 'pkill -f claude'"));
        assert!(denied("bash -n +n <<'EOF'\npkill -f claude\nEOF"));
        assert!(denied("bash -o noexec +o noexec -c 'pkill -f claude'"));
        assert!(denied("sudo bash -vn +n -c 'pkill -f claude'"));
        assert!(!denied("bash +n -n -c 'pkill -f claude'"));
        assert!(!denied("bash -o noexec -c 'pkill -f claude'"));
        assert!(!denied("bash -nc 'pkill -f claude'"));
        assert!(!denied("bash -n +x <<'EOF'\npkill -f claude\nEOF"));
        assert!(denied("bash -n -o noexec +o xtrace -c 'pkill -f claude'"), "noexec 以外的 -o／+o 值一律取消快速放行");
        // 帶值的選項（`-O`／`+O`、`-o`／`+o`、`--rcfile`）要把值一起吃掉，不能在值那裡停下、留著前面的 `-n`。
        assert!(denied("bash -n -O extglob +n -c 'kill -9 -1'"));
        assert!(denied("bash -n -O extglob +n -c 'pkill -f claude'"));
        assert!(denied("bash -n +O extglob +n -c 'pkill -f claude'"));
        assert!(denied("bash -n -o errexit +n -c 'pkill -f claude'"));
        assert!(denied("bash -n --rcfile /tmp/x +n -c 'pkill -f claude'"));
        assert!(denied("bash -n -O extglob +o noexec -c 'pkill -f claude'"));
        assert!(denied("bash -no noexec +n -c 'pkill -f claude'"));
        assert!(denied("bash -nO extglob +n <<'EOF'\npkill -f claude\nEOF"));
        assert!(!denied("bash -O extglob -n -c 'pkill -f claude'"));
        assert!(!denied("bash -n -O extglob -c 'pkill -f claude'"));
        assert!(!denied("bash -n -O extglob -O globstar -c 'pkill -f claude'"));
        assert!(!denied("bash -nO extglob -c 'pkill -f claude'"));
        assert!(!denied("sh -n <<'EOF'\npkill -f claude\nEOF"));
        // `-o`／`+o` 的值只有 noexec（及其等價寫法）才影響狀態；zsh 的 `-o exec` 會重新啟用執行，其他值一律取消快速放行。
        assert!(denied("zsh -n -o exec -c 'kill -9 -1'"));
        assert!(denied("zsh -n -o exec -c 'pkill -f claude'"));
        assert!(denied("zsh -n +o noexec -c 'pkill -f claude'"));
        assert!(denied("zsh -n +o NO_EXEC -c 'pkill -f claude'"));
        assert!(denied("zsh -n -o EXEC -c 'pkill -f claude'"));
        assert!(denied("zsh -n +o exec -c 'pkill -f claude'"));
        assert!(denied("zsh -n -o errexit -c 'pkill -f claude'"));
        assert!(denied("bash -n -o pipefail -c 'pkill -f claude'"));
        assert!(denied("bash -n -o frobnicate -c 'pkill -f claude'"));
        assert!(denied("zsh -n -O exec -c 'pkill -f claude'"));
        assert!(denied("zsh -no exec -c 'pkill -f claude'"));
        assert!(denied("zsh -n -o exec <<'EOF'\npkill -f claude\nEOF"));
        assert!(!denied("zsh -n -o noexec -c 'pkill -f claude'"));
        assert!(!denied("zsh -o NO_EXEC -c 'pkill -f claude'"));
        assert!(!denied("zsh -o no_exec <<'EOF'\npkill -f claude\nEOF"));
        assert!(!denied("zsh -n -o noexec -O extglob -c 'pkill -f claude'"));
        // 不認得的選項：判斷不了，不當 noexec 放行（照一般規則判斷）。
        assert!(denied("bash -n -Z -c 'pkill -f claude'"));
        assert!(denied("bash -n --frobnicate -c 'pkill -f claude'"));
        assert!(denied("bash -n -O extglob -Z -c 'pkill -f claude'"));
        // 位置參數／script 檔不是 stdin 的 script；`-s` 才是。
        assert!(!denied("bash script.sh <<'EOF'\npkill -f claude\nEOF"));
        assert!(!denied("bash -O extglob script.sh <<'EOF'\npkill -f claude\nEOF"));
        assert!(denied("bash -s arg <<'EOF'\npkill -f claude\nEOF"));
        assert!(denied("bash -O extglob <<'EOF'\npkill -f claude\nEOF"));
        // noexec 的 shell 只是不跑 script；外層 shell 展開的 heredoc 內文 `$(…)` 照樣會執行。
        assert!(denied("bash -n <<EOF\n$(pkill -f claude)\nEOF"));
        // `-c` 之後的字是 `$0`／參數，不是選項。
        assert!(!denied("bash -n -c 'pkill -f claude' +n"));
        assert!(denied("bash -c 'echo no' ; bash <<'EOF'\npkill -f claude\nEOF"));
    }
