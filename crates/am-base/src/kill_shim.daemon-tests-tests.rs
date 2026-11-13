    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn tmp() -> PathBuf {
        crate::testing::scratch_dir("am-kill-shim")
    }

    fn write_exec(p: &Path, content: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// 假的「真 pkill／killall」：把收到的每個參數各印一行，exit 0。
    const STUB: &str = "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done\n";

    /// 一個裝好的環境：`<root>/shim/{pkill,killall}`（真的 shim）、`<root>/real/{pkill,killall}`（stub），PATH 只有這兩個加系統目錄。
    struct Env {
        root: PathBuf,
    }

    impl Env {
        fn new() -> Env {
            let root = tmp();
            for n in NAMES {
                write_exec(&root.join("shim").join(n), shim_sh());
                write_exec(&root.join("real").join(n), STUB);
            }
            Env { root }
        }

        fn path(&self) -> String {
            format!("{}:{}:/usr/bin:/bin", self.root.join("shim").display(), self.root.join("real").display())
        }

        /// 用 `shell` 跑 shim（`sh` 是這台的 /bin/sh：Linux 是 dash、macOS 是 bash 3.2 的 sh 模式）。
        fn run(&self, shell: &str, tool: &str, args: &[&str]) -> (i32, String, String) {
            let out = Command::new(shell).arg(self.root.join("shim").join(tool)).args(args).env("PATH", self.path()).output().unwrap();
            (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
        }
    }

    impl Drop for Env {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// 這台能用來跑 shim 的 POSIX shell（`sh` 一定有；有 bash 就再用 `bash --posix` 的行為跑一遍——但 bash 要當 sh 跑）。
    fn shells() -> Vec<&'static str> {
        let mut v = vec!["sh"];
        if Command::new("bash").arg("-c").arg("true").status().is_ok_and(|s| s.success()) {
            v.push("bash");
        }
        v
    }

    fn rust_denies(tool: &str, args: &[&str]) -> bool {
        let cmd = std::iter::once(tool.to_string()).chain(args.iter().map(|a| am_base::hosts::sh_quote(a))).collect::<Vec<_>>().join(" ");
        matches!(check_command(&cmd), Verdict::Deny(_))
    }

    /// pkill／killall 一律擋：同一張表同時餵 Rust 的 `kill_guard` 和 sh shim，兩邊都要擋、而且擋下時絕不呼叫真的。
    #[test]
    fn the_shim_and_the_rust_guard_both_deny_every_form_under_every_shell() {
        let env = Env::new();
        let cases: &[(&str, &[&str])] = &[
            ("pkill", &[]),
            ("pkill", &["-f"]),
            ("pkill", &["-f", "next-server"]),
            ("pkill", &["-f", "next-server", "-P", "1"]),
            ("pkill", &["-P", "4242"]),
            ("pkill", &["-x", "vite"]),
            ("pkill", &["claude"]),
            ("pkill", &["-U", "1000"]),
            ("pkill", &["-f", "--", "-weird-name"]),
            ("killall", &[]),
            ("killall", &["-9"]),
            ("killall", &["vite"]),
            ("killall", &["-s", "TERM", "next-server"]),
            ("killall", &["-r", "^node$"]),
            ("killall", &["node"]),
        ];
        for shell in shells() {
            for (tool, args) in cases {
                assert!(rust_denies(tool, args), "kill_guard：{tool} {args:?}");
                let (code, out, err) = env.run(shell, tool, args);
                assert_eq!(code, DENIED_EXIT, "[{shell}] 應該擋：{tool} {args:?}\nstderr: {err}");
                assert!(out.is_empty(), "[{shell}] 擋下時不能呼叫真的：{tool} {args:?}\n{out}");
                assert!(err.contains("lsof -ti :PORT") && err.contains(tool), "[{shell}] 原因要講怎麼做才對：{err}");
            }
        }
    }

    #[test]
    fn the_1884_incident_line_is_denied_and_says_what_to_do_instead() {
        let env = Env::new();
        let (code, out, err) = env.run("sh", "pkill", &["-f", "next-server", "-P", "1"]);
        assert_eq!(code, DENIED_EXIT);
        assert!(out.is_empty());
        assert!(err.contains("pkill 一律") && err.contains("lsof -ti :PORT") && err.contains("kill <自己啟動時記下的 PID>"), "{err}");
    }

    /// 政策一律擋：訊息都要給替代寫法（記下 PID、port），Rust hook 同一段話。
    #[test]
    fn denying_always_gives_the_alternative() {
        let env = Env::new();
        for (tool, args) in [("pkill", vec!["-x", "node"]), ("pkill", vec!["-f", "claude"]), ("killall", vec!["node"]), ("killall", vec!["-r", "^node$"])] {
            let (code, out, err) = env.run("sh", tool, &args);
            assert_eq!(code, DENIED_EXIT, "{tool} {args:?}");
            assert!(out.is_empty());
            assert!(err.contains("lsof -ti :PORT") && err.contains("kill <自己啟動時記下的 PID>"), "替代寫法：{tool} {args:?}\n{err}");
            let cmd = std::iter::once(tool.to_string()).chain(args.iter().map(|a| am_base::hosts::sh_quote(a))).collect::<Vec<_>>().join(" ");
            let Verdict::Deny(r) = check_command(&cmd) else { panic!("{cmd}") };
            assert!(r.contains("lsof -ti :PORT") && r.contains("kill <"), "{r}");
        }
    }

    #[test]
    fn arguments_with_spaces_and_odd_characters_reach_the_real_tool_untouched() {
        let env = Env::new();
        let args = ["-f", "--", "next start -p 3917", "a b", "$HOME", "`x`", "zz-no-such", "it's"];
        let (code, out, err) = env.run("sh", "pkill", &args);
        // 政策：pkill 一律擋，參數再怎麼特殊都一樣；不能有任何輸出（沒有真的 pkill 被呼叫）。
        assert_eq!(code, DENIED_EXIT, "{err}");
        assert!(out.is_empty(), "{out}");
    }

    #[test]
    fn the_shim_is_filled_from_the_rust_constants() {
        let sh = shim_sh();
        assert!(sh.contains(am_base::kill_guard::HOW_TO), "替代做法");
        assert!(!sh.contains("@AM_"), "還有沒代入的標記");
        assert!(sh.starts_with("#!/bin/sh\n") && sh.ends_with('\n'));
        assert!(sh.lines().take(12).any(|l| l.contains("AM_SHIM_MARKER")), "marker 要在前 12 行");
        assert!(!sh.contains("\r"));
        assert!(!sh.contains("exec \"$_real\""), "shim 一律擋，不能 exec 真的");
    }

    #[test]
    fn the_shim_is_in_the_shared_shim_list_and_install_local_is_atomic_and_idempotent() {
        let list = shims();
        assert_eq!(list.len(), 5);
        for n in NAMES {
            assert_eq!(list.iter().find(|(name, _)| *name == n).map(|(_, c)| *c), Some(shim_sh()), "{n}");
        }
        let d = tmp();
        let bot = d.join("bots/01KILL");
        let bin = install_local(&bot).unwrap();
        for n in NAMES {
            let p = bin.join(n);
            assert_eq!(std::fs::read_to_string(&p).unwrap(), shim_sh());
            assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o755);
        }
        let before = std::fs::metadata(bin.join("pkill")).unwrap().modified().unwrap();
        install_local(&bot).unwrap();
        assert_eq!(std::fs::metadata(bin.join("pkill")).unwrap().modified().unwrap(), before, "內容一樣不重寫");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn boot_refresh_writes_the_kill_shims_into_bots_that_already_have_a_bin_dir() {
        let d = tmp();
        // 已經啟動過的 bot（bin/ 裡有 herdr）：開機掃描補上 pkill／killall，正在跑的 pane 下一次呼叫就生效。
        let started = d.join("bots/01STARTED/bin");
        write_exec(&started.join("herdr"), "#!/bin/sh\n# 舊\n");
        // 從沒啟動過的 bot（沒有 bin/）：不生出新檔案。
        std::fs::create_dir_all(d.join("bots/01NEVER")).unwrap();
        let out = refresh_all(&d);
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        for n in NAMES {
            assert_eq!(std::fs::read_to_string(started.join(n)).unwrap(), shim_sh(), "{n}");
        }
        assert!(!d.join("bots/01NEVER/bin").exists());
        assert!(out.changed.contains(&"01STARTED/pkill".to_string()) && out.changed.contains(&"01STARTED/killall".to_string()), "{:?}", out.changed);
        // 冪等。
        assert!(refresh_all(&d).changed.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }
