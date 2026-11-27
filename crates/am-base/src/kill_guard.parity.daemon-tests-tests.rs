    use super::*;
    use serde_json::{json, Value};
    use std::io::Write as _;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    const CASES: &[&str] = &[
        "pkill -P 1",
        "pkill -u eddie",
        "pkill -U 501",
        "/usr/bin/pkill -P 1",
        "sudo pkill -P 1",
        "echo ok; pkill -P 1",
        "timeout 5 pkill -P 1",
        "echo ok && pkill -P 1 || true",
        "ls | pkill -P 1",
        "echo $(pkill -P 1)",
        "echo `pkill -P 1`",
        "command pkill -P 1",
        "exec pkill -P 1",
        "nohup pkill -P 1 &",
        "env FOO=1 pkill -P 1",
        "lsof -ti :1 | xargs pkill -P 1",
        "kill 12345",
        "kill $PID",
        "kill \"$(cat pidfile)\"",
        "lsof -ti :5173 | xargs kill",
        "echo pkill",
        "grep pkill file",
        "git commit -m \"no pkill\"",
        "agent-browser --session x close; pkill -f \"next start -p 3917\"; pkill -f \"next-server\" -P 1 2>/dev/null; true",
        "pkill -f \"next-server\" -P 1",
        "pkill next-server -P 1",
        "pkill -f next-server -9",
        "pkill -f next-server -u eddie",
        "pkill -P 1 -f next-server",
        "pkill -f -- -weird-name",
        "pkill -f 'vite --port 5173'",
        "kill -9 12345 67890",
        "kill -TERM 4242",
        "kill -1 4242",
        "kill 111 222 333",
        "sleep 9 & kill $!",
        "kill -l",
        "lsof -ti :3917 | xargs kill -9",
        "kill -9 $(pgrep -f next-server)",
        "kill $(lsof -ti :3917)",
        "echo hello && ls -la",
        "",
        "lsof -ti:3000 | xargs kill",
        "lsof -t -i:3000 | xargs kill",
        "lsof -t -i :3000 | xargs kill -9",
        "lsof -i :3000 -t | xargs kill",
        "lsof -i:3000 -t | xargs -r kill",
        "lsof -t -i tcp:3000 | xargs kill",
        "lsof -nP -t -iTCP:3000 | xargs kill",
        "lsof -tiTCP@localhost:3000 | xargs kill",
        "lsof -ti :3000 -i :4000 | xargs kill",
        "sudo lsof -ti :3000 | xargs kill",
        "lsof -ti | xargs kill",
        "lsof -t -i | xargs kill",
        "lsof -ti 3000 | xargs kill",
        "lsof -ti :abc | xargs kill",
        "lsof -ti: | xargs kill",
        "lsof -i :3000 | xargs kill",
        "lsof -ti :3000 -i | xargs kill",
        "lsof -ti :3000 -u root | xargs kill",
        "lsof -ti :3000 /tmp | xargs kill",
        "lsof -t -p 1 | xargs kill",
        "lsof -t | xargs kill",
        "lsof -t :3000 | xargs kill",
        "echo 1 | xargs kill",
        "xargs kill",
        "pgrep -f claude | xargs kill",
        "ps aux | grep next | xargs kill",
        "pgrep next | while read p; do kill $p; done",
        "kill -0 \"$pid\"",
        "kill -0 $PID",
        "kill -s 0 \"$pid\"",
        "kill -n 0 $x",
        "kill -0 -- $pid",
        "kill -0 123",
        "if kill -0 \"$pid\" 2>/dev/null; then echo up; fi",
        "kill \"$pid\"",
        "kill -9 \"$pid\"",
        "kill -s 9 $pid",
        "kill -1 $pid",
        "kill $pid -0",
        "kill -s TERM -1",
        "kill -9 -1",
        "kill -1",
        "kill -- -1",
        "kill -- -123",
        "kill -TERM 0",
        "kill -KILL -1",
        "kill -s KILL -1",
        "kill 0",
        "kill -9 0",
        "/bin/kill -9 -1",
        "sudo kill -9 -1",
        "kill %1",
        "killall node",
        "killall -9 codex",
        "killall -u eddie",
        "killall -m '.*'",
        "pgrep -f claude",
        "sudo pkill -f claude",
        "timeout 5 pkill -f claude",
        "echo \"$(pkill -f claude)\"",
        "bash -c 'pkill -f \"next-server\" -P 1'",
        "sh -lc \"cd /tmp && killall node\"",
        "eval 'pkill -f claude'",
        "FOO=1 /usr/bin/pkill -f claude",
        "true; pkill -f claude; true",
        "make test && pkill -f claude || echo failed",
        "echo 'pkill -f claude'",
        "grep pkill notes.txt",
        "# pkill -f claude\nls",
        "if true; then pkill -f claude; fi",
        "{ pkill -f claude; }",
        "(pkill -f claude)",
        "pkill -f next-server 2>&1",
        "pkill -f next-server >/dev/null 2>&1 &",
        "pkill -f next-server > /dev/null",
        "cat <<'EOF'\npkill -f claude\nEOF",
        "cat > notes.md <<\"EOF\"\nrun pkill -f claude then\n$(pkill -f claude)\nEOF\necho done",
        "cat <<-EOF\n\tpkill -f claude\n\tEOF\nls",
        "cat << EOF\npkill -f claude\nEOF",
        "# $(pkill -f claude)\nls",
        "ls # `pkill -f claude`",
        "pkill -f claude 'unterminated",
        "pkill -f claude \"unterminated",
        "echo $(pkill -f claude",
        "echo `pkill -f claude",
        "pkill -f claude\necho 'unterminated",
        "echo ok\npkill -f claude 'unterminated",
        "cat <<EOF\n$(pkill -f claude)\nEOF",
        "cat <<EOF\nx `killall node` y\nEOF",
        "bash <<'EOF'\npkill -f claude\nEOF",
        "sudo sh -s <<EOF\nkill -9 -1\nEOF",
        "bash <<< 'pkill -f claude'",
        "bash -c 'echo hi' <<'EOF'\npkill -f claude\nEOF",
        "grep pkill <<< 'pkill -f claude'",
        "cat <<'EOF' ; pkill -f claude\nbody\nEOF",
        "cat <<'EOF'\nbody\nEOF\npkill -f claude",
        "cat <<'EOF'\npkill -f claude",
        "echo \"'$(pkill -f claude)'\"",
        "echo \"a 'b' $(pkill -f claude) 'c'\"",
        "echo \"$(echo \"$(pkill -f claude)\")\"",
        "x=\"$(kill -9 -1)\"",
        "echo '$(pkill -f claude)'",
        "echo \"\\$(pkill -f claude)\"",
        "echo \"$(echo ')')\"",
        "kill $(cat /tmp/pid)",
        "env -S \"pkill -f claude\"",
        "env -S 'FOO=1 pkill -f claude'",
        "env -iS \"pkill -f claude\"",
        "env -S\"pkill -f claude\"",
        "env --split-string=\"pkill -f claude\"",
        "env --split-string 'killall node'",
        "sudo env -S \"kill -9 -1\"",
        "env -S \"env -S 'pkill -f claude'\"",
        "env -S \"echo pkill -f claude\"",
        "env -u FOO pkill -f next-server",
        "bash -n <<'EOF'\npkill -f claude\nEOF",
        "bash -nv <<'EOF'\npkill -f claude\nEOF",
        "sh -n <<< 'pkill -f claude'",
        "bash -n -c 'pkill -f claude'",
        "sudo bash -vn -c 'pkill -f claude'",
        "bash -lc 'pkill -f claude'",
        "bash -x <<'EOF'\npkill -f claude\nEOF",
        "bash -n +n -c 'pkill -f claude'",
        "bash -o noexec +o noexec -c 'pkill -f claude'",
        "bash +n -n -c 'pkill -f claude'",
        "bash -o noexec -c 'pkill -f claude'",
        "bash -nc 'pkill -f claude'",
        "bash -n -o noexec +o xtrace -c 'pkill -f claude'",
        "bash -n -O extglob +n -c 'kill -9 -1'",
        "bash -n --rcfile /tmp/x +n -c 'pkill -f claude'",
        "bash -O extglob -n -c 'pkill -f claude'",
        "bash -n -O extglob -c 'pkill -f claude'",
        "zsh -n -o exec -c 'kill -9 -1'",
        "zsh -o NO_EXEC -c 'pkill -f claude'",
        "bash -n -Z -c 'pkill -f claude'",
        "bash -n --frobnicate -c 'pkill -f claude'",
        "bash script.sh <<'EOF'\npkill -f claude\nEOF",
        "bash -s arg <<'EOF'\npkill -f claude\nEOF",
        "bash -n <<EOF\n$(pkill -f claude)\nEOF",
        "bash -n -c 'pkill -f claude' +n",
        "bash -c 'echo no' ; bash <<'EOF'\npkill -f claude\nEOF",
        "bash -c 'bash -c \"bash -c \\\"bash -c pkill\\\"\"'",
        "pkill -f '(.*)*Z'",
        "kill 4242 # '((((((",
        "printf '%s\\n' \"kill -9 -1\" | sh",
        "kill \"a\\b\"",
        "kill \"a\\\\b\"",
        "kill a\\\\\\b",
        "kill '/x\\y'",
        "kill \"\\$PID\"",
        "kill 字串",
        "pkill -f 'é'",
        "kill -9 \"$(cat \"/tmp/a b\")\"",
    ];

    fn sh_verdict(script: &str, path: Option<&Path>, provider: &str, payload: &str) -> (Option<i32>, String, String) {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script).arg("kill-guard.sh").arg(provider);
        cmd.env("AM_BOT_ID", "01TEST");
        if let Some(dir) = path {
            let old = std::env::var("PATH").unwrap_or_default();
            cmd.env("PATH", format!("{}:{old}", dir.display()));
        }
        let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("sh");
        child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        (out.status.code(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
    }

    fn sh_reason(script: &str, path: Option<&Path>, cmd: &str) -> Option<String> {
        let payload = json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": {"command": cmd, "description": "x"}}).to_string();
        let (code, out, err) = sh_verdict(script, path, "claude", &payload);
        assert_eq!(code, Some(0), "{cmd:?}: {err}");
        if out.trim().is_empty() {
            return None;
        }
        let v: Value = serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("{cmd:?}: 不是合法 JSON（{e}）：{out}"));
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny", "{out}");
        Some(v["hookSpecificOutput"]["permissionDecisionReason"].as_str().unwrap().to_string())
    }

    fn rust_reason(cmd: &str) -> Option<String> {
        match check_command(cmd) {
            Verdict::Deny(r) => Some(r),
            Verdict::Allow => None,
        }
    }

    struct AwkDir(PathBuf);
    impl Drop for AwkDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn awk_dir(tag: &str, body: &str) -> AwkDir {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = crate::testing::scratch_dir(&format!("am-kill-guard-{tag}"));
        let awk = dir.join("awk");
        std::fs::write(&awk, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&awk, std::fs::Permissions::from_mode(0o755)).unwrap();
        AwkDir(dir)
    }

    fn has(prog: &str, arg: &str) -> bool {
        Command::new(prog).arg(arg).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok()
    }

    fn variants() -> Vec<(&'static str, Option<AwkDir>)> {
        let mut out = vec![("awk", None)];
        if has("mawk", "-W version") {
            out.push(("mawk", Some(awk_dir("mawk", "exec mawk \"$@\""))));
        }
        if has("gawk", "--version") {
            out.push(("gawk --posix", Some(awk_dir("gawk", "exec gawk --posix \"$@\""))));
        }
        if has("busybox", "--help") {
            out.push(("busybox awk", Some(awk_dir("busybox", "exec busybox awk \"$@\""))));
        }
        out
    }

    #[test]
    fn the_remote_sh_guard_agrees_with_check_command_on_every_case() {
        let script = remote_kill_guard_sh();
        for (name, dir) in variants() {
            let path = dir.as_ref().map(|d| d.0.as_path());
            for cmd in CASES {
                let rust = rust_reason(cmd);
                let sh = sh_reason(&script, path, cmd);
                assert_eq!(sh, rust, "[{name}] 判定不一致：{cmd:?}");
            }
        }
    }

    #[test]
    fn the_spec_cases_from_the_2026_11_27_m4p_incident() {
        for cmd in [
            "pkill -P 1",
            "pkill -u eddie",
            "pkill -U 501",
            "/usr/bin/pkill -P 1",
            "sudo pkill -P 1",
            "echo ok; pkill -P 1",
            "timeout 5 pkill -P 1",
            "kill -9 -1",
            "kill -TERM 0",
            "kill -- -123",
            "pgrep -f x | xargs kill",
            "ps aux | grep x | xargs kill",
        ] {
            assert!(rust_reason(cmd).is_some(), "應該擋：{cmd}");
        }
        for cmd in ["kill 12345", "lsof -ti :5173 | xargs kill", "echo pkill", "grep pkill file", "git commit -m \"no pkill\""] {
            assert!(rust_reason(cmd).is_none(), "應該放行：{cmd}");
        }
        for cmd in CASES {
            assert!(CASES.iter().filter(|c| c == &cmd).count() == 1, "重複的案例：{cmd:?}");
        }
    }

    #[test]
    fn the_remote_sh_guard_reads_every_cli_payload_shape_and_answers_in_its_format() {
        let script = remote_kill_guard_sh();
        let grok = r#"{"hookEventName":"pre_tool_use","toolName":"run_terminal_command","toolInput":{"command":"killall node"}}"#;
        let (code, out, _) = sh_verdict(&script, None, "grok", grok);
        assert_eq!(code, Some(0));
        let v: Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");

        let agy = "\u{feff}{\"conversationId\":\"c\",\"toolCall\":{\"name\":\"run_command\",\"args\":{\"CommandLine\":\"kill -9 -1\"}}}";
        let (code, out, _) = sh_verdict(&script, None, "agy", agy);
        assert_eq!(code, Some(0));
        let v: Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v.as_object().unwrap().len(), 2, "agy 只有 decision 與 reason：{out}");
        assert_eq!(v["decision"], "deny");
        assert!(v["reason"].as_str().unwrap().contains("-1"));

        let agy_ok = r#"{"toolCall":{"name":"run_command","args":{"CommandLine":"kill 4242"}}}"#;
        let (code, out, _) = sh_verdict(&script, None, "agy", agy_ok);
        assert_eq!((code, out.as_str()), (Some(0), ""), "agy 放行必須是空 stdout（`{{}}` 是拒絕）");

        let escaped = json!({"tool_input": {"command": "kill \"$x\"\t\\ \u{1}"}}).to_string();
        let (_, out, _) = sh_verdict(&script, None, "claude", &escaped);
        let v: Value = serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("{e}: {out}"));
        assert_eq!(v["hookSpecificOutput"]["permissionDecisionReason"].as_str().map(str::to_string), rust_reason("kill \"$x\"\t\\ \u{1}"));

        for not_shell in [r#"{"tool_name":"Read","tool_input":{"file_path":"/etc/hosts"}}"#, "", "not json", r#"{"tool_input":{"description":"\"command\": \"pkill -P 1\""}}"#] {
            let (code, out, _) = sh_verdict(&script, None, "claude", not_shell);
            assert_eq!((code, out.as_str()), (Some(0), ""), "{not_shell}");
        }
        let (code, _, err) = sh_verdict(&script, None, "claude", r#"{"tool_input":{"command":"pkill -P 1"#);
        assert_eq!(code, Some(0), "截斷的 JSON 放行並記一行");
        assert!(err.contains("kill_guard"), "{err}");
    }

    #[test]
    fn the_remote_sh_guard_blocks_when_it_cannot_judge_and_ignores_grok_and_agy_outside_daemon_panes() {
        let script = remote_kill_guard_sh();
        let broken = awk_dir("broken", "exit 1");
        let payload = r#"{"tool_input":{"command":"kill 4242"}}"#;
        let (code, _, err) = sh_verdict(&script, Some(&broken.0), "claude", payload);
        assert_eq!(code, Some(2), "awk 失敗就擋");
        assert!(err.contains(KILL_GUARD_UNAVAILABLE_MSG), "{err}");
        for provider in ["agy", "grok"] {
            let (code, out, _) = sh_verdict(&script, Some(&broken.0), provider, payload);
            assert_eq!(code, Some(0));
            assert!(out.contains("deny") && out.contains(KILL_GUARD_UNAVAILABLE_MSG), "{provider}: {out}");
        }
        for provider in ["agy", "grok"] {
            let out = Command::new("sh")
                .arg("-c").arg(&script).arg("kill-guard.sh").arg(provider)
                .env_remove("AM_BOT_ID")
                .stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()
                .and_then(|mut c| {
                    c.stdin.take().unwrap().write_all(br#"{"tool_input":{"command":"pkill -P 1"}}"#)?;
                    c.wait_with_output()
                })
                .unwrap();
            assert_eq!((out.status.code(), out.stdout.len()), (Some(0), 0), "{provider}：不是 AG Man 的 pane 不判斷");
        }
    }

    #[test]
    fn the_remote_sh_guard_is_fast_on_big_commands() {
        let script = remote_kill_guard_sh();
        let started = std::time::Instant::now();
        let big = format!("cat > f <<'EOF'\n{}\nEOF\nkill 4242", "line of text with $(x) and `y`\n".repeat(4000));
        assert_eq!(sh_reason(&script, None, &big), None);
        let quoted = format!("git commit -m '{}'", "x".repeat(200_000));
        assert_eq!(sh_reason(&script, None, &quoted), None);
        assert!(started.elapsed() < std::time::Duration::from_secs(3), "{:?}", started.elapsed());
    }

    #[test]
    fn the_remote_sh_guard_messages_fit_inside_single_quoted_sh_and_awk_strings() {
        for m in [PKILL_DENY, KILLALL_DENY, HOW_TO, KILL_GUARD_UNAVAILABLE_MSG] {
            assert!(!m.contains(['\'', '"', '\\', '\n']), "{m}");
        }
        let script = remote_kill_guard_sh();
        assert!(!script.contains("__"), "佔位字都換掉了");
        assert!(script.starts_with("#!/bin/sh\n"));
        assert!(!script.lines().any(|l| l == "AM_KILL_GUARD_EOF"));
    }
