
    use super::*;
    use crate::runners::pane_identity::{ProcEnv, ProcEnvHook, PsProcEnv, sync_child_identity};
    use crate::state::App;
    use futures::future::BoxFuture;
    use std::sync::Arc;
    use crate::config::IdentityCfg;

    struct FixedProcEnv {
        reads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        env: BTreeMap<String, String>,
    }

    impl ProcEnv for FixedProcEnv {
        fn env_of<'a>(

            &'a self,
            _app: &'a Arc<App>,
            _host: &'a str,
            _pid: i64,
        ) -> BoxFuture<'a, Option<BTreeMap<String, String>>> {
            Box::pin(async move {
                self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Some(self.env.clone())
            })
        }
    }

    fn idn(name: &str, kind: &str, dir: Option<&str>) -> IdentityCfg {
        let mut env = BTreeMap::new();
        if let Some(d) = dir {
            env.insert(config_dir_var(kind).unwrap().to_string(), d.to_string());
        }
        IdentityCfg { name: name.into(), kind: kind.into(), host: None, env, args: vec![] }
    }

    #[test]
    fn the_environment_is_read_out_of_a_ps_line_and_the_command_line_is_not() {
        let out = "  PID   TT  STAT      TIME COMMAND\n 4924 s026  S+     0:10.72 claude --model opus \
                   --settings=NOPE=1 AM_KIND=claude CLAUDE_CONFIG_DIR=/Users/m4p/.claude-cc2 PATH=/usr/bin\n";
        let env = parse_ps_env(out);
        assert_eq!(env.get("CLAUDE_CONFIG_DIR").map(String::as_str), Some("/Users/m4p/.claude-cc2"));
        assert_eq!(env.get("AM_KIND").map(String::as_str), Some("claude"));
        assert!(!env.contains_key("--settings"), "a flag that happens to hold `=` is not an environment variable");
    }

    #[test]
    fn a_trailing_slash_or_a_private_prefix_is_the_same_directory() {
        assert_eq!(norm_dir("/tmp/x/"), norm_dir("/private/tmp/x"));
        assert_eq!(norm_dir("/Users/m4p/.claude-cc2//"), "/Users/m4p/.claude-cc2");
    }

    #[test]
    fn the_account_directory_names_the_identity_it_belongs_to() {
        let ids = [idn("cc1", "claude", Some("$HOME/.claude-ccompany")), idn("cc2", "claude", Some("~/.claude-cc2"))];
        let var = "CLAUDE_CONFIG_DIR";
        assert_eq!(identity_named(&ids, "claude", var, "/Users/m4p", "/Users/m4p/.claude-cc2/"), Some("cc2".into()));
        assert_eq!(identity_named(&ids, "claude", var, "/Users/m4p", "/Users/m4p/.claude-ccompany"), Some("cc1".into()));
        // Unclaimed dir / wrong kind: no answer, caller keeps the inherited value.
        assert_eq!(identity_named(&ids, "claude", var, "/Users/m4p", "/Users/m4p/.claude-other"), None);
        assert_eq!(identity_named(&ids, "codex", "CODEX_HOME", "/Users/m4p", "/Users/m4p/.claude-cc2"), None);
    }

    /// m4p 2026-09-11: children of a pane exporting `CLAUDE_CONFIG_DIR=~/.claude` showed `cc1`, not `cc0`.
    #[test]
    fn the_default_account_directory_or_no_variable_is_the_empty_env_identity() {
        let var = "CLAUDE_CONFIG_DIR";
        let ids = [idn("cc0", "claude", None), idn("cc1", "claude", Some("$HOME/.claude-ccompany"))];
        let h = "/Users/m4p";
        assert_eq!(child_identity(&ids, "claude", var, h, Some("/Users/m4p/.claude")), Some("cc0".into()));
        assert_eq!(child_identity(&ids, "claude", var, h, Some("/Users/m4p/.claude/")), Some("cc0".into()));
        assert_eq!(child_identity(&ids, "claude", var, h, None), Some("cc0".into()));
        assert_eq!(child_identity(&ids, "claude", var, h, Some("/Users/m4p/.claude-ccompany")), Some("cc1".into()));
        assert_eq!(child_identity(&ids, "claude", var, h, Some("/Users/m4p/.claude-other")), None);
        assert_eq!(child_identity(&ids[1..], "claude", var, h, None), None);
    }

    #[tokio::test]
    async fn child_identity_waits_for_remote_home_and_retries_after_recovery() {
        use std::sync::atomic::Ordering;

        let env = crate::testing::env().await;
        let app = env.app.clone();
        let host = format!("child-home-616-{}", crate::db::ulid().to_ascii_lowercase());
        let conn = app.hosts.insert_remote_for_test(crate::config::HostCfg {
            shared_session: false,
            name: host.clone(),
            ssh: "unused".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        }).await;
        let bot = crate::testing::claude_bot(&app, &env.project_id, "child-home").await;
        sqlx::query("UPDATE bots SET managed_by='child', identity='cc1' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        let bot = crate::db::bot(&app.db, &bot.id).await.unwrap().unwrap();
        app.cfg.update(|cfg| {
            cfg.identities = vec![
                idn("cc1", "claude", Some("~/.claude-cc1")),
                idn("cc2", "claude", Some("~/.claude-cc2")),
            ].into_iter().map(|mut identity| { identity.host = Some(host.clone()); identity }).collect();
            Ok(())
        }).await.unwrap();
        let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        app.proc_env.set(std::sync::Arc::new(FixedProcEnv {
            reads: reads.clone(),
            env: [("CLAUDE_CONFIG_DIR".into(), "/home/remote-child/.claude-cc2".into())].into(),
        }));
        crate::hosts::set_ssh_fake(&host, |_| Err(anyhow::anyhow!("injected remote HOME read failure")));

        sync_child_identity(&app, &host, &bot, "w1:p1", Some(901)).await;
        let unchanged = crate::db::bot(&app.db, &bot.id).await.unwrap().unwrap();
        assert_eq!(unchanged.identity.as_deref(), Some("cc1"), "unreadable HOME leaves the inherited identity alone");
        assert!(probe_due(&bot.id, "w1:p1"), "HOME failure is retryable, not marked as probed");
        assert_eq!(reads.load(Ordering::SeqCst), 0, "do not read or interpret the pane env without its host HOME");

        *conn.remote_home.lock().await = Some("/home/remote-child".into());
        sync_child_identity(&app, &host, &bot, "w1:p1", Some(901)).await;
        let recovered = crate::db::bot(&app.db, &bot.id).await.unwrap().unwrap();
        assert_eq!(recovered.identity.as_deref(), Some("cc2"), "later pass resolves the child's identity under remote HOME");
        assert!(!probe_due(&bot.id, "w1:p1"));
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    /// Linux procps 的 `ps eww -p`（BSD 的 `e` 混 SysV 的 `-p`）也要吐出環境：child 的身分就靠它認
    /// （SPEC「Linux 主機」）。真的起一個行程，外部編譯主機會跑到。
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_real_ps_eww_shows_the_environment_of_our_own_process() {
        let mut child = std::process::Command::new("sleep").arg("30").env("CLAUDE_CONFIG_DIR", "/home/u/.claude-cc2").spawn().unwrap();
        let out = std::process::Command::new("/bin/sh").arg("-c").arg(ps_cmd(i64::from(child.id()))).output().unwrap();
        let _ = child.kill();
        let _ = child.wait();
        let env = parse_ps_env(&String::from_utf8_lossy(&out.stdout));
        assert_eq!(env.get("CLAUDE_CONFIG_DIR").map(String::as_str), Some("/home/u/.claude-cc2"), "{}", String::from_utf8_lossy(&out.stdout));
    }
