//! 遠端已刪 bot 的 `bots/<id>/` 目錄清理（issue #349）：本機那份開機時掃 `data_dir/bots`（`purge_deleted_bot_dirs`），
//! 遠端的目錄在別台機器上、開機掃不到，刪除 handler 的一次性 ssh purge 若在送出之前 daemon 就死了，那個目錄
//! （hook 設定／token、shim、spool）就沒人會再回頭清。
//!
//! 「欠的清理」**從 DB 推得出來**：軟刪的 bot ＋ 它專案的 host（專案軟刪後列還在）＋ 沒有「已清掉」的記號。
//! 所以刪除 commit 與 purge 之間任何時刻死掉都不會忘記；主機連上（含重連）與定期輪詢時掃一次，ssh 失敗就留著下次再試。
//! `remote_bot_dir_purges` 只記結果：清掉了（`purged_at`）、或欠著的失敗次數／原因（給 `due_actions` 看）。
//! 原則跟本機一樣 fail closed：DB 讀不到、run 讀不到或還活著就不刪；host 一律取自專案列，**不明就不動，不退回本機**。
use crate::state::App;
use anyhow::Result;
use sqlx::SqlitePool;
use std::sync::Arc;
use std::time::Duration;

/// 主機連著時的重試節奏；連上那一刻另外掃一次。
const POLL_EVERY: Duration = Duration::from_secs(5 * 60);
/// 一輪最多處理幾顆：舊的軟刪 bot 一次全清會佔住連線很久，剩下的下一輪接著做。
const PER_SWEEP: i64 = 100;

pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS remote_bot_dir_purges (
           bot_id TEXT PRIMARY KEY,
           host TEXT NOT NULL,
           purged_at TEXT,
           attempts INTEGER NOT NULL DEFAULT 0,
           last_error TEXT,
           next_attempt_at TEXT,
           updated_at TEXT NOT NULL
         )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// 記下一次 purge 的結果（`purge_bot_dir` 的遠端分支呼叫，刪除 handler 與掃描共用）。寫不進去只是少一筆記號：
/// 下一輪從 DB 重新推導、再搬一次（冪等）。
pub async fn record(app: &Arc<App>, bot_id: &str, host: &str, ok: bool, error: Option<&str>) {
    let now = crate::db::now();
    let res = if ok {
        sqlx::query(
            "INSERT INTO remote_bot_dir_purges (bot_id, host, purged_at, attempts, last_error, next_attempt_at, updated_at)
             VALUES (?, ?, ?, 0, NULL, NULL, ?)
             ON CONFLICT(bot_id) DO UPDATE SET host = excluded.host, purged_at = excluded.purged_at, last_error = NULL,
               next_attempt_at = NULL, updated_at = excluded.updated_at",
        )
        .bind(bot_id)
        .bind(host)
        .bind(&now)
        .bind(&now)
        .execute(&app.db)
        .await
    } else {
        sqlx::query(
            "INSERT INTO remote_bot_dir_purges (bot_id, host, purged_at, attempts, last_error, next_attempt_at, updated_at)
             VALUES (?, ?, NULL, 1, ?, ?, ?)
             ON CONFLICT(bot_id) DO UPDATE SET host = excluded.host, attempts = attempts + 1, last_error = excluded.last_error,
               next_attempt_at = excluded.next_attempt_at, updated_at = excluded.updated_at
             WHERE purged_at IS NULL",
        )
        .bind(bot_id)
        .bind(host)
        .bind(error.unwrap_or("purge failed"))
        .bind(crate::db::iso_in(POLL_EVERY.as_secs() as i64))
        .bind(&now)
        .execute(&app.db)
        .await
    };
    if let Err(e) = res {
        tracing::warn!(bot = %bot_id, host, error = %e, "could not record the remote bot dir purge result; it will be re-derived");
    }
}

/// 這台遠端主機欠著清理的 bot（軟刪、專案在這台、沒有已清掉的記號）。讀不到就回錯，呼叫端什麼都不刪。
pub async fn pending(pool: &SqlitePool, host: &str) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT b.id FROM bots b JOIN projects p ON p.id = b.project_id
          WHERE p.host = ? AND b.deleted_at IS NOT NULL
            AND NOT EXISTS (SELECT 1 FROM remote_bot_dir_purges r WHERE r.bot_id = b.id AND r.purged_at IS NOT NULL)
          ORDER BY b.id LIMIT ?",
    )
    .bind(host)
    .bind(PER_SWEEP)
    .fetch_all(pool)
    .await?)
}

