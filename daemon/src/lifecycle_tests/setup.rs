#![allow(unused_imports)]
//! #1862 L2-1：從 am-lifecycle 搬來、需要真 daemon App／DB 的測試。

mod remote_grok_diagnostic_tests {
    use crate::lifecycle::setup::test_seams::*;
    use crate::lifecycle::start::test_seams::*;
    use crate::lifecycle_tests::prelude::*;
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum Code {
        RootSymlink,
        FileTypeOrSymlink,
        FdOpen,
        GnuPathStat,
        GnuFdStat,
        BsdPathStat,
        BsdFdStat,
        PathFdIdentity,
        NlinkOrSize,
        FinalSymlinkRecheck,
        ReadReturn,
        InstallUntrusted,
        InstallChanged,
    }
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum StdoutKind {
        Empty,
        File,
        Missing,
        Untrusted,
        Changed,
        Installed,
        Other,
    }
    pub(super) type Handle = crate::lifecycle::diag_support::Recorder<Code, StdoutKind>;
    use crate::lifecycle::diag_support::RECORD_LIMIT;
    fn stdout_kind(stdout: &[u8]) -> StdoutKind {
        let first = stdout.split(|byte| *byte == b'\n').next().unwrap_or_default();
        match first {
            b"AM_GROK_FILE" => StdoutKind::File,
            b"AM_GROK_MISSING" => StdoutKind::Missing,
            b"AM_GROK_UNTRUSTED" => StdoutKind::Untrusted,
            b"AM_GROK_CHANGED" => StdoutKind::Changed,
            b"AM_GROK_INSTALLED" => StdoutKind::Installed,
            b"" => StdoutKind::Empty,
            _ => StdoutKind::Other,
        }
    }
    fn code_from_marker(line: &[u8]) -> Option<Code> {
        Some(match line {
            b"AM_DIAG:G:ROOT_SYMLINK" => Code::RootSymlink,
            b"AM_DIAG:G:FILE_TYPE_OR_SYMLINK" => Code::FileTypeOrSymlink,
            b"AM_DIAG:G:FD_OPEN" => Code::FdOpen,
            b"AM_DIAG:G:GNU_PATH_STAT" => Code::GnuPathStat,
            b"AM_DIAG:G:GNU_FD_STAT" => Code::GnuFdStat,
            b"AM_DIAG:G:BSD_PATH_STAT" => Code::BsdPathStat,
            b"AM_DIAG:G:BSD_FD_STAT" => Code::BsdFdStat,
            b"AM_DIAG:G:PATH_FD_IDENTITY" => Code::PathFdIdentity,
            b"AM_DIAG:G:NLINK_OR_SIZE" => Code::NlinkOrSize,
            b"AM_DIAG:G:FINAL_SYMLINK_RECHECK" => Code::FinalSymlinkRecheck,
            b"AM_DIAG:G:READ_RETURN" => Code::ReadReturn,
            b"AM_DIAG:G:INSTALL_UNTRUSTED" => Code::InstallUntrusted,
            b"AM_DIAG:G:INSTALL_CHANGED" => Code::InstallChanged,
            _ => return None,
        })
    }
    #[derive(Debug, Clone)]
    pub(super) struct Run {
        pub(super) stdout: Vec<u8>,
        pub(super) status: Option<i32>,
    }
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum Site {
        ReadReturn,
        RootSymlink,
        FileTypeOrSymlink,
        FdOpen,
        GnuPathStat,
        GnuFdStat,
        BsdPathStat,
        BsdFdStat,
        PathFdIdentity,
        NlinkOrSize,
        FinalSymlinkRecheck,
        InstallUntrusted,
        InstallChanged,
    }
    pub(super) type InstrumentError = crate::lifecycle::diag_support::InstrumentError<Site>;
    use crate::lifecycle::diag_support::replace_count;
    fn instrument_read(mut script: String) -> Result<String, InstrumentError> {
        let guard_count = script.matches("am_bad").count();
        if guard_count != 11 {
            return Err(InstrumentError { site: Site::ReadReturn, expected: 11, found: guard_count });
        }
        script = replace_count(
            &script,
            "am_bad() { printf 'AM_GROK_UNTRUSTED\\n'; exit 0; }",
            "am_bad() { printf 'AM_DIAG:G:READ_RETURN\\n' >&9; printf 'AM_GROK_UNTRUSTED\\n'; exit 0; }",
            1,
            Site::ReadReturn,
        )?;
        let replacements = [
            (
                "[ -L \"$G\" ] && am_bad",
                "[ -L \"$G\" ] && { printf 'AM_DIAG:G:ROOT_SYMLINK\\n' >&9; am_bad; }",
                Site::RootSymlink,
            ),
            (
                "[ ! -L \"$F\" ] && [ -f \"$F\" ] || am_bad",
                "[ ! -L \"$F\" ] && [ -f \"$F\" ] || { printf 'AM_DIAG:G:FILE_TYPE_OR_SYMLINK\\n' >&9; am_bad; }",
                Site::FileTypeOrSymlink,
            ),
            (
                "exec 3< \"$F\" || am_bad",
                "exec 3< \"$F\" || { printf 'AM_DIAG:G:FD_OPEN\\n' >&9; am_bad; }",
                Site::FdOpen,
            ),
            (
                "a=$(stat -L -c '%d %i %h %s' \"$F\") || am_bad",
                "a=$(stat -L -c '%d %i %h %s' \"$F\") || { printf 'AM_DIAG:G:GNU_PATH_STAT\\n' >&9; am_bad; }",
                Site::GnuPathStat,
            ),
            (
                "b=$(stat -L -c '%d %i %h %s' /dev/fd/3) || am_bad",
                "b=$(stat -L -c '%d %i %h %s' /dev/fd/3) || { printf 'AM_DIAG:G:GNU_FD_STAT\\n' >&9; am_bad; }",
                Site::GnuFdStat,
            ),
            (
                "a=$(stat -L -f '%d %i %l %z' \"$F\") || am_bad",
                "a=$(stat -L -f '%d %i %l %z' \"$F\") || { printf 'AM_DIAG:G:BSD_PATH_STAT\\n' >&9; am_bad; }",
                Site::BsdPathStat,
            ),
            (
                "b=$(stat -L -f '%d %i %l %z' <&3) || am_bad",
                "b=$(stat -L -f '%d %i %l %z' <&3) || { printf 'AM_DIAG:G:BSD_FD_STAT\\n' >&9; am_bad; }",
                Site::BsdFdStat,
            ),
            (
                "[ \"$a\" = \"$b\" ] || am_bad",
                "[ \"$a\" = \"$b\" ] || { printf 'AM_DIAG:G:PATH_FD_IDENTITY\\n' >&9; am_bad; }",
                Site::PathFdIdentity,
            ),
            (
                "[ \"$3\" = 1 ] && [ \"$4\" -le 262144 ] || am_bad",
                "[ \"$3\" = 1 ] && [ \"$4\" -le 262144 ] || { printf 'AM_DIAG:G:NLINK_OR_SIZE\\n' >&9; am_bad; }",
                Site::NlinkOrSize,
            ),
            (
                "[ ! -L \"$G\" ] && [ ! -L \"$F\" ] || am_bad",
                "[ ! -L \"$G\" ] && [ ! -L \"$F\" ] || { printf 'AM_DIAG:G:FINAL_SYMLINK_RECHECK\\n' >&9; am_bad; }",
                Site::FinalSymlinkRecheck,
            ),
        ];
        for (needle, replacement, site) in replacements {
            script = replace_count(&script, needle, replacement, 1, site)?;
        }
        Ok(format!("exec 9>&2\n{script}"))
    }
    fn instrument_install(script: String) -> Result<String, InstrumentError> {
        let untrusted = "printf 'AM_GROK_UNTRUSTED\\n'; exit 0;";
        let script = replace_count(
            &script,
            untrusted,
            "printf 'AM_DIAG:G:INSTALL_UNTRUSTED\\n' >&9; printf 'AM_GROK_UNTRUSTED\\n'; exit 0;",
            1,
            Site::InstallUntrusted,
        )?;
        let hash_guard_count = script.matches("NOW=$(sha256sum").count();
        let expected_changed = if hash_guard_count == 2 { 4 } else if hash_guard_count == 0 { 3 } else {
            return Err(InstrumentError { site: Site::InstallChanged, expected: 0, found: hash_guard_count });
        };
        let changed = "printf 'AM_GROK_CHANGED\\n'; exit 0;";
        let script = replace_count(
            &script,
            changed,
            "printf 'AM_DIAG:G:INSTALL_CHANGED\\n' >&9; printf 'AM_GROK_CHANGED\\n'; exit 0;",
            expected_changed,
            Site::InstallChanged,
        )?;
        Ok(format!("exec 9>&2\n{script}"))
    }
    fn instrument(script: &str) -> Result<String, InstrumentError> {
        if script.contains("am_bad() { printf 'AM_GROK_UNTRUSTED\\n'; exit 0; }") {
            instrument_read(script.to_owned())
        } else {
            instrument_install(script.to_owned())
        }
    }
    pub(super) fn run(
        script: &str,
        handle: &Handle,
        home: Option<&str>,
        path: Option<&str>,
    ) -> Result<Run, InstrumentError> {
        let copy = instrument(script)?;
        let mut command = std::process::Command::new("/bin/sh");
        command.arg("-c").arg(copy);
        if let Some(home) = home {
            command.env("HOME", home);
        }
        if let Some(path) = path {
            command.env("PATH", path);
        }
        let output = command.output().map_err(|_| InstrumentError { site: Site::ReadReturn, expected: 1, found: 0 })?;
        let status = output.status.code();
        handle.record_output(status, stdout_kind(&output.stdout), &output.stderr, code_from_marker);
        Ok(Run { stdout: output.stdout, status })
    }
    fn run_original(script: &str) -> (Vec<u8>, Option<i32>) {
        match std::process::Command::new("/bin/sh").arg("-c").arg(script).output() {
            Ok(output) => (output.stdout, output.status.code()),
            Err(_) => (Vec::new(), None),
        }
    }
    #[test]
    fn the_read_instrumenter_marks_a_real_guard_without_changing_stdout_or_exit() {
        let base = crate::testing::scratch_dir("am-grok-diag");
        let hooks_dir = base.join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let hooks = hooks_dir.join(crate::lifecycle::setup::GROK_HOOKS_FILE);
        std::fs::write(&hooks, b"{}\n").unwrap();

        let normal = remote_grok_read_script(&hooks_dir.to_string_lossy(), crate::lifecycle::setup::grok_hooks_file(None).as_str());
        let original = run_original(&normal);
        let handle = Handle::new();
        let instrumented = run(&normal, &handle, None, None).unwrap();
        assert!(original.0 == instrumented.stdout, "instrumentation changed the fixed read stdout");
        assert_eq!(original.1, instrumented.status);
        assert_eq!(stdout_kind(&instrumented.stdout), StdoutKind::File);

        std::fs::remove_file(&hooks).unwrap();
        std::fs::create_dir(&hooks).unwrap();
        let rejected = run_original(&normal);
        let handle = Handle::new();
        let instrumented = run(&normal, &handle, None, None).unwrap();
        assert!(rejected.0 == instrumented.stdout, "instrumentation changed the fixed rejected stdout");
        assert_eq!(rejected.1, instrumented.status);
        assert!(handle.contains(Code::FileTypeOrSymlink));
        assert!(handle.contains(Code::ReadReturn));
        let _ = std::fs::remove_dir_all(base);
    }
    #[tokio::test]
    async fn install_instrumenter_classifies_real_guards_without_changing_stdout_or_exit() {
        let env = crate::testing::env().await;
        let host = format!("grok-install-diag-{}", crate::db::ulid());
        let home = crate::testing::scratch_dir("am-grok-install-diag");
        let hooks_dir = home.join(".grok/hooks");
        let outside = home.join("safe-fixture-dir");
        std::fs::create_dir_all(&outside).unwrap();
        let conn = env
            .app
            .hosts
            .insert_remote_for_test(crate::config::HostCfg { name: host.clone(), ssh: "unused".into(), herdr_session: "am-test".into(), ..Default::default() })
            .await;
        *conn.remote_home.lock().await = Some(home.to_string_lossy().into_owned());
        let diag = Handle::new();
        let sink = diag.clone();
        let fake_hooks_dir = hooks_dir.clone();
        let fake_outside = outside.clone();
        crate::hosts::set_ssh_fake(&host, move |script| {
            if script.starts_with("G=") {
                return Ok("AM_GROK_MISSING\n".to_owned());
            }

            std::fs::create_dir_all(fake_hooks_dir.parent().expect("fixture parent"))?;
            if let Ok(metadata) = std::fs::symlink_metadata(&fake_hooks_dir) {
                if metadata.file_type().is_symlink() {
                    std::fs::remove_file(&fake_hooks_dir)?;
                } else {
                    std::fs::remove_dir_all(&fake_hooks_dir)?;
                }
            }
            std::os::unix::fs::symlink(&fake_outside, &fake_hooks_dir)?;
            let original_untrusted = run_original(script);
            let instrumented_untrusted = run(script, &sink, None, None)
                .map_err(|error| anyhow::Error::msg(error.summary()))?;
            assert!(original_untrusted.0 == instrumented_untrusted.stdout, "installer diagnostics changed untrusted stdout");
            assert_eq!(original_untrusted.1, instrumented_untrusted.status, "installer diagnostics changed untrusted exit");
            assert!(original_untrusted.0.as_slice() == b"AM_GROK_UNTRUSTED\n", "untrusted fixture missed its fixed branch");
            assert_eq!(instrumented_untrusted.status, Some(0));

            std::fs::remove_file(&fake_hooks_dir)?;
            std::fs::create_dir_all(&fake_hooks_dir)?;
            std::fs::write(fake_hooks_dir.join(crate::lifecycle::setup::GROK_HOOKS_FILE), b"safe fixture\n")?;
            let original_changed = run_original(script);
            let instrumented_changed = run(script, &sink, None, None)
                .map_err(|error| anyhow::Error::msg(error.summary()))?;
            assert!(original_changed.0 == instrumented_changed.stdout, "installer diagnostics changed changed-guard stdout");
            assert_eq!(original_changed.1, instrumented_changed.status, "installer diagnostics changed changed-guard exit");
            assert!(original_changed.0.as_slice() == b"AM_GROK_CHANGED\n", "changed fixture missed its fixed branch");
            assert_eq!(instrumented_changed.status, Some(0));
            assert!(sink.contains(Code::InstallUntrusted));
            assert!(sink.contains(Code::InstallChanged));
            Ok("AM_GROK_INSTALLED\n".to_owned())
        });

        crate::lifecycle::setup::test_seams::install_remote_grok_hook(&conn, &serde_json::json!({}), None)
            .await
            .unwrap_or_else(|_| panic!("test installer flow failed"));
        assert!(diag.contains(Code::InstallUntrusted), "{}", diag.summary());
        assert!(diag.contains(Code::InstallChanged), "{}", diag.summary());
        env.app.hosts.remove(&env.app, &host).await;
    }
    #[test]
    fn diagnostics_discard_unknown_stderr_bound_records_and_reject_changed_generators() {
        let handle = Handle::new();
        let sentinel = b"UNKNOWN_SENTINEL_WITH_PATH_OR_PAYLOAD";
        handle.record_output(Some(0), stdout_kind(b"AM_GROK_UNTRUSTED\n"), b"AM_DIAG:G:ROOT_SYMLINK\nUNKNOWN_SENTINEL_WITH_PATH_OR_PAYLOAD\n", code_from_marker);
        assert!(handle.contains(Code::RootSymlink));
        assert!(!handle.summary().contains(std::str::from_utf8(sentinel).unwrap()));

        for _ in 0..RECORD_LIMIT + 2 {
            handle.record_code(Code::FdOpen, StdoutKind::Empty);
        }
        assert_eq!(handle.length_and_overflow(), (RECORD_LIMIT, 3));

        let script = remote_grok_read_script("/tmp/g", "hooks.json");
        let missing_callsite = script.replace("[ -L \"$G\" ] && am_bad", "[ -L \"$G\" ] && true");
        assert!(instrument(&missing_callsite).is_err(), "missing required G guard was accepted");
        let duplicate_callsite = format!("{script}\n[ -L \"$G\" ] && am_bad\n");
        assert!(instrument(&duplicate_callsite).is_err(), "duplicate G guard was accepted");
    }
}

