
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
        conn.connected.store(true, std::sync::atomic::Ordering::SeqCst);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
        install_ssh(host, &calls, &fail);
        Remote { env, host: host.into(), calls, fail }
    }

    /// ssh 換成記錄腳本的假貨（`calls` 記每一支腳本，`fail` 為真就連不上）。
    fn install_ssh(host: &str, calls: &Arc<Mutex<Vec<String>>>, fail: &Arc<std::sync::atomic::AtomicBool>) {
        let (c, f) = (calls.clone(), fail.clone());
        crate::hosts::set_ssh_fake(host, move |script| {
            if f.load(std::sync::atomic::Ordering::SeqCst) {
                anyhow::bail!("ssh: connect to host: Connection refused");
            }
            c.lock().unwrap().push(script.to_string());
            // `sweep` 現在也會清回收區（#431）：那支腳本要回它自己的確認字，不然每個測試都在跑 gc 的失敗路徑。
            Ok(if script.contains("AM_TRASH_GC") { "AM_TRASH_GC 0\n".into() } else { "AM_TRASHED\n".into() })
        });
    }

    /// 第二台遠端主機與它自己的專案（同一份 calls、同一個 fail 開關），回傳專案 id。
    async fn second_remote_project(r: &Remote, host: &str) -> String {
        let cfg = crate::config::HostCfg { name: host.into(), ssh: host.into(), ssh_port: 22, ssh_opts: vec![], herdr_session: "agents-manager".into(), remote_path: String::new(), shared_session: false };
        let conn = r.env.app.hosts.insert_remote_for_test(cfg).await;
        *conn.remote_home.lock().await = Some("/home/x".into());
        conn.connected.store(true, std::sync::atomic::Ordering::SeqCst);
        install_ssh(host, &r.calls, &r.fail);
        let pid = crate::db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, ?, ?, ?, ?)")
            .bind(&pid)
            .bind(format!("/p-{host}"))
            .bind(host)
            .bind(host)
            .bind(crate::db::now())
            .execute(&r.env.app.db)
            .await
            .unwrap();
        pid
    }

    impl Remote {
        async fn deleted_bot(&self, name: &str) -> String {
            let bot = tt::claude_bot(&self.env.app, &self.env.project_id, name).await;
            sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?").bind(crate::db::now()).bind(&bot.id).execute(&self.env.app.db).await.unwrap();
            bot.id
        }
        /// 搬進回收區的那支腳本（`move_in`）才有 `NAME=`；gc/還原的腳本也會列出 bot id（`KNOWN=`/`B=`），不能拿來算搬移。
        fn removed(&self, id: &str) -> usize {
            self.calls.lock().unwrap().iter().filter(|s| s.contains("NAME=") && s.contains(id)).count()
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

/// **#1301**：一顆待刪 bot 的鎖被別的路一直握著，不能讓同一輪掃到的別台主機跟著停住。
/// H1（名字排前面，先掃）的那顆拿不到鎖就這一輪跳過、留著欠著；H2 的照常清掉。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bot_lock_that_stays_held_does_not_stall_other_hosts() {
    let h1 = remote("aa-purge-lock-stuck").await;
    let stuck = h1.deleted_bot("stuck").await;
    let h2_project = second_remote_project(&h1, "zz-purge-lock-ok").await;
    let other = tt::claude_bot(&h1.env.app, &h2_project, "other").await;
    sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?").bind(crate::db::now()).bind(&other.id).execute(&h1.env.app.db).await.unwrap();
    let app = h1.env.app.clone();

    let _held = app.bot_lock(&stuck).await.lock_owned().await;
    tokio::time::timeout(std::time::Duration::from_secs(10), crate::runners::remote_purge::sweep_connected_hosts(&app))
        .await
        .expect("一輪掃描不能等一顆被握住的鎖");
    assert_eq!(h1.removed(&other.id), 1, "另一台主機的那顆照常清掉");
    assert_eq!(h1.removed(&stuck), 0, "拿不到鎖的那顆這一輪不動");
    assert!(crate::remote_purge::pending(&app.db, &h1.host, false).await.unwrap().contains(&stuck), "仍欠著，下一輪再來");
}

/// **#1318**：前 100 顆每次都清不掉（連不上、永遠失敗）。失敗的要退到排程後面，沒試過的先來：第 101 顆在第二輪就清掉，
/// 不能因為前面一直失敗就永遠排不到。
#[tokio::test]
async fn a_permanently_failing_head_of_line_does_not_starve_the_rest() {
    let r = remote("purgehost-starve").await;
    let mut ids = Vec::new();
    for i in 0..101 {
        ids.push(r.deleted_bot(&format!("bot{i:03}")).await);
    }
    let app = r.env.app.clone();
    // 第一輪排得到的那 100 顆（按 id 排序的前 100）＝永遠失敗的；剩下那顆＝第 101 顆。
    let first_batch: std::collections::HashSet<String> = crate::remote_purge::pending(&app.db, &r.host, true).await.unwrap().into_iter().collect();
    assert_eq!(first_batch.len(), 100, "前提：一輪最多 100 顆");
    let lone = ids.iter().find(|id| !first_batch.contains(*id)).unwrap().clone();
    let failing = first_batch.clone();
    let calls = r.calls.clone();
    crate::hosts::set_ssh_fake(&r.host, move |script| {
        calls.lock().unwrap().push(script.to_string());
        if script.contains("bots-trash") && failing.iter().any(|id| script.contains(id.as_str())) {
            anyhow::bail!("Permission denied");
        }
        Ok(if script.contains("AM_TRASH_GC") { "AM_TRASH_GC 0\n".into() } else { "AM_TRASHED\n".into() })
    });

    assert_eq!(crate::runners::remote_purge::sweep_due(&app, &r.host).await, (0, 100), "第一輪：前 100 顆都失敗、欠著");
    assert_eq!(crate::runners::remote_purge::sweep_due(&app, &r.host).await, (1, 0), "第二輪：失敗的還在退避，第 101 顆要排得到、清掉");
    assert_eq!(r.removed(&lone), 1, "第 101 顆的目錄被搬進回收區");
}

/// **#1320**：專案搬到別台之後，舊主機上還欠著的已刪 bot 仍然屬於舊主機（義務寫在 DB 裡，不靠 `projects.host`）。
#[tokio::test]
async fn an_owed_bot_stays_owed_to_its_old_host_after_its_project_moves() {
    let r = remote("purgehost-move-a").await;
    let new_host = "zz-purgehost-move-b";
    let _ = second_remote_project(&r, new_host).await;
    let app = r.env.app.clone();
    let id = r.deleted_bot("alfa").await;
    // 刪除的時候專案還在舊主機；之後搬到新主機，舊主機離線、從來沒 purge 過。
    sqlx::query("UPDATE projects SET host = ? WHERE id = ?").bind(new_host).bind(&r.env.project_id).execute(&app.db).await.unwrap();
    let row = r.row(&id).await.expect("搬家時舊主機的義務要寫進 DB");
    assert!(row.0.is_none() && row.1 == 0, "欠著、還沒試過");
    assert!(crate::remote_purge::pending(&app.db, &r.host, false).await.unwrap().contains(&id), "舊主機還欠著它");
    assert!(!crate::remote_purge::pending(&app.db, new_host, false).await.unwrap().contains(&id), "新主機不碰舊主機的債");
    assert_eq!(sweep(&app, new_host).await, (0, 0), "新主機的掃描不搬它");
    assert_eq!(r.removed(&id), 0);
    assert_eq!(sweep(&app, &r.host).await, (1, 0), "舊主機連回來就清掉");
    assert_eq!(r.removed(&id), 1);
}

/// #1320：搬來搬去不重複寫、不把已清掉的記號改回欠著；新主機成功清掉不算舊主機的債。
#[tokio::test]
async fn moving_a_project_back_and_forth_neither_duplicates_nor_resets_the_debt() {
    let r = remote("purgehost-move-c").await;
    let other = "zz-purgehost-move-d";
    let _ = second_remote_project(&r, other).await;
    let app = r.env.app.clone();
    // done：在舊主機清掉（有已清記號）；owed：還欠著。
    let done = r.deleted_bot("done").await;
    assert_eq!(sweep(&app, &r.host).await, (1, 0));
    let owed = r.deleted_bot("owed").await;
    r.fail.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(sweep(&app, &r.host).await, (0, 1), "owed 失敗、欠著");
    r.fail.store(false, std::sync::atomic::Ordering::SeqCst);
    for host in [other, r.host.as_str(), other, r.host.as_str(), other] {
        sqlx::query("UPDATE projects SET host = ? WHERE id = ?").bind(host).bind(&r.env.project_id).execute(&app.db).await.unwrap();
    }
    let rows = |id: String| {
        let app = app.clone();
        async move {
            sqlx::query_as::<_, (String, Option<String>)>("SELECT host, purged_at FROM remote_bot_dir_purges WHERE bot_id = ?")
                .bind(id)
                .fetch_all(&app.db)
                .await
                .unwrap()
        }
    };
    let owed_rows = rows(owed.clone()).await;
    assert_eq!(owed_rows.len(), 1, "不重複寫");
    assert_eq!((owed_rows[0].0.as_str(), owed_rows[0].1.is_none()), (r.host.as_str(), true), "義務還在最初的舊主機上");
    let done_rows = rows(done).await;
    assert_eq!(done_rows.len(), 1);
    assert!(done_rows[0].1.is_some() && done_rows[0].0 == r.host, "已清掉的記號不被搬家改回欠著");
    // 新主機成功清掉它，不能沖掉舊主機的債。
    crate::remote_purge::record(&app, &owed, other, true, None).await;
    assert!(r.row(&owed).await.unwrap().0.is_none(), "新主機的成功不算舊主機的債");
}

/// #1320：專案被刪除時，已刪的 bot 欠債記在它當下的主機。
#[tokio::test]
async fn deleting_a_project_owes_its_deleted_bots_to_the_host_they_were_on() {
    let r = remote("purgehost-del-e").await;
    let app = r.env.app.clone();
    let id = r.deleted_bot("alfa").await;
    assert!(r.row(&id).await.is_none(), "前提：還沒有記號");
    sqlx::query("UPDATE projects SET deleted_at = ? WHERE id = ?").bind(crate::db::now()).bind(&r.env.project_id).execute(&app.db).await.unwrap();
    let row: (String, Option<String>) = sqlx::query_as("SELECT host, purged_at FROM remote_bot_dir_purges WHERE bot_id = ?").bind(&id).fetch_one(&app.db).await.unwrap();
    assert_eq!(row, (r.host.clone(), None));
}

/// #1320：舊資料庫（trigger 還不存在）migrate 之後 trigger 就在；重跑不會重複或壞掉。
#[tokio::test]
async fn an_old_database_gets_the_owed_triggers_on_migrate() {
    let env = tt::env().await;
    let app = env.app.clone();
    let names = || async {
        let v: Vec<String> = sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'trigger' AND name LIKE 'remote_bot_dir_purge_owed%' ORDER BY name")
            .fetch_all(&app.db)
            .await
            .unwrap();
        v
    };
    for t in ["remote_bot_dir_purge_owed_on_host_move", "remote_bot_dir_purge_owed_on_project_delete"] {
        sqlx::query(&format!("DROP TRIGGER IF EXISTS {t}")).execute(&app.db).await.unwrap();
    }
    assert!(names().await.is_empty(), "前提：舊庫沒有這兩個 trigger");
    crate::remote_purge::migrate(&app.db).await.unwrap();
    crate::remote_purge::migrate(&app.db).await.unwrap();
    assert_eq!(names().await, vec!["remote_bot_dir_purge_owed_on_host_move".to_string(), "remote_bot_dir_purge_owed_on_project_delete".to_string()]);
}

/// 直接插一顆已刪 bot（id 由呼叫端給，好讓字典序可以排）。
async fn insert_deleted_bot(app: &std::sync::Arc<crate::state::App>, project_id: &str, id: &str) {
    sqlx::query("INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, created_at, deleted_at) VALUES (?,?,?,'claude','[]',0,1,'tok','user',?,?)")
        .bind(id)
        .bind(project_id)
        .bind(id)
        .bind(crate::db::now())
        .bind(crate::db::now())
        .execute(&app.db)
        .await
        .unwrap();
}

/// #1407：移交出去的專案不清（#708）、也不記欠著，它的已刪 bot 永遠沒有記號。它們不能佔住同一輪的名額，
/// 否則同主機其他專案的已刪 bot 永遠排不到（#1318 沒修到的那條）；收回移交之後才又列出來。
#[tokio::test]
async fn a_handed_off_project_does_not_occupy_the_sweep_slots() {
    let r = remote("purgehost-handoff").await;
    let app = r.env.app.clone();
    let host = r.host.clone();
    let handed = crate::db::ulid();
    sqlx::query("INSERT INTO projects (id, path, label, host, created_at, handed_off_to) VALUES (?, '/handed', 'handed', ?, ?, 'mac')")
        .bind(&handed)
        .bind(&host)
        .bind(crate::db::now())
        .execute(&app.db)
        .await
        .unwrap();
    // 移交專案的 101 顆已刪 bot 排在字典序前面；正常專案只有一顆 z1。
    for i in 0..101 {
        insert_deleted_bot(&app, &handed, &format!("a{i:03}")).await;
    }
    insert_deleted_bot(&app, &r.env.project_id, "z1").await;
    assert_eq!(crate::remote_purge::pending(&app.db, &host, true).await.unwrap(), vec!["z1".to_string()], "移交出去的專案的已刪 bot 不能佔名額");
    sqlx::query("UPDATE projects SET handed_off_to = NULL WHERE id = ?").bind(&handed).execute(&app.db).await.unwrap();
    assert_eq!(crate::remote_purge::pending(&app.db, &host, true).await.unwrap().len(), 100, "收回移交之後照常列出（一輪最多 100）");
}
