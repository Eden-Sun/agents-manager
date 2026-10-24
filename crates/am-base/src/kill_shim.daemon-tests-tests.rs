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

    /// (工具, 參數, 是否應該擋)。同一張表同時餵 Rust 的 `kill_guard` 和 sh shim，兩邊都要等於「期待」。
    fn cases() -> Vec<(&'static str, Vec<&'static str>, bool)> {
        let long_pattern: &'static str = Box::leak(format!("claude{}", "x".repeat(300)).into_boxed_str());
        let nine_deep: &'static str = Box::leak(format!("{}claude{}", "(".repeat(9), ")".repeat(9)).into_boxed_str());
        let eight_deep: &'static str = Box::leak(format!("{}claude{}", "(".repeat(8), ")".repeat(8)).into_boxed_str());
        let too_long_regex: &'static str = Box::leak(format!("claude.{}", "a?".repeat(130)).into_boxed_str());
        let mut v: Vec<(&str, Vec<&str>, bool)> = Vec::new();
        let mut add = |tool: &'static str, args: &[&'static str], deny: bool| v.push((tool, args.to_vec(), deny));
        // ── 正常用法：放行 ──
        add("pkill", &["-f", "next start -p 3917"], false);
        add("pkill", &["-f", "next-server"], false);
        add("pkill", &["-TERM", "-f", "vite --port 5173"], false);
        add("pkill", &["-9", "-f", "next-server"], false);
        add("pkill", &["-P", "1", "-f", "next-server"], false);
        add("pkill", &["-U", "1000", "-f", "next-server"], false);
        add("pkill", &["-f", "--", "-weird-name"], false);
        add("pkill", &["-f", "vite --port 5173"], false);
        add("pkill", &["-x", "vite"], false);
        add("pkill", &["-x", "ab"], false);
        add("pkill", &["-P", "4242"], false);
        add("pkill", &["-f", "python3 -m http.server 8000"], false);
        add("pkill", &["-f", "(.*)*Z"], false);
        // 可判斷語法以外（shim 的 grep 與 Rust 的迷你 ERE 語意不同的地方）：兩邊一律「判不出來」→ 放行。
        for pat in ["claude**", "claude{0}", "claude{", "claude}", "claude|", "|claude", "(claude", "claude)", "()claude", "[claude", "claude]", "*claude", "+claude", "^*claude", "claude$x", "x^claude", "cla\\dde", "cla\\bde", "cl[a-c]ude", "cl[[:alpha:]]ude", "cl[]a]ude", "cl[a-]ude", "cl[^]ude", "cla\\ude", "cla\\", "cla\tde", "ééé", "claude\n"] {
            add("pkill", &["-f", pat], false);
            add("pkill", &[pat], false);
            add("killall", &["-r", pat], false);
        }
        add("pkill", &["-f", nine_deep], false);
        add("pkill", &["-f", too_long_regex], false);
        add("pkill", &["é"], true); // 位元組數不到 3：兩邊都當太短
        add("pkill", &["-x", "ééé"], false);
        add("pkill", &["-f", long_pattern], false);
        add("pkill", &[], false);
        add("killall", &["Chrome"], false);
        add("killall", &["vite"], false);
        add("killall", &["-s", "TERM", "next-server"], false);
        add("killall", &["-9", "chromedriver"], false);
        add("killall", &["-m", "next-server"], false);
        add("killall", &["-m", "vite.*"], false);
        add("killall", &[], false);
        // ── 確定廣域：擋 ──
        add("pkill", &["-f", "next-server", "-P", "1"], true); // 事故那行（-P 1 在 BSD 是 pattern）
        add("pkill", &["next-server", "-P", "1"], true);
        add("pkill", &["-f", "next-server", "-9"], true);
        add("pkill", &["-f", "next-server", "-u", "eddie"], true);
        add("pkill", &["-f", "next-server", "-U", "1000"], true);
        add("pkill", &["-f", "x", "--", "-y"], true);
        add("pkill", &["-f", "claude"], true);
        add("pkill", &["claude"], true);
        add("pkill", &["-f", "codex"], true);
        add("pkill", &["-f", "agents-managerd serve"], true);
        add("pkill", &["herdr"], true);
        add("pkill", &["-f", "cla.de"], true);
        add("pkill", &["-f", "^claude"], true);
        add("pkill", &["-f", "cla(u|a)de"], true);
        add("pkill", &["-f", "bots/01M"], true);
        // 可判斷語法以內：兩邊語意相同，照擋。
        for pat in ["cl[a-z]ude+", "cl[^0-9]ude", "cl[ade]*u", "^cla", "cla.de|zzz", "^(claude|codex)", "(cla)(ude)?", "cla(u|a)de", "c.*e", "claude.?", "agents-managerd", "\\.*claude", "(.*)*claude", "((((((((claude))))))))", eight_deep, "claude.*settings", "zzz|claude", "codex|^zzz"] {
            add("pkill", &["-f", pat], true);
        }
        add("pkill", &["claude$"], true);
        add("pkill", &["(codex|grok)$"], true);
        add("pkill", &["-f", "claude$"], false); // -f 比整條命令列，樣本沒有以 claude 結尾的
        add("pkill", &["-x", "cla.de"], true);
        add("pkill", &["-ix", "CLAUDE"], true);
        add("pkill", &["-f", "zzz.*claude"], false);
        add("pkill", &["-f", "cla(ud)+x"], false);
        add("killall", &["-r", "cl[a-z]ude"], true);
        add("killall", &["-r", "^claude$"], true);
        add("killall", &["-r", "^(codex|grok)$"], true);
        add("pkill", &["-f", "node"], true);
        add("pkill", &["-x", "node"], true); // node 是 npm 安裝版 claude／codex 的行程名
        add("pkill", &["-x", "bash"], true);
        add("pkill", &["-i", "CLAUDE"], true);
        add("pkill", &["-f", "--", "claude"], true);
        add("pkill", &["-f", "1"], true);
        add("pkill", &["-f", "."], true);
        add("pkill", &[""], true);
        add("pkill", &["-u", "eddie"], true);
        add("pkill", &["-U", "1000"], true);
        add("pkill", &["-G", "staff"], true);
        add("pkill", &["-P", "1"], true);
        add("pkill", &["-P", "0"], true);
        add("pkill", &["--parent", "1"], true);
        add("pkill", &["--uid=1000"], true);
        add("killall", &["node"], true);
        add("killall", &["claude"], true);
        add("killall", &["-9", "codex"], true);
        add("killall", &["-KILL", "node"], true);
        add("killall", &["-I", "NODE"], true);
        add("killall", &["-u", "eddie"], true);
        add("killall", &["-m", ".*"], true);
        add("killall", &["-r", ".*"], true);
        add("killall", &["-r", "cla.de"], true);
        add("killall", &["-s", "KILL", "node"], true);
        // 一條指令最多對 16 個 pattern 做 regex 比對：第 17 個起判不出來→放行；前 16 個內的照擋。
        let benign: Vec<&'static str> = (0..16).map(|i| &*Box::leak(format!("zz{i}x").into_boxed_str())).collect();
        let mut late = vec!["-f"];
        late.extend(benign.iter().copied());
        late.push("claude");
        let late: &'static [&'static str] = Box::leak(late.into_boxed_slice());
        let mut early = vec!["-f", "claude"];
        early.extend(benign.iter().copied());
        let early: &'static [&'static str] = Box::leak(early.into_boxed_slice());
        add("pkill", late, false);
        add("pkill", early, true);
        v
    }

    #[test]
    fn the_shim_and_the_rust_guard_agree_on_every_case_under_every_shell() {
        let env = Env::new();
        let mut checked = 0;
        for shell in shells() {
            for (tool, args, deny) in cases() {
                let rust = rust_denies(tool, &args);
                assert_eq!(rust, deny, "kill_guard：{tool} {args:?}");
                let (code, out, err) = env.run(shell, tool, &args);
                if deny {
                    assert_eq!(code, DENIED_EXIT, "[{shell}] 應該擋：{tool} {args:?}\nstderr: {err}");
                    assert!(out.is_empty(), "[{shell}] 擋下時不能呼叫真的：{tool} {args:?}\n{out}");
                    assert!(err.contains("lsof -ti :PORT") && err.contains(tool), "[{shell}] 原因要講怎麼做才對：{err}");
                } else {
                    assert_eq!(code, 0, "[{shell}] 應該放行：{tool} {args:?}\nstderr: {err}");
                    let want: String = args.iter().map(|a| format!("{a}\n")).collect();
                    assert_eq!(out, want, "[{shell}] 放行時參數要原封不動交給真的：{tool} {args:?}");
                }
                checked += 1;
            }
        }
        assert!(checked >= 60, "{checked}");
    }

    #[test]
    fn the_1884_incident_line_is_denied_with_the_bsd_explanation() {
        let env = Env::new();
        let (code, out, err) = env.run("sh", "pkill", &["-f", "next-server", "-P", "1"]);
        assert_eq!(code, DENIED_EXIT);
        assert!(out.is_empty());
        assert!(err.contains("-P") && err.contains("BSD") && err.contains("#1884") && err.contains("kill <自己啟動時記下的 PID>"), "{err}");
    }

    #[test]
    fn it_never_recurses_into_itself_or_another_bots_shim() {
        let env = Env::new();
        // 另一顆 bot 的 shim 目錄排在真的前面：不能當成真的 pkill（否則 shim → shim → …）。
        let other = env.root.join("bots/01OTHER/bin");
        for n in NAMES {
            write_exec(&other.join(n), shim_sh());
        }
        // 就算是檔頭沒有 marker 的舊版 shim，只要在 `bots/<id>/bin` 底下也跳過。
        let old = env.root.join("bots/01OLD/bin");
        write_exec(&old.join("pkill"), "#!/bin/sh\necho OLD-SHIM\n");
        let path = format!("{}:{}:{}:{}:/usr/bin:/bin", old.display(), other.display(), env.root.join("shim").display(), env.root.join("real").display());
        let out = Command::new("sh").arg(env.root.join("shim/pkill")).args(["-f", "next-server"]).env("PATH", path).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(String::from_utf8_lossy(&out.stdout), "-f\nnext-server\n", "要走到 real/pkill，不是舊 shim 或別顆 bot 的 shim");
        // marker 認得出不在 bot 目錄的另一份拷貝。
        let copy = env.root.join("elsewhere");
        write_exec(&copy.join("pkill"), shim_sh());
        let path = format!("{}:{}:{}:/usr/bin:/bin", copy.display(), env.root.join("shim").display(), env.root.join("real").display());
        let out = Command::new("/bin/sh").arg(env.root.join("shim/pkill")).args(["-x", "vite"]).env("PATH", path).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "-x\nvite\n");
    }

    #[test]
    fn without_a_real_tool_it_says_so_and_exits_127() {
        let env = Env::new();
        std::fs::remove_file(env.root.join("real/pkill")).unwrap();
        let path = format!("{}:/usr/bin:/bin", env.root.join("shim").display());
        // 標準位置的 fallback 若剛好有真的 pkill，shim 就會 exec 它——測試不能真的去殺東西，所以只在這台沒有時才跑這一段。
        let has_system = ["/usr/bin/pkill", "/bin/pkill", "/usr/sbin/pkill", "/sbin/pkill"].iter().any(|p| Path::new(p).exists());
        if !has_system {
            let out = Command::new("/bin/sh").arg(env.root.join("shim/pkill")).args(["-f", "next-server"]).env("PATH", &path).output().unwrap();
            assert_eq!(out.status.code(), Some(127));
            assert!(String::from_utf8_lossy(&out.stderr).contains("找不到真的 pkill"));
        }
        // 擋的判斷在找真的之前（沒有真的也一樣擋）由 parity 測試（stub 當真的）涵蓋；這裡不再跑會真的 exec /usr/bin/pkill 的擋案例，
        // 免得 shim 哪天回歸就真的去殺 claude。
        let _ = path;
    }

    /// `pkill -x node`／`pkill -f node`／`killall node` 被擋時，訊息要說明原因（node 也是 claude／codex 的行程名）並直接給替代寫法。
    #[test]
    fn denying_node_explains_why_and_gives_the_alternative() {
        let env = Env::new();
        for (tool, args) in [("pkill", vec!["-x", "node"]), ("pkill", vec!["-f", "node"]), ("killall", vec!["node"]), ("killall", vec!["-r", "^node$"])] {
            let (code, out, err) = env.run("sh", tool, &args);
            assert_eq!(code, DENIED_EXIT, "{tool} {args:?}");
            assert!(out.is_empty());
            assert!(err.contains("也是 claude／codex") && err.contains("行程名"), "原因：{tool} {args:?}\n{err}");
            assert!(err.contains("lsof -ti :PORT | xargs kill") && err.contains("kill <PID>"), "替代寫法：{tool} {args:?}\n{err}");
            // Rust hook 同一段話。
            let cmd = std::iter::once(tool.to_string()).chain(args.iter().map(|a| am_base::hosts::sh_quote(a))).collect::<Vec<_>>().join(" ");
            let Verdict::Deny(r) = check_command(&cmd) else { panic!("{cmd}") };
            assert!(r.contains("也是 claude／codex") && r.contains("lsof -ti :PORT | xargs kill") && r.contains("kill <PID>"), "{r}");
        }
        // 別的名字不帶這段。
        let (_, _, err) = env.run("sh", "pkill", &["claude"]);
        assert!(!err.contains("也是 claude／codex"), "{err}");
    }

    #[test]
    fn arguments_with_spaces_and_odd_characters_reach_the_real_tool_untouched() {
        let env = Env::new();
        let args = ["-f", "--", "next start -p 3917", "a b", "$HOME", "`x`", "zz-no-such", "it's"];
        let (code, out, err) = env.run("sh", "pkill", &args);
        // `-- a b …` 之後全是 pattern、`a b` 等都不是受保護名單：放行。
        assert_eq!(code, 0, "{err}");
        let want: String = args.iter().map(|a| format!("{a}\n")).collect();
        assert_eq!(out, want);
    }

    #[test]
    fn the_shim_is_filled_from_the_rust_constants() {
        let sh = shim_sh();
        for n in am_base::kill_guard::PROTECTED_NAMES {
            assert!(sh.contains(n), "{n}");
        }
        for c in am_base::kill_guard::PROTECTED_CMDLINES {
            assert!(sh.contains(c), "{c}");
        }
        for s in am_base::kill_guard::SIGNAL_NAMES {
            assert!(sh.contains(s), "{s}");
        }
        assert!(!sh.contains("@AM_"), "還有沒代入的標記");
        assert!(sh.starts_with("#!/bin/sh\n") && sh.ends_with('\n'));
        assert!(sh.lines().take(12).any(|l| l.contains("AM_SHIM_MARKER")), "marker 要在前 12 行（am_is_shim 只看檔頭）");
        assert!(!sh.contains("\r"));
    }

    #[test]
    fn the_shim_is_in_the_shared_shim_list_and_install_local_is_atomic_and_idempotent() {
        let list = shims();
        assert_eq!(list.len(), 4);
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