mod hook_cmd_parts_tests {
    use crate::lifecycle::setup::test_seams::*;
    use crate::lifecycle::start::test_seams::*;
    use crate::lifecycle_tests::prelude::*;
    use crate::lifecycle::setup::write_private;
    /// issue #79：claude 的 `--settings` 要訂 `StopFailure`，daemon 才收得到「回合失敗收尾」的原生訊號；
    /// 既有的兩個 hook 一個都不能掉，三個都指向同一支 hook 指令（分類在 daemon 裡做）。
    /// issue #1770：`PreCompact` 也訂，壓縮中的標記才有來源；issue #2227：`PostCompact` 也訂，壓縮結束才送得到。
    #[test]
    fn a_claude_bot_subscribes_to_stop_failure_as_well_as_stop() {
        let v = claude_settings("/usr/bin/agents-managerd hook claude --bot b1", "/usr/bin/agents-managerd statusline", false, true, "Asia/Taipei");
        let hooks = v.get("hooks").and_then(|h| h.as_object()).expect("hooks");
        let mut names: Vec<&String> = hooks.keys().collect();
        names.sort();
        assert_eq!(
            names,
            vec!["Notification", "PostCompact", "PostToolUse", "PreCompact", "SessionStart", "Stop", "StopFailure", "SubagentStart", "SubagentStop", "UserPromptSubmit"],
            "{hooks:?}"
        );
        for name in ["SessionStart", "Stop", "StopFailure", "PreCompact", "PostCompact", "SubagentStart", "SubagentStop", "PostToolUse", "UserPromptSubmit", "Notification"] {
            let cmd = hooks[name][0]["hooks"][0]["command"].as_str().unwrap_or_default();
            assert!(cmd.contains("hook claude"), "{name} 要指向同一支 hook 指令：{cmd}");
        }
    }
    /// 遠端 Stop 會把 transcript 最後一句人話帶進 spool。路徑必須是這顆 bot 的
    /// `CLAUDE_CONFIG_DIR/projects` 底下的一般檔：別的身份、或 FIFO，都不能讀。
    #[test]
    fn macos_local_remote_hook_reads_only_this_bots_transcript_and_does_not_block_on_a_fifo() {
        let home = crate::testing::track(std::env::temp_dir().join(format!("am-hooksh-{}", crate::db::ulid())));
        let own = home.join("own");
        let other = home.join("other");
        std::fs::create_dir_all(own.join("projects")).unwrap();
        std::fs::create_dir_all(other.join("projects")).unwrap();
        std::fs::create_dir_all(home.join(".claude/projects")).unwrap();
        let line = |text: &str| format!(r#"{{"type":"user","message":{{"content":"{text}"}},"origin":{{"kind":"human"}}}}"#);
        let own_file = own.join("projects/own.jsonl");
        std::fs::write(&own_file, line("OWN-TEXT")).unwrap();
        let foreign = other.join("projects/other.jsonl");
        std::fs::write(&foreign, line("FOREIGN-TEXT")).unwrap();
        let default_file = home.join(".claude/projects/default.jsonl");
        std::fs::write(&default_file, line("DEFAULT-TEXT")).unwrap();
        let fifo = own.join("projects/wait.jsonl");
        assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        let symlink = own.join("projects/link.jsonl");
        std::os::unix::fs::symlink(&own_file, &symlink).unwrap();
        let nested = own.join("projects/nested");
        std::fs::create_dir(&nested).unwrap();
        let nested_file = nested.join("nested.jsonl");
        std::fs::write(&nested_file, line("NESTED-TEXT")).unwrap();
        let nested_link = own.join("projects/alias");
        std::os::unix::fs::symlink(&nested, &nested_link).unwrap();
        let linked_config = home.join("linked-config");
        std::fs::create_dir_all(&linked_config).unwrap();
        std::os::unix::fs::symlink(own.join("projects"), linked_config.join("projects")).unwrap();
        let script = home.join("hook.sh");
        std::fs::write(&script, crate::lifecycle::setup::remote_hook_sh("am-root")).unwrap();
        let spool = home.join("am-root/bots/bot-h/hook-spool.d");
        let run = |path: &std::path::Path, config: Option<&std::path::Path>| {
            if spool.exists() {
                for ent in std::fs::read_dir(&spool).unwrap() {
                    let p = ent.unwrap().path();
                    if p.extension().and_then(|e| e.to_str()) == Some("json") {
                        std::fs::remove_file(p).ok();
                    }
                }
            }
            let payload = serde_json::json!({"hook_event_name":"Stop", "transcript_path":path.to_string_lossy()}).to_string();
            let mut command = std::process::Command::new("/bin/sh");
            command.env_remove("AM_CHILD_OF");
            command
                .arg(&script)
                .arg("claude")
                .arg("bot-h")
                .arg("-")
                .env("HOME", &home)
                .env_remove("HERDR_PANE_ID")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            if let Some(config) = config {
                command.env("CLAUDE_CONFIG_DIR", config);
            } else {
                command.env_remove("CLAUDE_CONFIG_DIR");
            }
            let mut child = command.spawn().unwrap();
            use std::io::Write as _;
            child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if std::time::Instant::now() >= deadline {
                    child.kill().ok();
                    let status = child.wait().unwrap();
                    panic!("hook.sh 必須在 30 秒內結束，不能卡在 transcript：{status}");
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            };
            assert!(status.success(), "hook.sh 必須在 30 秒內成功結束：{status}");
            let body = std::fs::read_dir(&spool)
                .ok()
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
                .find(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
                .map(|e| std::fs::read_to_string(e.path()).unwrap())
                .unwrap_or_default();
            body
        };
        let foreign_body = run(&foreign, Some(&own));
        assert!(!foreign_body.contains("FOREIGN-TEXT"), "別的 CLAUDE_CONFIG_DIR 不能進 spool：{foreign_body}");
        let own_body = run(&own_file, Some(&own));
        assert!(own_body.contains("OWN-TEXT"), "一般 transcript 仍要帶最後一句：{own_body}");
        let fifo_body = run(&fifo, Some(&own));
        assert!(!fifo_body.contains("agm_user_text"), "FIFO 不能當 transcript：{fifo_body}");
        let symlink_body = run(&symlink, Some(&own));
        assert!(!symlink_body.contains("agm_user_text"), "transcript symlink 不能讀：{symlink_body}");
        let nested_link_body = run(&nested_link.join("nested.jsonl"), Some(&own));
        assert!(!nested_link_body.contains("agm_user_text"), "projects 底下的 symlink 路徑不能讀：{nested_link_body}");
        let hardlink = own.join("projects/hardlink.jsonl");
        std::fs::hard_link(&own_file, &hardlink).unwrap();
        let hardlink_body = run(&hardlink, Some(&own));
        assert!(!hardlink_body.contains("agm_user_text"), "hardlink 不能讀：{hardlink_body}");
        let linked_projects_body = run(&own_file, Some(&linked_config));
        assert!(!linked_projects_body.contains("agm_user_text"), "projects symlink 不能讀：{linked_projects_body}");
        let default_body = run(&default_file, None);
        assert!(default_body.contains("DEFAULT-TEXT"), "CLAUDE_CONFIG_DIR 未設時要用 ~/.claude：{default_body}");
    }
    /// 產生出來的 hook 與 statusLine 指令，真的丟給 daemon 自己的 CLI parser 要過（2026-09-15 回歸：hook 加了
    /// `--data-dir`，statusline 子命令不認，claude 的狀態列整個不見）。只比對字串抓不到這種事。
    #[test]
    fn the_generated_hook_and_statusline_commands_parse() {
        use clap::Parser as _;
        let hook = hook_cmd_parts_for("/usr/bin/agents-managerd", 7788, "b1", "claude", "/tmp/am-iso");
        crate::Cli::try_parse_from(&hook).unwrap_or_else(|e| panic!("hook argv 不合法：{e}\n{hook:?}"));
        let statusline = crate::lifecycle::setup::test_seams::statusline_parts(hook);
        assert_eq!(statusline[1], "statusline");
        crate::Cli::try_parse_from(&statusline).unwrap_or_else(|e| panic!("statusline argv 不合法：{e}\n{statusline:?}"));
    }
    #[cfg(unix)]
    #[test]
    fn write_private_replaces_atomically_and_cleans_up_on_failure() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-wp-atomic-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.json");
        let alias = dir.join("alias.json");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::hard_link(&path, &alias).unwrap();
        write_private(&path, b"new-secret").unwrap();
        assert_eq!(std::fs::read(&alias).unwrap(), b"old", "不可在舊 inode 上原地截斷寫入");
        assert_eq!(std::fs::read(&path).unwrap(), b"new-secret");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let bad = dir.join("d");
        std::fs::create_dir_all(bad.join("x")).unwrap();
        assert!(write_private(&bad, b"secret").is_err());
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).filter(|n| n.ends_with(".tmp")).collect();
        assert!(left.is_empty(), "暫存檔要清掉：{left:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

mod pane_env_tests {
    use crate::lifecycle::setup::test_seams::*;
    use crate::lifecycle::start::test_seams::*;
    use crate::lifecycle_tests::prelude::*;
    use crate::lifecycle::setup::*;
    use crate::testing as tt;
    async fn remote_home(app: &Arc<App>, host: &str, home: &str) {
        let conn = app.hosts.insert_remote_for_test(crate::config::HostCfg { name: host.into(), ssh: "unused".into(), herdr_session: "agents-manager".into(), ..Default::default() }).await;
        *conn.remote_home.lock().await = Some(home.into());
    }
    #[tokio::test]
    async fn remote_host_with_expert_refuses_new_pane_without_daemon_url() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "remote-other").await;
        remote_home(&env.app, "box", "/home/remote").await;
        sqlx::query("INSERT INTO projects(id,path,label,host,mode,created_at) VALUES('expert-other','/tmp/expert','expert','box','expert',?)").bind(crate::db::now()).execute(env.app.db()).await.unwrap();
        let err = pane_env(&env.app, &bot, "box", "run", "name", None).await.unwrap_err();
        assert!(err.to_string().contains("daemon_url"), "{err:?}");
    }
    #[tokio::test]
    async fn daemon_url_is_owned_by_daemon_and_missing_remote_is_not_guessed() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "url").await;
        remote_home(&env.app, "box", "/home/remote").await;
        let local = pane_env(&env.app, &bot, LOCAL_HOST, "run", "name", None).await.unwrap();
        assert_eq!(local["AM_DAEMON_URL"], json!(format!("http://127.0.0.1:{}", env.app.port)));
        let remote = pane_env(&env.app, &bot, "box", "run", "name", None).await.unwrap();
        assert!(remote.get("AM_DAEMON_URL").is_none());
        let source_conn = env.app.hosts.get("box").await.unwrap();
        *source_conn.remote_ssh_source.lock().await = Some("100.64.0.1".parse().unwrap());
        let remote = pane_env(&env.app, &bot, "box", "run", "name", None).await.unwrap();
        assert_eq!(remote["AM_DAEMON_URL"], json!(format!("http://100.64.0.1:{}", env.app.port)));
        let run = tt::fake_run(&env.app, &bot.id).await;
        crate::lifecycle::start::test_seams::record_daemon_url_injected(env.app.db(), &run, &remote).await.unwrap();
        let injected: i64 = sqlx::query_scalar("SELECT daemon_url_injected FROM runs WHERE id=?").bind(&run).fetch_one(env.app.db()).await.unwrap();
        assert_eq!(injected, 1, "the inferred remote address is recorded for the active run");

