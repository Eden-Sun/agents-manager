
    //! #709：連上共用 session 的主機時，對方的 herdr server 在跑就一點都不碰（絕不 `server stop`）。
    use super::*;

    fn fake(bin: &std::path::Path, name: &str, body: &str) {
        crate::testing::write_exec(bin.join(name), format!("#!/bin/sh\n{body}\n"));
    }

    fn generated_launchd_plist() -> (String, String, u32, u32, u32) {
        use std::os::unix::fs::MetadataExt;

        let root = std::path::PathBuf::from(format!("/tmp/am-rs-{}", &crate::db::ulid()[18..]));
        let home = root.join("home & files");
        let bin = home.join("bin & tools");
        let (log, capture) = (root.join("log"), root.join("captured.plist"));
        std::fs::create_dir_all(&bin).unwrap();
        let sock_dir = home.join(".config/herdr/sessions/test");
        std::fs::create_dir_all(&sock_dir).unwrap();
        let _sock = std::os::unix::net::UnixListener::bind(sock_dir.join("herdr.sock")).unwrap();
        fake(&bin, "herdr", r#"echo "herdr $*" >> "$AM_LOG"; [ "$1" = session ] && [ "$2" = list ] && echo "test running"; exit 0"#);
        fake(&bin, "uname", "echo Darwin");
        fake(&bin, "stat", "id -un");
        fake(
            &bin,
            "launchctl",
            r#"echo "launchctl $*" >> "$AM_LOG"; [ "$1" = print ] && exit 1; [ "$1" = bootstrap ] && cp "$3" "$AM_CAPTURE"; exit 0"#,
        );
        fake(&bin, "sleep", "exit 0");
        let cfg = HostCfg {
            name: "sh1".into(),
            ssh: "sh1.invalid".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "test".into(),
            remote_path: String::new(),
            shared_session: false,
        };
        let command = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("umask 000; {}", HostConn::remote_session_script(&cfg, false)))
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("HOME", &home)
            .env("AM_LOG", &log)
            .env("AM_CAPTURE", &capture)
            .output()
            .unwrap();
        assert!(command.status.success(), "{}", String::from_utf8_lossy(&command.stderr));
        let plist = std::fs::read_to_string(&capture).unwrap();
        let meta = std::fs::metadata(&capture).unwrap();
        let dir_mode = std::fs::metadata(home.join("Library/LaunchAgents")).unwrap().mode() & 0o777;
        let bin_path = bin.join("herdr").to_string_lossy().into_owned();
        let mode = meta.mode() & 0o777;
        let uid = meta.uid();
        std::fs::remove_dir_all(&root).ok();
        (plist, bin_path, mode, dir_mode, uid)
    }

    /// 假的 macOS：herdr 說 session 在跑（nohup 起的，launchd 沒有它）——正是以前會 `server stop` 交給 launchd 的形狀。
    fn run(shared: bool) -> (String, String) {
        let root = std::path::PathBuf::from(format!("/tmp/am-rs-{}", &crate::db::ulid()[18..]));
        let (bin, home, log) = (root.join("bin"), root.join("home"), root.join("log"));
        std::fs::create_dir_all(&bin).unwrap();
        let sock_dir = home.join(".config/herdr/sessions/test");
        std::fs::create_dir_all(&sock_dir).unwrap();
        let _sock = std::os::unix::net::UnixListener::bind(sock_dir.join("herdr.sock")).unwrap();
        fake(&bin, "herdr", r#"echo "herdr $*" >> "$AM_LOG"; [ "$1" = session ] && [ "$2" = list ] && echo "test running"; exit 0"#);
        fake(&bin, "uname", "echo Darwin");
        fake(&bin, "stat", "id -un");
        fake(&bin, "launchctl", r#"echo "launchctl $*" >> "$AM_LOG"; [ "$1" = print ] && exit 1; exit 0"#);
        fake(&bin, "sleep", "exit 0");
        let cfg = HostCfg {
            name: "sh1".into(),
            ssh: "sh1.invalid".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "test".into(),
            remote_path: String::new(),
            shared_session: shared,
        };
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(HostConn::remote_session_script(&cfg, shared))
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
    fn a_running_shared_session_is_never_stopped_or_handed_to_launchd() {
        let (out, calls) = run(true);
        assert!(out.contains("AM_MODE=shared-running") && out.contains("AM_OK=1"), "{out}");
        assert!(!calls.contains("server stop"), "{calls}");
        assert!(!calls.contains("launchctl bootstrap"), "{calls}");

        let (out, calls) = run(false);
        assert!(calls.contains("server stop"), "不共用時照舊交給 launchd（對照組）：{calls}\n{out}");
    }

    #[test]
    fn macos_local_generated_launchd_plist_xml_escapes_paths_with_xml_metacharacters() {
        let (plist, bin_path, _, _, _) = generated_launchd_plist();
        let escaped = bin_path.replace('&', "&amp;");
        assert!(plist.contains(&escaped), "plist did not XML-escape the executable path: {plist}");
        assert!(!plist.contains(&bin_path), "plist still contains the unescaped executable path: {plist}");
    }

    #[test]
    fn macos_local_generated_launchd_plist_and_directory_are_private_under_a_permissive_umask() {
        let (_, _, file_mode, dir_mode, owner) = generated_launchd_plist();
        assert_eq!(owner, unsafe { libc::geteuid() }, "launch agent plist must belong to the SSH user");
        assert_eq!(file_mode & 0o077, 0, "launch agent plist must not be group/world accessible");
        assert_eq!(dir_mode & 0o022, 0, "new LaunchAgents directory must not be group/world writable");
    }
