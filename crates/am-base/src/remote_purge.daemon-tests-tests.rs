
    use crate::testing as tt;
    use crate::runners::remote_purge::sweep;
    use std::sync::{Arc, Mutex};

    struct Remote {
        env: tt::Env,
        host: String,
        calls: Arc<Mutex<Vec<String>>>,
        fail: Arc<std::sync::atomic::AtomicBool>,
    }

    /// 專案在遠端主機 `host`、主機連線物件有家目錄（不必真的 ssh），ssh 換成記錄腳本的假貨。
    async fn remote(host: &str) -> Remote {
        let env = tt::env().await;
        let cfg = crate::config::HostCfg { name: host.into(), ssh: host.into(), ssh_port: 22, ssh_opts: vec![], herdr_session: "agents-manager".into(), remote_path: String::new(), shared_session: false };
        let conn = env.app.hosts.insert_remote_for_test(cfg).await;
        *conn.remote_home.lock().await = Some("/home/x".into());
        sqlx::query("UPDATE projects SET host = ? WHERE id = ?").bind(host).bind(&env.project_id).execute(&env.app.db).await.unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (c, f) = (calls.clone(), fail.clone());
        crate::hosts::set_ssh_fake(host, move |script| {
            if f.load(std::sync::atomic::Ordering::SeqCst) {
                anyhow::bail!("ssh: connect to host: Connection refused");
            }
            c.lock().unwrap().push(script.to_string());
            // `sweep` 現在也會清回收區（#431）：那支腳本要回它自己的確認字，不然每個測試都在跑 gc 的失敗路徑。
            Ok(if script.contains("AM_TRASH_GC") { "AM_TRASH_GC 0\n".into() } else { "AM_TRASHED\n".into() })
        });
        Remote { env, host: host.into(), calls, fail }
    }

    impl Remote {
        async fn deleted_bot(&self, name: &str) -> String {
            let bot = tt::claude_bot(&self.env.app, &self.env.project_id, name).await;
            sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?").bind(crate::db::now()).bind(&bot.id).execute(&self.env.app.db).await.unwrap();
            bot.id
        }
        fn removed(&self, id: &str) -> usize {
            self.calls.lock().unwrap().iter().filter(|s| s.contains("bots-trash") && s.contains(id)).count()
        }
        async fn row(&self, id: &str) -> Option<(Option<String>, i64, Option<String>)> {
            sqlx::query_as("SELECT purged_at, attempts, last_error FROM remote_bot_dir_purges WHERE bot_id = ?")
                .bind(id)
                .fetch_optional(&self.env.app.db)
                .await
                .unwrap()
        }
    }

    /// #349：刪除已 commit、handler 的一次性 purge 沒跑到就死了——主機連上時的掃描把它收掉，而且只收一次。
    #[tokio::test]
    async fn a_delete_that_committed_before_the_purge_ran_is_purged_on_the_next_sweep() {
        let r = remote("purgehost-a").await;
        let id = r.deleted_bot("alfa").await;
        assert_eq!(r.removed(&id), 0, "前提：還沒 purge");
        assert_eq!(sweep(&r.env.app, &r.host).await, (1, 0));
        assert_eq!(r.removed(&id), 1);
        assert!(r.row(&id).await.unwrap().0.is_some(), "記下已清掉");
        assert_eq!(sweep(&r.env.app, &r.host).await, (0, 0));
        assert_eq!(r.removed(&id), 1, "已清掉的不再 ssh 一次");
    }

    /// 整個專案刪掉（好幾顆 bot）也一樣；活著的 bot 不動。
    #[tokio::test]
    async fn every_deleted_bot_of_a_remote_project_is_purged_and_live_ones_are_not() {
        let r = remote("purgehost-b").await;
        let ids = [r.deleted_bot("alfa").await, r.deleted_bot("bravo").await, r.deleted_bot("charlie").await];
        let live = tt::claude_bot(&r.env.app, &r.env.project_id, "live").await;
        assert_eq!(sweep(&r.env.app, &r.host).await, (3, 0));
        for id in &ids {
            assert_eq!(r.removed(id), 1);
        }
        assert_eq!(r.removed(&live.id), 0, "沒刪的 bot 不動");
    }

    /// 主機離線、第一次 purge 失敗：欠著（記次數與原因），連回來再掃就清掉。
    #[tokio::test]
    async fn a_failed_purge_stays_owed_and_is_retried_after_the_host_recovers() {
        let r = remote("purgehost-c").await;
        let id = r.deleted_bot("alfa").await;
        r.fail.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(sweep(&r.env.app, &r.host).await, (0, 1));
        let (purged, attempts, err) = r.row(&id).await.unwrap();
        assert!(purged.is_none() && attempts == 1 && err.unwrap().contains("Connection refused"), "欠著且看得到原因");
        r.fail.store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(sweep(&r.env.app, &r.host).await, (1, 0));
        assert_eq!(r.removed(&id), 1);
        assert!(r.row(&id).await.unwrap().0.is_some());
    }

    /// DB 讀不到就什麼都不刪、欠著留著；讀得到之後才清。
    #[tokio::test]
    async fn an_unreadable_db_keeps_the_debt_and_removes_nothing() {
        let r = remote("purgehost-d").await;
        let id = r.deleted_bot("alfa").await;
        tt::make_table_unreadable(&r.env.app, "bots").await;
        assert_eq!(sweep(&r.env.app, &r.host).await, (0, 0));
        assert_eq!(r.removed(&id), 0);
        tt::make_table_readable(&r.env.app, "bots").await;
        assert_eq!(sweep(&r.env.app, &r.host).await, (1, 0));
        assert_eq!(r.removed(&id), 1);
    }

    /// run 還活著、或讀不到 run 的狀態：fail closed，不刪。
    #[tokio::test]
    async fn a_live_or_unreadable_run_is_never_purged() {
        let r = remote("purgehost-e").await;
        let id = r.deleted_bot("alfa").await;
        let run = tt::fake_run(&r.env.app, &id).await;
        assert_eq!(sweep(&r.env.app, &r.host).await, (0, 1));
        assert_eq!(r.removed(&id), 0, "run 還活著");
        tt::make_table_unreadable(&r.env.app, "runs").await;
        assert_eq!(sweep(&r.env.app, &r.host).await, (0, 1));
        assert_eq!(r.removed(&id), 0, "讀不到 run");
        tt::make_table_readable(&r.env.app, "runs").await;
        sqlx::query("UPDATE runs SET state='exited', ended_at=? WHERE id=?").bind(crate::db::now()).bind(&run).execute(&r.env.app.db).await.unwrap();
        assert_eq!(sweep(&r.env.app, &r.host).await, (1, 0));
        assert_eq!(r.removed(&id), 1);
    }

    /// **#511**：`pending()` 讀完、還沒輪到這顆之前，使用者按了「復原」。以前 sweep 不拿 per-bot 鎖、
    /// 也不重讀 `deleted_at`，照樣把**活著的** bot 的遠端目錄搬進回收區，還補回一列 `purged_at`——
    /// 那列會讓它之後真的被刪時被 `pending` 排除，遠端目錄從此沒人回收。
    #[tokio::test]
    async fn a_bot_restored_while_the_sweep_is_running_keeps_its_directory_and_its_debt() {
        let r = remote("purgehost-restore").await;
        let app = r.env.app.clone();
        let id = r.deleted_bot("alfa").await;
        // child：還原只清 `deleted_at`，不必動 config.toml。
        sqlx::query("UPDATE bots SET managed_by = 'child' WHERE id = ?").bind(&id).execute(&app.db).await.unwrap();

        let (a2, i2) = (app.clone(), id.clone());
        crate::runners::app_ports_p11::race_point::arm("remote_sweep_after_pending", &r.host, move || async move {
            crate::runners::app_ports_p11::test_helpers::restore_bot(a2, i2).await.unwrap();
        });

        assert_eq!(sweep(&app, &r.host).await, (0, 0), "還原掉的那顆既沒清也不算欠著");
        // `removed` 會把還原自己那次 ssh 也算進去（同樣提到 bots-trash 與這顆的 id），所以直接認搬進回收區的那支腳本。
        let moved_in = r.calls.lock().unwrap().iter().filter(|s| s.contains("AM_TRASHED") && s.contains(&id)).count();
        assert_eq!(moved_in, 0, "沒有任何一次「搬進回收區」下在這顆身上");
        assert!(crate::db::bot(&app.db, &id).await.unwrap().unwrap().deleted_at.is_none(), "它是活的");
        assert!(r.row(&id).await.is_none(), "沒有補回那列 purged_at：之後真的刪它時 pending 還撈得到");
    }

    /// 清理只認專案列記的主機：本機專案的已刪 bot 不會被遠端掃描碰到，也不會退回本機去刪。
    #[tokio::test]
    async fn the_sweep_never_touches_another_hosts_bots_or_falls_back_to_local() {
        let r = remote("purgehost-f").await;
        let other = tt::env().await;
        let local_bot = tt::claude_bot(&other.app, &other.project_id, "alfa").await;
        sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?").bind(crate::db::now()).bind(&local_bot.id).execute(&other.app.db).await.unwrap();
        let dir = other.app.data_dir.join("bots").join(&local_bot.id);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(sweep(&other.app, "purgehost-f").await, (0, 0), "這個 app 沒有專案在那台");
        assert_eq!(sweep(&other.app, crate::config::LOCAL_HOST).await, (0, 0), "本機不歸這裡");
        assert!(dir.exists());
        assert!(r.calls.lock().unwrap().is_empty());
    }