        let conn = env.app.hosts.insert_remote_for_test(crate::config::HostCfg { daemon_url: Some("http://100.64.0.1:7788".into()), name: "box2".into(), ssh: "unused".into(), herdr_session: "agents-manager".into(), ..Default::default() }).await;
        *conn.remote_home.lock().await = Some("/home/remote".into());
        *conn.remote_ssh_source.lock().await = Some("2001:db8::1".parse().unwrap());
        let remote = pane_env(&env.app, &bot, "box2", "run", "name", None).await.unwrap();
        assert_eq!(remote["AM_DAEMON_URL"], json!("http://100.64.0.1:7788"));
    }
    /// §6.5f：本機 pane 拿到自己的 outbox（啟動時就建好），自訂 env 搬不走；遠端拿那台上的目錄（網頁走 ssh 列）。
    #[tokio::test]
    async fn a_local_pane_gets_its_own_outbox_that_custom_env_cannot_move() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        remote_home(&env.app, "box", "/home/remote").await;
        let want = env.app.data_dir.join("outbox").join(&bot.id);
        let e = pane_env(&env.app, &bot, LOCAL_HOST, "run-1", "proj-alfa", None).await.unwrap();
        assert_eq!(e["AM_OUTBOX"], json!(want.to_string_lossy()));
        assert!(want.is_dir(), "啟動時就建好");
        let remote_want = json!(format!("/home/remote/.config/agents-manager/outbox/{}", bot.id));
        assert_eq!(pane_env(&env.app, &bot, "box", "run-1", "proj-alfa", None).await.unwrap()["AM_OUTBOX"], remote_want, "遠端給那台上的目錄");

        sqlx::query("UPDATE bots SET env_json = ? WHERE id = ?")
            .bind(r#"{"AM_OUTBOX":"/elsewhere","FOO":"kept"}"#)
            .bind(&bot.id)
            .execute(env.app.db())
            .await
            .unwrap();
        let bot = db::bot(env.app.db(), &bot.id).await.unwrap().unwrap();
        let e = pane_env(&env.app, &bot, LOCAL_HOST, "run-1", "proj-alfa", None).await.unwrap();
        assert_eq!(e["AM_OUTBOX"], json!(want.to_string_lossy()), "bot.env 蓋不過去");
        assert_eq!(e["FOO"], json!("kept"));
        assert_eq!(pane_env(&env.app, &bot, "box", "run-1", "proj-alfa", None).await.unwrap()["AM_OUTBOX"], remote_want, "遠端也不留自訂的假路徑");
    }
    #[tokio::test]
    async fn bot_api_token_is_reserved_and_independent_of_hook_installation() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "api-token").await;
        for (enabled, want_hook) in [(0, false), (1, true)] {
            sqlx::query("UPDATE bots SET inject_hooks=?, env_json=? WHERE id=?")
                .bind(enabled)
                .bind(r#"{"AM_BOT_ID":"forged","AM_BOT_TOKEN":"forged","AM_HOOK_TOKEN":"forged"}"#)
                .bind(&bot.id)
                .execute(env.app.db())
                .await
                .unwrap();
            let current = db::bot(env.app.db(), &bot.id).await.unwrap().unwrap();
            let pane = pane_env(&env.app, &current, LOCAL_HOST, "run-1", "proj-api-token", None).await.unwrap();
            assert_eq!(pane["AM_BOT_ID"], json!(bot.id));
            assert_eq!(pane["AM_BOT_TOKEN"], json!(bot.hook_token), "API identity is always the stored per-bot token");
            assert_eq!(pane.get("AM_HOOK_TOKEN").is_some(), want_hook, "hook env still follows inject_hooks");
            if want_hook {
                assert_eq!(pane["AM_HOOK_TOKEN"], json!(bot.hook_token));
            }
        }
    }
    /// claude 2.1.280 的「建議下一句」不再被 AG Man 關掉（網頁要顯示、一鍵送出，2026-10-03）：本機、遠端 pane 都不帶這個 env，
    /// 跟著 CLI 自己的設定走；bot env 設 `false` 的照樣關得掉。
    #[tokio::test]
    async fn claude_panes_leave_prompt_suggestions_to_the_cli() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        remote_home(&env.app, "box", "/home/remote").await;
        for host in [LOCAL_HOST, "box"] {
            let e = pane_env(&env.app, &bot, host, "run-1", "proj-alfa", None).await.unwrap();
            assert!(e.get("CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION").is_none(), "{host}");
        }

        sqlx::query("UPDATE bots SET env_json = ? WHERE id = ?")
            .bind(r#"{"CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION":"false"}"#)
            .bind(&bot.id)
            .execute(env.app.db())
            .await
            .unwrap();
        let bot = db::bot(env.app.db(), &bot.id).await.unwrap().unwrap();
        let e = pane_env(&env.app, &bot, LOCAL_HOST, "run-1", "proj-alfa", None).await.unwrap();
        assert_eq!(e["CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION"], json!("false"), "使用者自己要關就尊重");
    }
    /// #92 live-SSH：CLI 只裝在遠端的 `~/.local/bin`（`remote_path` 就是為這個設的），pane 的 PATH 卻寫死成
    /// issue #1775：母 pane 的 shim 用 `AM_IDENTITIES` 查 `agent start --identity <名>`。只列那台看得到的身份
    /// （別台的 cc9 不列），帳號目錄展開成那台的家目錄，其他 key（這裡的 API key）不進清單。
    #[tokio::test]
    async fn a_pane_lists_the_identities_its_host_can_use_for_the_shim() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        remote_home(&env.app, "box", "/home/remote").await;
        let on = |name: &str, kind: &str, host: &str, vars: &[(&str, &str)]| crate::config::IdentityCfg {
            name: name.into(),
            kind: kind.into(),
            host: Some(host.into()),
            env: vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            args: vec![],
        };
        env.app
            .cfg
            .update(|c| {
                c.identities = vec![
                    on("cc1", "claude", "box", &[("CLAUDE_CONFIG_DIR", "$HOME/.claude-cc1")]),
                    on("cx1", "codex", "box", &[("CODEX_HOME", "~/.codex-cx1"), ("OPENAI_API_KEY", "sk-secret")]),
                    on("cc9", "claude", "local", &[("CLAUDE_CONFIG_DIR", "/home/me/.claude-cc9")]),
                ];
                Ok(())
            })
            .await
            .unwrap();
        let e = pane_env(&env.app, &bot, "box", "run-1", "proj-alfa", None).await.unwrap();
        assert_eq!(e["AM_IDENTITIES"], json!("claude/cc1:CLAUDE_CONFIG_DIR=/home/remote/.claude-cc1;codex/cx1:CODEX_HOME=/home/remote/.codex-cx1;agy/ag0:HOME=/home/remote"), "內建的 agy 預設帳號 ag0 每台都列，帶那台的真家目錄");
    }
    /// `<shim>:/usr/local/bin:…`，claude 起不來（`command not found`）。遠端 pane 的 PATH 要照 host 的 `remote_path`，
    /// 開頭的 `$HOME`／`~` 展開成那台的 home（herdr 照字面設 env，不經 shell）。
    #[tokio::test]
    async fn a_remote_pane_path_keeps_the_hosts_remote_path() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let host_cfg = crate::config::HostCfg { name: "box".into(), ssh: "127.0.0.1".into(), ssh_port: port, ssh_opts: vec!["-o".into(), "ConnectTimeout=2".into()], herdr_session: "agents-manager".into(), remote_path: "/opt/tools/bin:$HOME/.local/bin".into(), ..Default::default() };
        env.app.hosts.apply_config(&env.app, &[host_cfg]).await;
        *env.app.hosts.get("box").await.unwrap().remote_home.lock().await = Some("/home/u".into());

        let shim = "/home/u/.config/agents-manager/bots/B/bin";
        let e = pane_env(&env.app, &bot, "box", "run-1", "proj-alfa", Some(shim)).await.unwrap();
        assert_eq!(
            e["PATH"],
            json!(format!("{shim}:/opt/tools/bin:/home/u/.local/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin")),
            "shim 最前、接著 remote_path、最後系統目錄"
        );
        env.app.hosts.remove(&env.app, "box").await;
    }
    #[tokio::test]
    async fn pane_env_fails_closed_on_remote_home_error_and_recovers_with_remote_home() {
        let env = tt::env().await;
        let app = &env.app;
        let host = format!("pane-home-616-{}", crate::db::ulid().to_ascii_lowercase());
        let conn = app.hosts.insert_remote_for_test(crate::config::HostCfg { name: host.clone(), ssh: "unused".into(), herdr_session: "agents-manager".into(), ..Default::default() }).await;
        let bot = tt::claude_bot(app, &env.project_id, "home-failure").await;
        app.cfg.update(|cfg| {
            cfg.identities.push(crate::config::IdentityCfg {
                name: "cc1".into(),
                kind: "claude".into(),
                host: Some(host.clone()),
                env: [("CLAUDE_CONFIG_DIR".into(), "~/.claude-cc1".into())].into(),
                args: vec![],
            });
            Ok(())
        }).await.unwrap();
        sqlx::query("UPDATE bots SET identity = ? WHERE id = ?").bind("cc1").bind(&bot.id).execute(app.db()).await.unwrap();
        let bot = db::bot(app.db(), &bot.id).await.unwrap().unwrap();

        let calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let calls2 = calls.clone();
        crate::hosts::set_ssh_fake(&host, move |script| {
            calls2.lock().unwrap().push(script.to_string());
            Err(anyhow::anyhow!("injected remote HOME read failure"))
        });
        let error = pane_env(app, &bot, &host, "run-1", "proj-home-failure", None)
            .await
            .expect_err("a remote pane cannot launch with unknown HOME authority");
        assert!(error.to_string().contains("HOME"), "preserve the cause for retry/reporting: {error:#}");
        assert_eq!(calls.lock().unwrap().len(), 1, "no path-dependent remote operation follows failed HOME lookup");

        *conn.remote_home.lock().await = Some("/home/remote-pane".into());
        let pane = pane_env(app, &bot, &host, "run-2", "proj-home-failure", None).await.unwrap();
        assert_eq!(pane["CLAUDE_CONFIG_DIR"], "/home/remote-pane/.claude-cc1");
    }
    /// hook 打不通時會 spool 到 `AM_DATA_DIR`；沒注入的話隔離跑的 bot 會把檔案丟進正式資料目錄，
    /// 換成正式 daemon 去重播它（sol 複審 2026-09-14）。
    #[tokio::test]
    async fn a_local_pane_learns_the_daemons_data_dir() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        let e = pane_env(&env.app, &bot, LOCAL_HOST, "run-1", "proj-alfa", None).await.unwrap();
        assert_eq!(e["AM_DATA_DIR"], json!(env.app.data_dir.to_string_lossy()));
        assert_ne!(e["AM_DATA_DIR"], json!(""));

        remote_home(&env.app, "box", "/home/remote").await;
        let remote = pane_env(&env.app, &bot, "box", "run-1", "proj-alfa", None).await.unwrap();
        assert!(remote.get("AM_DATA_DIR").is_none());

        assert!(e.get("AM_INSTANCE").is_none());
        env.app.set_instance(Some("a1b2".into()));
        for host in [LOCAL_HOST, "box"] {
            let iso = pane_env(&env.app, &bot, host, "run-1", "proj-alfa", None).await.unwrap();
            assert_eq!(iso["AM_INSTANCE"], json!("a1b2"), "{host}");
        }

        let refused = env
            .app
            .cfg
            .update(|cfg| {
                cfg.identities = vec![crate::config::IdentityCfg {
                    name: "cc9".into(),
                    kind: "claude".into(),
                    host: None,
                    env: [("AM_DATA_DIR".to_string(), "/identity/dir".to_string())].into(),
                    args: vec![],
                }];
                Ok(())
            })
            .await;
        assert!(format!("{refused:?}").contains("reserved"), "identity.env 的 AM_* 要在寫入時被擋：{refused:?}");

        env.app
            .cfg
            .update(|cfg| {
                let forged = || -> std::collections::BTreeMap<String, String> {
                    [("ID_ONLY", "kept")]
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .into()
                };
                cfg.identities = vec![
                    crate::config::IdentityCfg {
                        name: "cc9".into(),
                        kind: "claude".into(),
                        host: None,
                        env: forged(),
                        args: vec![],
                    },
                    crate::config::IdentityCfg {
                        name: "cc9".into(),
                        kind: "claude".into(),
                        host: Some("box".into()),
                        env: forged(),
                        args: vec![],
                    },
                ];
                Ok(())
            })
            .await
            .unwrap();
        for layer in ["identity", "bot"] {
            let (identity, env_json) = match layer {
                "identity" => (Some("cc9"), r#"{"FOO":"kept"}"#),
                _ => (None, r#"{"AM_INSTANCE":"forged-by-bot","AM_DATA_DIR":"/bot/dir","FOO":"kept"}"#),
            };
            sqlx::query("UPDATE bots SET identity = ?, env_json = ? WHERE id = ?")
                .bind(identity)
                .bind(env_json)
                .bind(&bot.id)
                .execute(env.app.db())
                .await
                .unwrap();
            let bot = db::bot(env.app.db(), &bot.id).await.unwrap().unwrap();
            for instance in [None, Some("a1b2".to_string())] {
                env.app.set_instance(instance.clone());
                for host in [LOCAL_HOST, "box"] {
                    let got = pane_env(&env.app, &bot, host, "run-1", "proj-alfa", None).await.unwrap();
                    let case = format!("layer={layer} instance={instance:?} host={host}");
                    assert_eq!(got.get("AM_INSTANCE"), instance.as_ref().map(|s| json!(s)).as_ref(), "{case}");
                    let dir = (host == LOCAL_HOST).then(|| json!(env.app.data_dir.to_string_lossy()));
                    assert_eq!(got.get("AM_DATA_DIR"), dir.as_ref(), "{case}");
                    assert_eq!(got["FOO"], json!("kept"), "其他自訂 env 照舊生效：{case}");
                    if layer == "identity" {
                        assert_eq!(got["ID_ONLY"], json!("kept"), "{case}");
                    }
                }
            }
        }
    }
}

