
    //! issue #677：遠端是 Linux、裝了 `herdr@.service` 就交給 systemd 看管；沒裝或起不來照舊 nohup；
    //! server 已經在跑（不管誰起的）一律不動。
    use super::*;

    fn fake(bin: &std::path::Path, name: &str, body: &str) {
        crate::testing::write_exec(bin.join(name), format!("#!/bin/sh\n{body}\n"));
    }

    /// `unit`：`systemctl --user cat herdr@test.service` 找得到；`start_ok`：start 成功（會把 server 起來）；
    /// `running`：一開始 server 就在跑。回傳 (stdout, 呼叫紀錄)。
    fn run(unit: bool, start_ok: bool, running: bool) -> (String, String) {
        let root = std::path::PathBuf::from(format!("/tmp/am-rl-{}", &crate::db::ulid()[18..]));
        let (bin, home, log) = (root.join("bin"), root.join("home"), root.join("log"));
        std::fs::create_dir_all(&bin).unwrap();
        let sock_dir = home.join(".config/herdr/sessions/test");
        std::fs::create_dir_all(&sock_dir).unwrap();
        let _sock = std::os::unix::net::UnixListener::bind(sock_dir.join("herdr.sock")).unwrap();
        let up = root.join("up");
        if running {
            std::fs::write(&up, "").unwrap();
        }
        let up = up.display();
        fake(
            &bin,
            "herdr",
            &format!(
                r#"echo "herdr $*" >> "$AM_LOG"
[ "$1" = session ] && [ "$2" = list ] && [ -f {up} ] && echo "test running"
[ "$3" = server ] && touch {up}
exit 0"#
            ),
        );
        fake(&bin, "uname", "echo Linux");
        fake(&bin, "stat", "exit 1");
        fake(
            &bin,
            "systemctl",
            &format!(
                r#"echo "systemctl $* XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR" >> "$AM_LOG"
case "$2" in
  cat) exit {cat} ;;
  start) [ {start} = 0 ] && touch {up}; [ {start} = 0 ] || echo "Failed to start herdr@test.service" >&2; exit {start} ;;
esac
exit 0"#,
                cat = if unit { 0 } else { 1 },
                start = if start_ok { 0 } else { 1 },
            ),
        );
        // nohup 分支是背景起的：真的睡一下，等待迴圈才不會在假 herdr 起來之前就跑完（最多 20 × 50ms）。
        fake(&bin, "sleep", "/bin/sleep 0.05");
        let cfg = HostCfg {
            name: "lx1".into(),
            ssh: "lx1.invalid".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "test".into(),
            remote_path: String::new(),
            shared_session: false,
        };
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(HostConn::remote_session_script(&cfg, false))
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("HOME", &home)
            .env("AM_LOG", &log)
            .output()
            .unwrap();
        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        std::fs::remove_dir_all(&root).ok();
        (String::from_utf8_lossy(&out.stdout).to_string(), calls)
    }

    #[test]
    fn an_installed_unit_owns_the_server() {
        let (out, calls) = run(true, true, false);
        assert!(out.contains("AM_MODE=systemd\n") && out.contains("AM_OK=1"), "{out}");
        assert!(calls.contains("systemctl --user start herdr@test.service"), "{calls}");
        // 沒有登入 session 的 ssh 也連得到 user bus。
        assert!(calls.contains("XDG_RUNTIME_DIR=/run/user/"), "{calls}");
        assert!(!calls.contains("--session test server"), "unit 起了就不能再 nohup 一顆：{calls}");
    }

    #[test]
    fn without_the_unit_or_when_it_fails_the_server_is_started_the_old_way() {
        let (out, calls) = run(false, true, false);
        assert!(out.contains("AM_MODE=nohup") && out.contains("AM_OK=1"), "{out}");
        assert!(!calls.contains("--user start"), "{calls}");
        assert!(calls.contains("herdr --session test server"), "{calls}");

        let (out, calls) = run(true, false, false);
        assert!(out.contains("AM_MODE=nohup-fallback systemctl: Failed to start herdr@test.service"), "{out}");
        assert!(out.contains("AM_OK=1"), "{out}");
        assert!(calls.contains("herdr --session test server"), "{calls}");
    }

    #[test]
    fn a_running_server_is_left_alone() {
        let (out, calls) = run(true, true, true);
        assert!(out.contains("AM_MODE=systemd-existing") && out.contains("AM_OK=1"), "{out}");
        assert!(!calls.contains("--user start") && !calls.contains("server stop") && !calls.contains("test server"), "{calls}");
    }