/// 掃一台遠端主機：回 `(清掉, 留著)`。只有「確定軟刪」而且「確定沒有 active run」的才搬進遠端回收區（#411）。
///
/// 順便清掉回收區裡放過 [`crate::bot_trash::KEEP_DAYS`] 的（#431）。掛在這裡而不是只掛在「主機連上」那一次：
/// 連上只發生在開機與重連，常駐連線的主機因此從來不清，`KEEP_DAYS` 等於沒生效——回收區放的是整個 bot 目錄
/// （hook spool、shim），而遠端同時是外部編譯主機，磁碟已經被 #141／#196 塞爆過兩次。
/// 清理跟「還欠著誰的目錄」是兩件事，所以排在 `pending` 之前：那一段讀不到也要清。
/// 代價是每輪多一次 ssh（腳本很短，走既有的 ControlMaster）。
pub async fn sweep(app: &Arc<App>, host: &str) -> (usize, usize) {
    if host == crate::config::LOCAL_HOST {
        return (0, 0);
    }
    crate::remote_trash::gc_host(app, host).await;
    let ids = match pending(&app.db, host).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(host, error = %e, "could not read which deleted bots still owe a remote directory purge; nothing removed");
            return (0, 0);
        }
    };
    let (mut purged, mut kept) = (0, 0);
    for id in ids {
        match crate::db::active_run(&app.db, &id).await {
            Ok(None) => {}
            Ok(Some(_)) => {
                record(app, &id, host, false, Some("run_still_active")).await;
                kept += 1;
                continue;
            }
            Err(e) => {
                tracing::warn!(bot = %id, host, error = %e, "could not read whether a deleted remote bot still has a live run; leaving its directory");
                kept += 1;
                continue;
            }
        }
        // purge_bot_dir 自己把結果記進 remote_bot_dir_purges。
        if crate::lifecycle::purge_bot_dir(app, &id, host).await {
            purged += 1;
        } else {
            kept += 1;
        }
    }
    if purged + kept > 0 {
        tracing::info!(host, purged, kept, "swept remote directories of deleted bots");
    }
    (purged, kept)
}

/// 還原的 bot 忘掉「已清掉」的記號（issue #411）：之後再被刪一次，掃描才會再搬它的目錄。寫不進去只記 log。
pub async fn forget(app: &Arc<App>, bot_id: &str) {
    if let Err(e) = sqlx::query("DELETE FROM remote_bot_dir_purges WHERE bot_id = ?").bind(bot_id).execute(&app.db).await {
        tracing::warn!(bot = %bot_id, error = %e, "could not clear the remote bot dir purge mark of a restored bot");
    }
}

/// 主機連上（含重連）那一刻背景掃一次（回收區的過期清理在 `sweep` 裡，#431）。
pub fn spawn_sweep(app: Arc<App>, host: String) {
    tokio::spawn(async move {
        sweep(&app, &host).await;
    });
}

/// 連著的遠端主機定期再掃：ssh 一時失敗、run 剛結束的那些不必等下一次重連，
/// 回收區裡過期的也在這一輪清掉（#431：不重連的主機以前永遠不清）。
pub fn spawn_poller(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(POLL_EVERY).await;
            for name in app.hosts.names().await {
                let Some(conn) = app.hosts.get(&name).await else { continue };
                if !conn.is_local() && conn.is_connected() {
                    sweep(&app, &name).await;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing as tt;
    use std::sync::Mutex;

    struct Remote {
        env: tt::Env,
        host: String,
        calls: Arc<Mutex<Vec<String>>>,
        fail: Arc<std::sync::atomic::AtomicBool>,
    }

    /// 專案在遠端主機 `host`、主機連線物件有家目錄（不必真的 ssh），ssh 換成記錄腳本的假貨。
    async fn remote(host: &str) -> Remote {
        let env = tt::env().await;
        let cfg = crate::config::HostCfg { name: host.into(), ssh: host.into(), ssh_port: 22, ssh_opts: vec![], herdr_session: "agents-manager".into(), remote_path: String::new() };
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
}