mod remote_install_permission_tests {
    use crate::lifecycle::setup::test_seams::*;
    use crate::lifecycle::start::test_seams::*;
    use crate::lifecycle_tests::prelude::*;
    use crate::lifecycle::setup::*;
    use crate::config::HostCfg;
    use crate::testing as tt;
    use std::os::unix::fs::PermissionsExt as _;
    fn mode_of(p: &std::path::Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
    }
    /// ssh 換成「就地用 /bin/sh 跑這段腳本」：不連任何主機，但跑的是正式碼產生的同一段腳本。
    #[tokio::test]
    async fn the_remote_install_tightens_the_bot_dir_and_repairs_old_modes() {
        let env = tt::env().await;
        let host = format!("perm-{}", crate::db::ulid());
        let home = crate::testing::track(std::env::temp_dir().join(format!("am-perm-{}", crate::db::ulid())));
        std::fs::create_dir_all(&home).unwrap();
        let conn = env
            .app
            .hosts
            .insert_remote_for_test(HostCfg { name: host.clone(), ssh: "unused".into(), herdr_session: "am-test".into(), ..Default::default() })
            .await;
        *conn.remote_home.lock().await = Some(home.to_string_lossy().into_owned());
        crate::hosts::set_ssh_fake(&host, |script| {
            let out = std::process::Command::new("/bin/sh").arg("-c").arg(script).output()?;
            if !out.status.success() {
                anyhow::bail!("script failed: {}", String::from_utf8_lossy(&out.stderr));
            }
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        });

        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        let dir = home.join(crate::hosts::REMOTE_ROOT).join("bots").join(&bot.id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let spool = dir.join("hook-spool.jsonl");
        std::fs::write(&spool, "{}\n").unwrap();
        std::fs::set_permissions(&spool, std::fs::Permissions::from_mode(0o644)).unwrap();

        let p = crate::lifecycle::setup::test_seams::install_remote_hook(&conn, &bot, None, "Asia/Taipei").await.unwrap();
        assert_eq!(p.dir, dir.to_string_lossy());
        assert_eq!(mode_of(&dir), 0o700, "目錄要收回來");
        assert_eq!(mode_of(std::path::Path::new(&p.hook_sh)), 0o700, "hook.sh 自己跑得起來就好");
        assert_eq!(mode_of(std::path::Path::new(&p.settings)), 0o600, "settings 不給別人讀");
        assert_eq!(mode_of(&spool), 0o600, "升級前留下的 spool 也要修");
        assert_eq!(std::fs::read_to_string(&spool).unwrap(), "{}\n", "修權限不准動內容");
        let _ = std::fs::remove_dir_all(&home);
    }
    #[tokio::test]
    async fn remote_grok_install_preserves_user_hooks_and_unrelated_keys() {
        let env = tt::env().await;
        let host = format!("grok-hook-{}", crate::db::ulid());
        let home = tt::scratch_dir("am-remote-grok-home");
        let grok_home = home.join(".grok");
        let hooks_dir = grok_home.join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        std::fs::set_permissions(&hooks_dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let hooks_path = hooks_dir.join(GROK_HOOKS_FILE);
        let existing = json!({
            "hooks": {
                "PreToolUse": [{"hooks": [{"type": "command", "command": "/home/u/guard.sh", "timeout": 9}]}],
                "Stop": [{"hooks": [{"type": "command", "command": "/home/u/stop.sh", "timeout": 8}]}]
            },
            "user_note": "keep me"
        });
        std::fs::write(&hooks_path, existing.to_string()).unwrap();

        let conn = env.app.hosts.insert_remote_for_test(HostCfg { name: host.clone(), ssh: "unused".into(), herdr_session: "am-test".into(), ..Default::default() }).await;
        *conn.remote_home.lock().await = Some(home.to_string_lossy().into_owned());
        let diag = crate::lifecycle_tests::setup::remote_grok_diagnostic_tests::Handle::new();
        let sink = diag.clone();
        crate::hosts::set_ssh_fake(&host, move |script| {
            let out = crate::lifecycle_tests::setup::remote_grok_diagnostic_tests::run(script, &sink, None, None)
                .map_err(|error| anyhow::Error::msg(error.summary()))?;
            if out.status != Some(0) {
                anyhow::bail!("test remote script exit status: {:?}", out.status);
            }
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        });

        install_remote_grok_hook(&conn, &json!({}), None)
            .await
            .unwrap_or_else(|_| panic!("remote Grok install failed: {}", diag.summary()));

        let installed: Value = serde_json::from_str(&std::fs::read_to_string(&hooks_path).unwrap()).unwrap();
        assert_eq!(mode_of(&hooks_dir), 0o700, "remote hooks directory must not let other users replace commands");
        assert_eq!(mode_of(&hooks_path), 0o600, "remote hook JSON stays private");
        let dispatcher = home.join(crate::hosts::REMOTE_ROOT).join(GROK_DISPATCH_SH);
        assert_eq!(mode_of(&dispatcher), 0o700, "remote dispatcher stays private and executable");
        assert_eq!(installed["user_note"], "keep me");
        assert_eq!(installed["hooks"]["PreToolUse"], existing["hooks"]["PreToolUse"]);
        assert_eq!(
            installed["hooks"]["Stop"][0]["hooks"][0]["command"],
            "/home/u/stop.sh",
            "remote install preserves existing user entries"
        );
        assert!(installed["hooks"]["Stop"].as_array().unwrap().iter().any(|group| {
            group["hooks"].as_array().is_some_and(|hooks| hooks.iter().any(|hook| {
                hook["command"].as_str().is_some_and(|command| command.ends_with("/grok-hook.sh"))
            }))
        }));
    }
    #[tokio::test]
    async fn remote_grok_install_refuses_a_directory_at_the_hooks_file_path() {
        let env = tt::env().await;
        let host = format!("grok-hook-dir-{}", crate::db::ulid());
        let home = tt::scratch_dir("am-remote-grok-dir-home");
        let hooks_dir = home.join(".grok/hooks");
        let hooks_path = hooks_dir.join(GROK_HOOKS_FILE);
        std::fs::create_dir_all(&hooks_path).unwrap();
        let user_file = hooks_path.join("user-config.json");
        std::fs::write(&user_file, "leave this directory alone").unwrap();

        let conn = env.app.hosts.insert_remote_for_test(HostCfg { name: host.clone(), ssh: "unused".into(), herdr_session: "am-test".into(), ..Default::default() }).await;
        *conn.remote_home.lock().await = Some(home.to_string_lossy().into_owned());
        let diag = crate::lifecycle_tests::setup::remote_grok_diagnostic_tests::Handle::new();
        let sink = diag.clone();
        crate::hosts::set_ssh_fake(&host, move |script| {
            let out = crate::lifecycle_tests::setup::remote_grok_diagnostic_tests::run(script, &sink, None, None)
                .map_err(|error| anyhow::Error::msg(error.summary()))?;
            if out.status != Some(0) {
                anyhow::bail!("test remote script exit status: {:?}", out.status);
            }
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        });

        assert!(
            install_remote_grok_hook(&conn, &json!({}), None).await.is_err(),
            "a directory isn't an absent hooks file; {}",
            diag.summary()
        );
        assert!(diag.contains(crate::lifecycle_tests::setup::remote_grok_diagnostic_tests::Code::FileTypeOrSymlink), "{}", diag.summary());
        assert_eq!(std::fs::read_to_string(user_file).unwrap(), "leave this directory alone");
        assert_eq!(std::fs::read_dir(&hooks_path).unwrap().count(), 1, "don't install a JSON file inside the user's directory");
    }
}

mod remote_grok_merge_tests {
    use crate::lifecycle::setup::test_seams::*;
    use crate::lifecycle::start::test_seams::*;
    use crate::lifecycle_tests::prelude::*;
    use crate::config::HostCfg;
    use crate::testing as tt;
    use serde_json::{json, Value};
    /// 只在失敗路徑呼叫：`sh -x` 逐行印出讀端腳本，看是哪一道 `am_bad` 閘擋下；再附上相關路徑的 mode／uid。
    /// 子行程只帶 `PATH`、不繼承環境，所以 token 之類的值不會進訊息；腳本本身只含路徑。
    pub(super) fn read_script_diag(script: &str, paths: &[&std::path::Path]) -> String {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let trace = match std::process::Command::new("/bin/sh")
            .arg("-x")
            .arg("-c")
            .arg(script)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .output()
        {
            Ok(o) => format!(
                "exit={:?} first_stdout_line={:?}\n{}",
                o.status.code(),
                String::from_utf8_lossy(&o.stdout).lines().next(),
                String::from_utf8_lossy(&o.stderr)
            ),
            Err(e) => format!("sh spawn failed: {e}"),
        };
        let states: Vec<String> = paths
            .iter()
            .map(|p| match std::fs::symlink_metadata(p) {
                Ok(m) => format!(
                    "{} mode={:o} uid={} dir={} symlink={}",
                    p.display(),
                    m.permissions().mode() & 0o7777,
                    m.uid(),
                    m.is_dir(),
                    m.file_type().is_symlink()
                ),
                Err(e) => format!("{} {e}", p.display()),
            })
            .collect();
        format!("{trace}\n{}", states.join("\n"))
    }
    #[tokio::test]
    async fn remote_grok_install_preserves_user_hooks_in_its_shared_file() {
        let env = tt::env().await;
        let host = format!("grok-merge-{}", crate::db::ulid());
        let home = tt::scratch_dir("am-remote-grok-merge");
        let grok_home = home.join(".grok");
        let hooks = grok_home.join("hooks").join(crate::lifecycle::setup::grok_hooks_file(None));
        std::fs::create_dir_all(hooks.parent().unwrap()).unwrap();
        std::fs::write(&hooks, json!({
            "note": "keep this",
            "hooks": {
                "SessionStart": [{"hooks": [{"type": "command", "command": "/home/u/grok-hook.sh", "timeout": 9}]}],
                "PreToolUse": [{"hooks": [{"type": "command", "command": "/home/u/guard.sh"}]}]
            }
        }).to_string()).unwrap();

        let conn = env.app.hosts.insert_remote_for_test(HostCfg { name: host.clone(), ssh: "unused".into(), herdr_session: "am-test".into(), ..Default::default() }).await;
        *conn.remote_home.lock().await = Some(home.to_string_lossy().into_owned());
        let remote_home = home.to_string_lossy().into_owned();
        let fake_home = remote_home.clone();
        let diag = crate::lifecycle_tests::setup::remote_grok_diagnostic_tests::Handle::new();
        let sink = diag.clone();
        crate::hosts::set_ssh_fake(&host, move |script| {
            let out = crate::lifecycle_tests::setup::remote_grok_diagnostic_tests::run(script, &sink, Some(&fake_home), None)
                .map_err(|error| anyhow::Error::msg(error.summary()))?;
            if out.status != Some(0) {
                anyhow::bail!("test remote script exit status: {:?}", out.status);
            }
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        });

        crate::lifecycle::setup::test_seams::install_remote_grok_hook(&conn, &json!({"GROK_HOME": grok_home.to_string_lossy()}), None)
            .await
            .unwrap_or_else(|_| panic!("remote Grok install failed: {}", diag.summary()));
        let actual: Value = serde_json::from_slice(&std::fs::read(&hooks).unwrap()).unwrap();
        assert_eq!(actual["note"], "keep this");
        let commands = |event: &str| -> Vec<String> {
            actual["hooks"][event].as_array().into_iter().flatten()
                .flat_map(|g| g["hooks"].as_array().into_iter().flatten())
                .filter_map(|h| h["command"].as_str().map(str::to_owned)).collect()
        };
        assert_eq!(commands("SessionStart").iter().filter(|s| *s == "/home/u/grok-hook.sh").count(), 1);
        assert!(commands("SessionStart").iter().any(|s| s.ends_with("/grok-hook.sh") && s.starts_with(remote_home.as_str())));
        assert_eq!(commands("PreToolUse"), ["/home/u/guard.sh"]);
    }
    #[test]
    fn remote_grok_reader_caps_growth_after_the_fd_size_check() {
        use base64::Engine as _;
        let base = tt::scratch_dir("am-remote-grok-growth");
        let hooks_dir = base.join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let hooks = hooks_dir.join(crate::lifecycle::setup::grok_hooks_file(None));
        std::fs::write(&hooks, b"{}\n").unwrap();
        let append = crate::hosts::sh_quote(&hooks.to_string_lossy());
        let script = format!(
            "stat() {{ command stat \"$@\"; rc=$?; case \"$*\" in *\"%d %i %h %s /dev/fd/3\"*) dd if=/dev/zero bs=300000 count=1 >> {append} 2>/dev/null;; esac; return \"$rc\"; }}\n{}",
            crate::lifecycle::setup::test_seams::remote_grok_read_script(&hooks_dir.to_string_lossy(), &crate::lifecycle::setup::grok_hooks_file(None)),
        );
        let out = std::process::Command::new("/bin/sh").arg("-c").arg(&script).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let stdout = String::from_utf8(out.stdout).unwrap();
        let (marker, encoded) = stdout.split_once('\n').unwrap();
        assert_eq!(marker, "AM_GROK_FILE", "{}", read_script_diag(&script, &[hooks_dir.as_path(), hooks.as_path()]));
        let encoded: String = encoded.chars().filter(|c| !c.is_ascii_whitespace()).collect();
        let bytes = base64::engine::general_purpose::STANDARD.decode(encoded).unwrap();
        assert!(bytes.len() <= 262_145, "remote hook file grew past the read cap: {} bytes", bytes.len());
    }
}

mod codex_stop_args_tests {
    use crate::lifecycle::setup::test_seams::*;
    use crate::lifecycle::start::test_seams::*;
    use crate::lifecycle_tests::prelude::*;
    use crate::lifecycle::setup::*;
    use crate::testing as tt;
    #[tokio::test]
    async fn the_default_and_untrusted_starts_retain_legacy_without_creating_trust() {
        let env = tt::env().await;
        let mut bot = tt::claude_bot(&env.app, &env.project_id, "stop-hook-canary").await;
        sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&bot.id).execute(env.app.db()).await.unwrap();
        bot.kind = "codex".into();
        let project = db::project(env.app.db(), &env.project_id).await.unwrap().unwrap();
        let home = env.dir.join("codex-identity");
        let bin = env.dir.join("codex-bin");
        std::fs::create_dir_all(&bin).unwrap();
        tt::write_exec(&bin.join("codex"), "#!/bin/sh\nprintf 'codex-cli 0.164.0\\n'\n");
        let pane_env = json!({"CODEX_HOME":home, "PATH":bin});
        let before = injected_args(&env.app, &bot, &project, &pane_env).await.unwrap();
        let expected = hook_cmd_parts(&env.app, &bot, "codex");
        let mut legacy = permission_args("codex", bot.auto_approve != 0);
        legacy.extend(["-c".into(), format!("notify={}", serde_json::to_string(&expected).unwrap())]);
        assert_eq!(before, legacy, "default argv must be byte-for-byte legacy");
        assert!(!home.exists(), "default must not write hooks.json");
        assert!(!env.app.data_dir.join("codex-hook.sh").exists(), "default must not install dispatcher");

        env.app.cfg.update(|cfg| { cfg.codex.stop_hook = true; Ok(()) }).await.unwrap();
        let untrusted = injected_args(&env.app, &bot, &project, &pane_env).await.unwrap();
        assert_eq!(untrusted, legacy);
        assert!(!untrusted.contains(&"notify=[]".into()));
        assert!(!home.join("config.toml").exists(), "installer never creates trust state");
        let hooks_path = home.join("hooks.json");
        assert!(!hooks_path.exists(), "a start never registers an untrusted Stop handler");
        let key = format!("{}:stop:0:0", hooks_path.display());
        let reviewed = format!("[hooks.state.{key:?}]\ntrusted_hash=\"user-owned-sentinel\"\n");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("config.toml"), &reviewed).unwrap();
        let trusted = injected_args(&env.app, &bot, &project, &pane_env).await.unwrap();
        assert_eq!(trusted, legacy);
        assert!(!trusted.contains(&"notify=[]".into()));
        assert!(!hooks_path.exists());
        assert_eq!(std::fs::read_to_string(home.join("config.toml")).unwrap(), reviewed);
        let command = am_base::hosts::sh_quote(&env.app.data_dir.join("codex-hook.sh").to_string_lossy());
        am_base::codex_hook::install_and_trust_local(&home, &env.app.data_dir, env.app.instance().as_deref()).unwrap();
        let mut old_hooks: Value = serde_json::from_slice(&std::fs::read(&hooks_path).unwrap()).unwrap();
        old_hooks["hooks"].as_object_mut().unwrap().remove("SessionStart");
        old_hooks["hooks"].as_object_mut().unwrap().remove("UserPromptSubmit");
        std::fs::write(&hooks_path, serde_json::to_vec(&old_hooks).unwrap()).unwrap();
        let good = format!("[hooks.state.{key:?}]\ntrusted_hash={:?}\n", am_base::codex_hook::stop_hook_trust_hash(&command));
        std::fs::write(home.join("config.toml"), &good).unwrap();
        let phase_two = injected_args(&env.app, &bot, &project, &pane_env).await.unwrap();
        let mut expected = permission_args("codex", bot.auto_approve != 0);
        expected.extend(["-c".into(), "notify=[]".into()]);
        assert_eq!(phase_two, expected);
        env.app.cfg.update(|cfg| { cfg.codex.stop_hook = false; Ok(()) }).await.unwrap();
        assert_eq!(injected_args(&env.app, &bot, &project, &pane_env).await.unwrap(), legacy, "switch off: trusted hash changes nothing");
        env.app.cfg.update(|cfg| { cfg.codex.stop_hook = true; Ok(()) }).await.unwrap();
        bot.inject_hooks = 0;
        assert_eq!(injected_args(&env.app, &bot, &project, &pane_env).await.unwrap(), permission_args("codex", bot.auto_approve != 0));
    }
    fn leftover_hooks(command: &str) -> Value {
        json!({"hooks":{"Stop":[
            {"hooks":[{"type":"command","command":"user-first","timeout":5}]},
            {"hooks":[{"type":"command","command":command,"timeout":5}]},
            {"hooks":[{"type":"command","command":"user-last","timeout":5}]}
        ]}})
    }
    fn stop_commands(hooks_path: &std::path::Path) -> Vec<Vec<String>> {
        let v: Value = serde_json::from_slice(&std::fs::read(hooks_path).unwrap()).unwrap();
        v["hooks"]["Stop"].as_array().unwrap().iter()
            .map(|g| g["hooks"].as_array().unwrap().iter().map(|h| h["command"].as_str().unwrap().to_owned()).collect())
            .collect()
    }
    fn backups(dir: &std::path::Path, prefix: &str) -> usize {
        std::fs::read_dir(dir).unwrap().filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().starts_with(prefix)).count()
    }
    /// 事故現場（#1989）：先前版本留下未信任的 handler。下一次啟動（開關開或關）在 codex 起來之前把它拿掉，
    /// 第三方 group 與它們的信任鍵留在原位。
    #[tokio::test]
    async fn a_start_removes_our_untrusted_leftover_and_keeps_third_party_hooks_in_place() {
        for enabled in [true, false] {
            let env = tt::env().await;
            let mut bot = tt::claude_bot(&env.app, &env.project_id, "stop-hook-leftover").await;
            sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&bot.id).execute(env.app.db()).await.unwrap();
            bot.kind = "codex".into();
            let project = db::project(env.app.db(), &env.project_id).await.unwrap().unwrap();
            let home = env.dir.join("codex-identity");
            let bin = env.dir.join("codex-bin");
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::create_dir_all(&home).unwrap();
            tt::write_exec(&bin.join("codex"), "#!/bin/sh\nprintf 'codex-cli 0.169.0\\n'\n");
            let command = am_base::hosts::sh_quote(&env.app.data_dir.join("codex-hook.sh").to_string_lossy());
            let hooks_path = home.join("hooks.json");
            std::fs::write(&hooks_path, leftover_hooks(&command).to_string()).unwrap();
            let third_party = format!("[hooks.state.{:?}]\ntrusted_hash=\"sha256:user-last\"\n", format!("{}:stop:2:0", hooks_path.display()));
            std::fs::write(home.join("config.toml"), &third_party).unwrap();
            env.app.cfg.update(|cfg| { cfg.codex.stop_hook = enabled; Ok(()) }).await.unwrap();
            let args = injected_args(&env.app, &bot, &project, &json!({"CODEX_HOME":home, "PATH":bin})).await.unwrap();
            assert!(!args.contains(&"notify=[]".into()), "untrusted: legacy notify stays (enabled={enabled})");
            assert_eq!(stop_commands(&hooks_path), vec![vec!["user-first".to_string()], vec![], vec!["user-last".to_string()]], "enabled={enabled}");
            assert_eq!(std::fs::read_to_string(home.join("config.toml")).unwrap(), third_party, "third-party trust is untouched");
            assert_eq!(backups(&home, "hooks.json.am-bak-"), 1, "hooks.json is backed up before the write");
            assert_eq!(backups(&home, "config.toml.am-bak-"), 0, "config.toml is not rewritten when we have no entry in it");
        }
    }
    #[tokio::test]
    async fn a_start_keeps_our_trusted_handler_when_enabled_and_removes_it_with_its_trust_when_disabled() {
        let env = tt::env().await;
        let mut bot = tt::claude_bot(&env.app, &env.project_id, "stop-hook-trusted").await;
        sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&bot.id).execute(env.app.db()).await.unwrap();
        bot.kind = "codex".into();
        let project = db::project(env.app.db(), &env.project_id).await.unwrap().unwrap();
        let home = env.dir.join("codex-identity");
        let bin = env.dir.join("codex-bin");
        std::fs::create_dir_all(&bin).unwrap();
        tt::write_exec(&bin.join("codex"), "#!/bin/sh\nprintf 'codex-cli 0.169.0\\n'\n");
        let pane_env = json!({"CODEX_HOME":home, "PATH":bin});
        am_base::codex_hook::install_and_trust_local(&home, &env.app.data_dir, env.app.instance().as_deref()).unwrap();
        let hooks_path = home.join("hooks.json");
        let (hooks_before, config_before) = (std::fs::read(&hooks_path).unwrap(), std::fs::read(home.join("config.toml")).unwrap());
        env.app.cfg.update(|cfg| { cfg.codex.stop_hook = true; Ok(()) }).await.unwrap();
        assert!(injected_args(&env.app, &bot, &project, &pane_env).await.unwrap().contains(&"notify=[]".into()));
        assert_eq!(std::fs::read(&hooks_path).unwrap(), hooks_before, "trusted: not touched");
        assert_eq!(std::fs::read(home.join("config.toml")).unwrap(), config_before);
        assert_eq!(backups(&home, "hooks.json.am-bak-"), 0);
        env.app.cfg.update(|cfg| { cfg.codex.stop_hook = false; Ok(()) }).await.unwrap();
        assert!(!injected_args(&env.app, &bot, &project, &pane_env).await.unwrap().contains(&"notify=[]".into()));
        assert!(stop_commands(&hooks_path).is_empty(), "disabled: our handler is unregistered");
        assert!(!std::fs::read_to_string(home.join("config.toml")).unwrap().contains("trusted_hash"), "and so is its trust entry");
        assert_eq!(backups(&home, "hooks.json.am-bak-"), 1);
    }
    async fn fake_remote(env: &tt::Env, home: &std::path::Path) -> (std::sync::Arc<HostConn>, String) {
        let host = format!("codex-rem-{}", crate::db::ulid().to_ascii_lowercase());
        let conn = env.app.hosts.insert_remote_for_test(crate::config::HostCfg { name: host.clone(), ssh: "unused".into(), herdr_session: "am-test".into(), ..Default::default() }).await;
        *conn.remote_home.lock().await = Some(home.to_string_lossy().into_owned());
        let fake_home = home.to_path_buf();
        crate::hosts::set_ssh_fake(&host, move |script| {
            let out = std::process::Command::new("/bin/sh").arg("-c").arg(script).env("HOME", &fake_home).output()?;
            if !out.status.success() {
                anyhow::bail!("script failed: {}", String::from_utf8_lossy(&out.stderr));
            }
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        });
        (conn, host)
    }
    /// 遠端路徑（m4p 的現場）：未信任的殘留被移除，第三方 group 與信任鍵不位移；已信任的不動；關掉開關才移除。
    #[tokio::test]
    async fn the_remote_reconcile_removes_only_our_untrusted_leftover() {
        let env = tt::env().await;
        let remote_home = tt::scratch_dir("am-remote-codex-leftover");
        let (conn, host) = fake_remote(&env, &remote_home).await;
        let root = crate::hosts::remote_root_for(env.app.instance().as_deref());
        let (codex_home, command) = remote_codex_paths(&json!({}), &conn, &root).await.unwrap();
        assert_eq!(codex_home, format!("{}/.codex", remote_home.display()));
        let dir = std::path::PathBuf::from(&codex_home);
        std::fs::create_dir_all(&dir).unwrap();
        let hooks_path = dir.join("hooks.json");
        std::fs::write(&hooks_path, leftover_hooks(&command).to_string()).unwrap();
        let third_party = format!("model = \"x\"\n\n[hooks.state.{:?}]\ntrusted_hash=\"sha256:user-last\"\n", format!("{}:stop:2:0", hooks_path.display()));
        std::fs::write(dir.join("config.toml"), &third_party).unwrap();

        assert!(!reconcile_remote_stop_hook(&conn, &host, &codex_home, &command, true).await);
        assert_eq!(stop_commands(&hooks_path), vec![vec!["user-first".to_string()], vec![], vec!["user-last".to_string()]]);
        assert_eq!(std::fs::read_to_string(dir.join("config.toml")).unwrap(), third_party);
        assert_eq!(backups(&dir, "hooks.json.am-bak-"), 1);
        assert_eq!(backups(&dir, "config.toml.am-bak-"), 0);
        let settled = std::fs::read(&hooks_path).unwrap();
        assert!(!reconcile_remote_stop_hook(&conn, &host, &codex_home, &command, false).await);
        assert_eq!(std::fs::read(&hooks_path).unwrap(), settled);
        assert_eq!(backups(&dir, "hooks.json.am-bak-"), 1);

        am_base::codex_hook::install_and_trust_remote(&conn, &codex_home, &root, env.app.instance().as_deref()).await.unwrap();
        let registered: serde_json::Value = serde_json::from_slice(&std::fs::read(&hooks_path).unwrap()).unwrap();
        for event in ["SessionStart", "UserPromptSubmit"] {
            assert_eq!(registered["hooks"][event][0]["hooks"][0]["command"], command);
        }
        let trusted = std::fs::read(&hooks_path).unwrap();
        assert!(reconcile_remote_stop_hook(&conn, &host, &codex_home, &command, true).await, "trusted: kept");
        assert_eq!(std::fs::read(&hooks_path).unwrap(), trusted);
        assert!(!reconcile_remote_stop_hook(&conn, &host, &codex_home, &command, false).await);
        let after = std::fs::read_to_string(&hooks_path).unwrap();
        assert!(!after.contains(&command) && after.contains("user-first") && after.contains("user-last"));
        let removed: serde_json::Value = serde_json::from_str(&after).unwrap();
        for event in ["SessionStart", "UserPromptSubmit"] {
            assert!(removed["hooks"][event].as_array().unwrap().is_empty());
        }
        let cfg = std::fs::read_to_string(dir.join("config.toml")).unwrap();
        assert!(cfg.contains("sha256:user-last") && cfg.starts_with("model = \"x\"\n"), "{cfg:?}");
        assert!(!cfg.contains(&am_base::codex_hook::stop_hook_trust_hash(&command)), "{cfg:?}");
    }
}
