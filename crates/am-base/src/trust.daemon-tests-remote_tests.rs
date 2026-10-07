
    use super::*;
    use crate::testing as tt;
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const CWD: &str = "/home/ubuntu/zz-proj";

    fn run_sh(script: &str) -> Result<String> {
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-s")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        child.stdin.take().unwrap().write_all(script.as_bytes())?;
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!("sh failed: {}", String::from_utf8_lossy(&out.stderr));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// 遠端主機＋兩個只在那台的 claude 身分（各自的 `CLAUDE_CONFIG_DIR`）。ssh 假貨以主機名為鍵、是全域的：
    /// 每個測試用自己的主機名，平行跑才不會互蓋。
    async fn remote_env(host: &'static str) -> (tt::Env, PathBuf) {
        let env = tt::env().await;
        let home = env.dir.join("remote-home");
        std::fs::create_dir_all(&home).unwrap();
        let cfg = crate::config::HostCfg {
            shared_session: false,
            name: host.into(),
            ssh: host.into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        };
        let conn = env.app.hosts.insert_remote_for_test(cfg).await;
        *conn.remote_home.lock().await = Some(home.to_string_lossy().into_owned());
        env.app
            .cfg
            .update(|c| {
                for n in ["ra", "rb"] {
                    c.identities.push(crate::config::IdentityCfg {
                        name: n.into(),
                        kind: "claude".into(),
                        host: Some(host.into()),
                        env: [("CLAUDE_CONFIG_DIR".to_string(), format!("$HOME/.claude-{n}"))].into(),
                        args: vec![],
                    });
                }
                Ok(())
            })
            .await
            .unwrap();
        (env, home)
    }

    async fn bot_on(env: &tt::Env, identity: &str) -> db::Bot {
        let bot = tt::claude_bot(&env.app, &env.project_id, "remote").await;
        switch(env, &bot.id, identity).await
    }

    async fn switch(env: &tt::Env, bot_id: &str, identity: &str) -> db::Bot {
        let bot = db::bot(&env.app.db, bot_id).await.unwrap().unwrap();
        sqlx::query("UPDATE bots SET identity = ? WHERE id = ?").bind(identity).bind(&bot.id).execute(&env.app.db).await.unwrap();
        db::bot(&env.app.db, &bot.id).await.unwrap().unwrap()
    }

    fn trusted(store: &Path) -> Value {
        let v: Value = serde_json::from_str(&std::fs::read_to_string(store).unwrap()).unwrap();
        v["projects"][CWD][CLAUDE_KEY].clone()
    }

    /// 換身分＝換到一個從沒用過（目錄都還沒有）的設定目錄：照樣寫進**那個**目錄的 `.claude.json`。
    #[tokio::test]
    async fn switching_a_remote_bot_to_a_never_used_identity_pre_trusts_its_config_dir() {
        const HOST: &str = "trustbox-switch";
        let (env, home) = remote_env(HOST).await;
        crate::hosts::set_ssh_fake(HOST, run_sh);

        let bot = bot_on(&env, "ra").await;
        assert!(pretrust_for_start(&env.app, &bot, HOST, CWD).await.is_empty());
        assert_eq!(trusted(&home.join(".claude-ra/.claude.json")), json!(true));

        let bot = switch(&env, &bot.id, "rb").await;
        assert!(!home.join(".claude-rb").exists(), "前提：B 的目錄還不存在");
        let errs = pretrust_for_start(&env.app, &bot, HOST, CWD).await;
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(trusted(&home.join(".claude-rb/.claude.json")), json!(true), "換過去的身分也信任了");
        assert!(!home.join(".claude.json").exists(), "沒寫到預設帳號的檔");
    }

    /// 既有的 `.claude.json` 其他欄位原樣；已經信任時不重寫。
    #[tokio::test]
    async fn remote_pre_trust_keeps_the_rest_of_the_file_and_skips_when_already_trusted() {
        const HOST: &str = "trustbox-keep";
        let (env, home) = remote_env(HOST).await;
        let writes = Arc::new(AtomicUsize::new(0));
        let w = writes.clone();
        crate::hosts::set_ssh_fake(HOST, move |script| {
            if script.contains("AM_TRUST_OK") {
                w.fetch_add(1, Ordering::SeqCst);
            }
            run_sh(script)
        });
        let store = home.join(".claude-ra/.claude.json");
        std::fs::create_dir_all(store.parent().unwrap()).unwrap();
        std::fs::write(&store, r#"{"hasCompletedOnboarding":true,"oauthAccount":{"x":1},"projects":{"/other":{"allowedTools":["Bash"]}}}"#).unwrap();

        let bot = bot_on(&env, "ra").await;
        assert!(pretrust_for_start(&env.app, &bot, HOST, CWD).await.is_empty());
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&store).unwrap()).unwrap();
        assert_eq!(v["hasCompletedOnboarding"], json!(true));
        assert_eq!(v["oauthAccount"], json!({"x": 1}));
        assert_eq!(v["projects"]["/other"], json!({"allowedTools": ["Bash"]}));
        assert_eq!(v["projects"][CWD][CLAUDE_KEY], json!(true));
        assert_eq!(writes.load(Ordering::SeqCst), 1);

        assert!(pretrust_for_start(&env.app, &bot, HOST, CWD).await.is_empty());
        assert_eq!(writes.load(Ordering::SeqCst), 1, "已經信任了就不再寫");
    }

    /// 讀和寫之間 claude 自己改了檔（它常寫 `.claude.json`）：不能拿舊內容蓋掉，重讀再合併。
    #[tokio::test]
    async fn remote_pre_trust_does_not_clobber_a_concurrent_write() {
        const HOST: &str = "trustbox-race";
        let (env, home) = remote_env(HOST).await;
        let store = home.join(".claude-ra/.claude.json");
        std::fs::create_dir_all(store.parent().unwrap()).unwrap();
        std::fs::write(&store, r#"{"numStartups":1}"#).unwrap();
        let raced = Arc::new(AtomicUsize::new(0));
        let (r, st) = (raced.clone(), store.clone());
        crate::hosts::set_ssh_fake(HOST, move |script| {
            if script.contains("AM_TRUST_OK") && r.fetch_add(1, Ordering::SeqCst) == 0 {
                std::fs::write(&st, r#"{"numStartups":2,"tipsHistory":{"a":1}}"#).unwrap();
            }
            run_sh(script)
        });

        let bot = bot_on(&env, "ra").await;
        let errs = pretrust_for_start(&env.app, &bot, HOST, CWD).await;
        assert!(errs.is_empty(), "{errs:?}");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&store).unwrap()).unwrap();
        assert_eq!(v["numStartups"], json!(2), "中途寫進去的留著");
        assert_eq!(v["tipsHistory"], json!({"a": 1}));
        assert_eq!(v["projects"][CWD][CLAUDE_KEY], json!(true));
    }

    /// #407 review (1)：主機沒死透時每趟 ssh 都會等滿 `SSH_EXEC_TIMEOUT`，這段又擋在開 pane 前面。
    /// 整段有總上限，超過就只回警告——`start_inner` 照樣往下走。
    #[tokio::test]
    async fn a_hung_remote_gives_the_start_a_warning_within_the_budget() {
        const HOST: &str = "trustbox-hang";
        let (env, home) = remote_env(HOST).await;
        // 收下連線卻不回話：每趟 ssh 都會慢慢等到 `SSH_EXEC_TIMEOUT`（30 秒）。
        crate::hosts::set_ssh_delay(HOST, std::time::Duration::from_secs(30));
        crate::hosts::set_ssh_fake(HOST, run_sh);
        let bot = bot_on(&env, "ra").await;
        let mut b = bot.clone();
        b.cwd = Some(CWD.to_string());

        let budget = std::time::Duration::from_millis(300);
        let t0 = std::time::Instant::now();
        let errs = pretrust_bots_remote_within(&env.app, HOST, std::slice::from_ref(&b), budget).await;
        let took = t0.elapsed();

        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("took longer than"), "{errs:?}");
        assert!(took < std::time::Duration::from_secs(5), "沒有在上限內回來：{took:?}");
        assert!(!home.join(".claude-ra").exists(), "逾時不會留半個檔");
    }

    /// #407 review (1)：已經信任的只花一趟 ssh（讀），不會再發第二趟。
    #[tokio::test]
    async fn an_already_trusted_remote_store_costs_exactly_one_ssh_round_trip() {
        const HOST: &str = "trustbox-oneshot";
        let (env, home) = remote_env(HOST).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        crate::hosts::set_ssh_fake(HOST, move |script| {
            c.fetch_add(1, Ordering::SeqCst);
            run_sh(script)
        });
        let bot = bot_on(&env, "ra").await;
        assert!(pretrust_for_start(&env.app, &bot, HOST, CWD).await.is_empty());
        assert_eq!(calls.swap(0, Ordering::SeqCst), 2, "第一次是讀 + 寫");
        assert_eq!(trusted(&home.join(".claude-ra/.claude.json")), json!(true));

        assert!(pretrust_for_start(&env.app, &bot, HOST, CWD).await.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 1, "已經信任：只讀一趟，不再寫");
    }

    /// #407 review (2)：鍵要用**遠端**的 `pwd -P`。symlink 的工作目錄（worktree、`/tmp`→`/private/tmp`）
    /// 照字面寫下去只會多一個沒用的鍵，claude 起來照樣問。
    #[tokio::test]
    async fn the_recorded_key_is_the_remote_physical_path_not_the_literal_cwd() {
        const HOST: &str = "trustbox-symlink";
        let (env, home) = remote_env(HOST).await;
        crate::hosts::set_ssh_fake(HOST, run_sh);

        let real = env.dir.join("real-proj");
        std::fs::create_dir_all(&real).unwrap();
        let link = env.dir.join("link-proj");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let physical = std::fs::canonicalize(&real).unwrap().to_string_lossy().into_owned();
        assert_ne!(physical, link.to_string_lossy(), "前提：兩個路徑不一樣");

        let bot = bot_on(&env, "ra").await;
        let errs = pretrust_for_start(&env.app, &bot, HOST, &link.to_string_lossy()).await;
        assert!(errs.is_empty(), "{errs:?}");

        let v: Value = serde_json::from_str(&std::fs::read_to_string(home.join(".claude-ra/.claude.json")).unwrap()).unwrap();
        assert_eq!(v["projects"][&physical][CLAUDE_KEY], json!(true), "寫的是 pwd -P 的結果：{v}");
        assert!(v["projects"].get(link.to_string_lossy().as_ref()).is_none(), "不留字面路徑那個沒用的鍵：{v}");
        assert_eq!(v["projects"].as_object().unwrap().len(), 1);
    }

    /// 目錄還不存在（bot 的 cwd 還沒建出來）就照字面留著，跟本機的 [`canonical`] 一樣，不是錯。
    #[tokio::test]
    async fn a_remote_cwd_that_does_not_exist_yet_is_recorded_verbatim() {
        const HOST: &str = "trustbox-nodir";
        let (env, home) = remote_env(HOST).await;
        crate::hosts::set_ssh_fake(HOST, run_sh);
        let bot = bot_on(&env, "ra").await;
        assert!(pretrust_for_start(&env.app, &bot, HOST, "/no/such/dir/anywhere").await.is_empty());
        let v: Value = serde_json::from_str(&std::fs::read_to_string(home.join(".claude-ra/.claude.json")).unwrap()).unwrap();
        assert_eq!(v["projects"]["/no/such/dir/anywhere"][CLAUDE_KEY], json!(true));
    }

    /// #407 review (3)：`mv` 之前掛掉不能把 `.am-trust.*.tmp` 留在使用者的設定目錄裡。
    #[tokio::test]
    async fn a_write_that_dies_before_the_rename_leaves_no_temp_file() {
        const HOST: &str = "trustbox-trap";
        let (env, home) = remote_env(HOST).await;
        let dir = home.join(".claude-ra");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".claude.json"), "{}").unwrap();
        // `mv` 那一行換成 `exit 1`：cat 已經寫好暫存檔，接著就死了。
        crate::hosts::set_ssh_fake(HOST, |script| run_sh(&script.replace("mv -f \"$T\" \"$F\"", "exit 1")));

        let bot = bot_on(&env, "ra").await;
        let errs = pretrust_for_start(&env.app, &bot, HOST, CWD).await;
        assert_eq!(errs.len(), 1, "寫失敗要回警告：{errs:?}");
        let strays: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("am-trust"))
            .collect();
        assert!(strays.is_empty(), "暫存檔沒被 trap 收掉：{strays:?}");
        assert_eq!(std::fs::read_to_string(dir.join(".claude.json")).unwrap(), "{}", "原檔沒被動到");
    }

    /// 讀回來缺一段（ssh 中途斷、遠端 shell 吐別的）不能當成「檔案是空的」而整個蓋掉。
    #[test]
    fn a_truncated_remote_read_is_an_error_not_an_empty_file() {
        let (paths, sum, body) = parse_remote_read("AM_P=/a\nAM_SUM=missing\n", 1).unwrap();
        assert_eq!((paths, sum.as_str(), body.as_str()), (vec!["/a".to_string()], "missing", ""));
        let (_, sum, body) = parse_remote_read("AM_P=/a\nAM_SUM=1 2\n{\"x\":1}", 1).unwrap();
        assert_eq!((sum.as_str(), body.as_str()), ("1 2", "{\"x\":1}"));
        assert!(parse_remote_read("AM_SUM=missing\n", 1).is_err(), "少了路徑");
        assert!(parse_remote_read("AM_P=/a\n", 1).is_err(), "少了 cksum");
        assert!(parse_remote_read("", 1).is_err());
    }

    /// ssh 失敗是警告，不擋啟動（跟本機一樣 best effort），也不會寫出半個檔。
    #[tokio::test]
    async fn an_unreachable_remote_is_a_warning_not_a_failure() {
        const HOST: &str = "trustbox-down";
        let (env, home) = remote_env(HOST).await;
        crate::hosts::set_ssh_fake(HOST, |_| bail!("ssh: connect to host: Connection refused"));
        let bot = bot_on(&env, "ra").await;
        let errs = pretrust_for_start(&env.app, &bot, HOST, CWD).await;
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("Connection refused"), "{errs:?}");
        assert!(!home.join(".claude-ra").exists());
    }
