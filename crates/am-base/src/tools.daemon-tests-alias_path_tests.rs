
    use super::*;

    fn bin(dir: &std::path::Path, name: &str, body: &str) {
        crate::testing::write_exec(dir.join(name), body);
    }

    fn sh(script: &str, shell: &std::path::Path, path: &str) -> String {
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .env("SHELL", shell)
            .env("PATH", path)
            .env("HOME", "/tmp")
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn macos_local_alias_from_the_login_shell_does_not_hide_the_binary() {
        let root = crate::testing::track(std::env::temp_dir().join(format!("am-alias-{}", crate::db::ulid())));
        let bindir = root.join("bin");
        std::fs::create_dir_all(&bindir).unwrap();
        bin(&bindir, "claude", "#!/bin/sh\necho 'claude 2.1.0'\n");
        bin(&bindir, "codex", "#!/bin/sh\necho 'codex-cli 0.120.0'\n");
        bin(&bindir, "herdr", "#!/bin/sh\necho 'herdr 0.9.1'\n");
        // `$2` is the `command -v <name>` string the probe passes to `sh -lic`.
        bin(
            &root,
            "login-sh",
            "#!/bin/sh\ncase \"$2\" in *claude*) echo \"alias claude='claude --flag'\" ;; *codex*) echo \"alias codex='codex --yolo'\" ;; *) ;; esac\n",
        );
        let path = format!("{}:/usr/bin:/bin", bindir.display());
        let shell = root.join("login-sh");
        let out = sh(PROBE_SH, &shell, &path);
        let path_of = |kind: &str| {
            out.lines()
                .find_map(|l| l.strip_prefix(&format!("AM_PATH {kind} ")))
                .unwrap_or("")
                .trim()
                .to_string()
        };
        assert_eq!(path_of("claude"), bindir.join("claude").to_string_lossy());
        assert_eq!(path_of("codex"), bindir.join("codex").to_string_lossy());
        assert!(out.contains("AM_VER claude claude 2.1.0"), "{out}");
        assert!(out.contains("AM_HERDR herdr 0.9.1"), "{out}");

        let ver = sh(
            &format!("{}; [ -n \"$p\" ] && \"$p\" --version", login_abs_sh("claude")),
            &shell,
            &path,
        );
        assert!(ver.contains("claude 2.1.0"), "changelog 探測不能去執行 alias 那行：{ver}");

        let herdr = sh(HERDR_CLI_SH, &shell, &path);
        assert!(herdr.contains("AM_HERDR herdr 0.9.1"), "{herdr}");

        // 登入 shell 給 alias、PATH 上也沒有：要空，不能把 alias 字串留著。
        let bare = crate::testing::track(std::env::temp_dir().join(format!("am-alias-bare-{}", crate::db::ulid())));
        std::fs::create_dir_all(&bare).unwrap();
        let out = sh(PROBE_SH, &shell, "/usr/bin:/bin");
        assert!(out.lines().any(|l| l.trim() == "AM_PATH claude"), "沒有執行檔就要是空路徑：{out}");
        assert!(!out.contains("alias"), "alias 字串不能出現在探測結果：{out}");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&bare);
    }
