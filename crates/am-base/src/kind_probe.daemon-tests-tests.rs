
    use super::*;
    use crate::lifecycle::{start_bot, LcError};
    use crate::testing::{claude_bot, env, Env};
    use std::path::Path;

    /// 不走登入 shell（`SHELL` 指到不存在的東西；登入 shell 會把 PATH 換成這台機器的），PATH 只有 `bin` 與系統目錄。
    fn sh_runner(bin: &Path) -> Arc<Runner> {
        let path = format!("{}:/usr/bin:/bin", bin.display());
        Arc::new(move |_host, _kind, probe| {
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(probe)
                .env_clear()
                .env("PATH", &path)
                .env("SHELL", "/nonexistent-shell")
                .output()
                .ok()?;
            Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
        })
    }

    fn stub(bin: &Path, name: &str) {
        std::fs::create_dir_all(bin).unwrap();
        let f = bin.join(name);
        crate::testing::write_exec(&f, "#!/bin/sh\nexit 0\n");
    }

    fn agent_started(e: &Env) -> bool {
        e.herdr.methods().iter().any(|m| m == "agent.start")
    }

    /// PATH 上（這裡是測試自己的 `bin/`）找得到＝放行，而且真的走到 agent.start。
    #[tokio::test]
    async fn a_cli_that_is_on_the_path_passes_the_preflight() {
        let e = env().await;
        let bin = e.dir.join("bin");
        stub(&bin, "claude");
        e.app.kind_probe.set(sh_runner(&bin));
        let bot = claude_bot(&e.app, &e.project_id, "pm").await;
        start_bot(&e.app, &bot.id).await.expect("stub 在 PATH 上，preflight 要過");
        assert!(agent_started(&e));
    }

    /// 真的缺 CLI：start 要拒絕、講清楚缺什麼，而且沒有碰 herdr——不是讓它在 launch_pending 裡默默等 60 秒。
    /// 只有別的 agent 的執行檔（codex）不算數。
    #[tokio::test]
    async fn a_missing_cli_refuses_the_start_and_says_which_one() {
        let e = env().await;
        let bin = e.dir.join("bin");
        stub(&bin, "codex");
        e.app.kind_probe.set(sh_runner(&bin));
        let bot = claude_bot(&e.app, &e.project_id, "pm").await;

        let err = start_bot(&e.app, &bot.id).await.expect_err("PATH 上沒有 claude：不能啟動");
        let LcError::Bad(reason) = err else { panic!("expected LcError::Bad, got {err:?}") };
        assert!(reason.contains("本機上找不到 `claude` 執行檔"), "{reason}");
        assert!(!agent_started(&e), "沒有執行檔就不該碰 herdr 開 agent：{:?}", e.herdr.methods());

        let conv = crate::db::conversation_id(&e.app.db, &bot.id).await.unwrap();
        let note: String = sqlx::query_scalar(
            "SELECT content FROM messages WHERE conversation_id=? AND role='system' ORDER BY created_at DESC LIMIT 1",
        )
        .bind(&conv)
        .fetch_one(&e.app.db)
        .await
        .unwrap();
        assert_eq!(note, reason, "聊天室裡要看得到同一句原因");
    }

    /// 查不了（探測本身跑不起來）不擋 start：這一步只是提早報錯，不是關卡。
    #[tokio::test]
    async fn a_probe_that_cannot_run_does_not_block_the_start() {
        let e = env().await;
        e.app.kind_probe.set(Arc::new(|_host, _kind, _probe| None));
        let bot = claude_bot(&e.app, &e.project_id, "pm").await;
        start_bot(&e.app, &bot.id).await.expect("查不了就放行");
        assert!(agent_started(&e));
    }

    #[test]
    fn the_reason_names_the_machine_that_lacks_the_cli() {
        assert_eq!(verdict("local", "claude", Some("/usr/bin/claude".into())), Ok(()));
        assert_eq!(verdict("box", "claude", None), Ok(()));
        let local = verdict("local", "codex", Some(String::new())).unwrap_err();
        assert!(local.starts_with("本機上找不到 `codex` 執行檔"), "{local}");
        let remote = verdict("box", "grok", Some(String::new())).unwrap_err();
        assert!(remote.starts_with("主機 box上找不到 `grok` 執行檔"), "{remote}");
    }

    /// 測試 build 的預設 runner 不看機器：沒有任何 stub、也不必裝任何 CLI，每個 kind 都答「有」。
    /// （這是 #139 的根本：21 條 start_bot 測試在沒裝 claude／codex 的外部編譯主機上必紅。）
    #[tokio::test]
    async fn the_test_default_does_not_depend_on_what_the_machine_has_installed() {
        let e = env().await;
        let run = e.app.kind_probe.get().expect("測試 build 預設就有假的 runner");
        for kind in crate::config::KINDS {
            let found = run("local", kind, &probe_command(kind));
            assert!(matches!(&found, Some(p) if !p.is_empty()), "{kind}: {found:?}");
        }
        let bot = claude_bot(&e.app, &e.project_id, "pm").await;
        start_bot(&e.app, &bot.id).await.expect("預設就過 preflight");
    }
