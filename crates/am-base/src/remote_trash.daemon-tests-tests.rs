
    use super::*;
    use crate::runners::remote_trash::restore_for;
    use crate::testing as tt;
    use std::io::Write;
    use std::path::{Path, PathBuf};

    /// 假 ssh：腳本真的交給本機 `/bin/sh` 跑，遠端家目錄是測試自己的暫存目錄——搬的、清的都是真的檔案。
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

    /// 專案在遠端主機 `host`（每個測試自己的名字：ssh 假貨是全域的），家目錄在暫存目錄。
    async fn remote(host: &'static str) -> (tt::Env, PathBuf) {
        let env = tt::env().await;
        let home = env.dir.join("remote-home");
        std::fs::create_dir_all(&home).unwrap();
        let cfg = crate::config::HostCfg { name: host.into(), ssh: host.into(), ssh_port: 22, ssh_opts: vec![], herdr_session: "agents-manager".into(), remote_path: String::new(), shared_session: false };
        let conn = env.app.hosts.insert_remote_for_test(cfg).await;
        *conn.remote_home.lock().await = Some(home.to_string_lossy().into_owned());
        sqlx::query("UPDATE projects SET host = ? WHERE id = ?").bind(host).bind(&env.project_id).execute(&env.app.db).await.unwrap();
        crate::hosts::set_ssh_fake(host, run_sh);
        let root = home.join(crate::startup::remote_root_for(crate::startup::instance().as_deref()));
        (env, root)
    }

    fn test_remote_root() -> String {
        crate::startup::remote_root_for(crate::startup::instance().as_deref())
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir).map(|r| r.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
        v.sort();
        v
    }

    /// #411：刪除把遠端目錄搬進回收區（不是 `rm -rf`），還原搬回來、內容原樣，「已清掉」的記號也忘掉。
    #[tokio::test]
    async fn a_deleted_remote_bot_dir_goes_to_the_trash_and_comes_back_on_restore() {
        let (env, root) = remote("trashbox-roundtrip").await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
        // child：還原只清 `deleted_at`，不必動 config.toml。
        sqlx::query("UPDATE bots SET managed_by = 'child' WHERE id = ?").bind(&bot.id).execute(&app.db).await.unwrap();
        let dir = root.join("bots").join(&bot.id);
        std::fs::create_dir_all(dir.join("spool")).unwrap();
        std::fs::write(dir.join("spool/ev.json"), "pending").unwrap();

        assert!(crate::runners::app_ports_p11::purge_bot_dir(&app, &bot.id, "trashbox-roundtrip").await);
        assert!(!dir.exists(), "搬走了");
        let trashed = entries(&root.join("bots-trash"));
        assert_eq!(trashed.len(), 1, "{trashed:?}");
        assert!(trashed[0].starts_with(&format!("{}.", bot.id)), "{trashed:?}");
        let purged: Option<String> = sqlx::query_scalar("SELECT purged_at FROM remote_bot_dir_purges WHERE bot_id = ?").bind(&bot.id).fetch_one(&app.db).await.unwrap();
        assert!(purged.is_some(), "照舊記下已處理");

        sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?").bind(crate::db::now()).bind(&bot.id).execute(&app.db).await.unwrap();
        crate::runners::app_ports_p11::test_helpers::restore_bot(app.clone(), bot.id.clone()).await.unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("spool/ev.json")).unwrap(), "pending", "還原 API 把 spool 裡的東西拿回來");
        assert!(entries(&root.join("bots-trash")).is_empty());
        let row: Option<String> = sqlx::query_scalar("SELECT bot_id FROM remote_bot_dir_purges WHERE bot_id = ?").bind(&bot.id).fetch_optional(&app.db).await.unwrap();
        assert!(row.is_none(), "還原後忘掉記號：再刪一次，掃描才會再搬");

        // 還原時目錄已經在（重建過）就不動回收區那份。
        assert!(crate::runners::app_ports_p11::purge_bot_dir(&app, &bot.id, "trashbox-roundtrip").await);
        std::fs::create_dir_all(&dir).unwrap();
        restore_for(&app, &bot.id).await;
        assert_eq!(entries(&root.join("bots-trash")).len(), 1, "不蓋掉已經在的目錄");
        assert!(!dir.join("spool").exists());

        // 目錄本來就不在：什麼都不搬，照樣算處理完。
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(crate::runners::app_ports_p11::purge_bot_dir(&app, &bot.id, "trashbox-roundtrip").await);
        assert_eq!(entries(&root.join("bots-trash")).len(), 1);
    }

    /// 同一顆被刪過好幾次：還原拿最新那份（名字裡的毫秒最大）。glob 是字典序（900 排最後、1000 排最前），頭尾都不是最新的。
    #[tokio::test]
    async fn restore_takes_the_newest_copy() {
        let (env, root) = remote("trashbox-newest").await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
        let trash = root.join("bots-trash");
        for (ms, tag) in [("900", "oldest"), ("1000", "old"), ("2000", "new")] {
            std::fs::create_dir_all(trash.join(format!("{}.{ms}", bot.id))).unwrap();
            std::fs::write(trash.join(format!("{}.{ms}/tag", bot.id)), tag).unwrap();
        }
        restore_for(&app, &bot.id).await;
        assert_eq!(std::fs::read_to_string(root.join("bots").join(&bot.id).join("tag")).unwrap(), "new");
    }

    /// 遠端 `bots/<id>` 是 symlink 時 `[ -e ]` 為真，還原會回 AM_KEPT、回收區留著。必須只拆連結再搬回，
    /// 不能把垃圾桶 `mv` 進連結指到的目錄。真目錄仍 AM_KEPT。
    #[tokio::test]
    async fn a_remote_symlink_is_not_a_rebuilt_bot_dir() {
        let (env, root) = remote("trashbox-symlink").await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
        let dir = root.join("bots").join(&bot.id);
        let trash = root.join("bots-trash").join(format!("{}.1000", bot.id));
        std::fs::create_dir_all(&trash).unwrap();
        std::fs::write(trash.join("keep.txt"), "secret").unwrap();
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("victim.txt"), "keep-me").unwrap();
        std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&outside, &dir).unwrap();

        restore_for(&app, &bot.id).await;
        assert_eq!(std::fs::read_to_string(dir.join("keep.txt")).unwrap(), "secret");
        assert_eq!(std::fs::read_to_string(outside.join("victim.txt")).unwrap(), "keep-me");
        assert!(!outside.join("keep.txt").exists());
        assert!(std::fs::symlink_metadata(&dir).unwrap().is_dir());

        std::fs::rename(&dir, root.join("bots-trash").join(format!("{}.2000", bot.id))).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("fresh.txt"), "new").unwrap();
        restore_for(&app, &bot.id).await;
        assert_eq!(std::fs::read_to_string(dir.join("fresh.txt")).unwrap(), "new");
        assert!(root.join("bots-trash").join(format!("{}.2000", bot.id)).join("keep.txt").exists());
    }

    /// 主機連上時清掉放超過保留期的；新的、名字不合規矩的都不碰。
    #[tokio::test]
    async fn expired_trash_entries_are_removed_and_fresh_ones_kept() {
        let (env, root) = remote("trashbox-gc").await;
        let conn = env.app.hosts.get("trashbox-gc").await.unwrap();
        let trash = root.join("bots-trash");
        let old = now_ms() - Duration::from_secs(8 * 86_400).as_millis();
        let fresh = now_ms() - Duration::from_secs(86_400).as_millis();
        for name in [format!("aaa.{old}"), format!("bbb.{fresh}"), "notes.txt.bak".into(), "ccc".into()] {
            std::fs::create_dir_all(trash.join(name)).unwrap();
        }
        assert_eq!(gc(&conn, &test_remote_root(), Duration::from_secs(crate::bot_trash::KEEP_DAYS * 86_400), crate::bot_trash::MAX_BYTES).await.unwrap(), (1, 0));
        assert_eq!(entries(&trash), vec!["bbb.".to_string() + &fresh.to_string(), "ccc".into(), "notes.txt.bak".into()]);
        crate::runners::remote_trash::gc_host(&env.app, "trashbox-gc").await;
        assert_eq!(entries(&trash).len(), 3, "沒有過期的就不動");
    }

    /// 在回收區種一份 `<id>.<ms>/`，裡面放 `kb` KB 的內容（`du -sk` 量得到的才算數）。
    fn seed(trash: &Path, id: &str, ms: u128, kb: usize) -> PathBuf {
        let d = trash.join(format!("{id}.{ms}"));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("blob"), vec![b'x'; kb * 1024]).unwrap();
        d
    }

    /// **#441**：時間規則擋不住「七天之內連刪十幾顆」——那正是 #141／#196 塞爆遠端磁碟的形狀。
    /// 跟本機 `bot_trash::the_oldest_entries_go_first_once_the_trash_is_over_its_size_cap` 同一套：
    /// 沒超量一個都不動；超量就從最舊的清到降下來，最新那一份永遠留著。
    #[tokio::test]
    async fn the_oldest_remote_entries_go_first_once_the_trash_is_over_its_size_cap() {
        let (env, root) = remote("trashbox-cap").await;
        let conn = env.app.hosts.get("trashbox-cap").await.unwrap();
        let trash = root.join("bots-trash");
        let now = now_ms();
        let keep_long = Duration::from_secs(3600);
        let (old, mid, new) = (seed(&trash, "b1", now - 3_000, 64), seed(&trash, "b2", now - 2_000, 64), seed(&trash, "b3", now - 1_000, 64));

        // 沒超量、也沒過期：一個都不動。
        assert_eq!(gc(&conn, &test_remote_root(), keep_long, 100 * 1024 * 1024).await.unwrap(), (0, 0));
        assert!(old.exists() && mid.exists() && new.exists());

        // 上限只容得下一份：最舊的兩份走，最新那份留著。
        assert_eq!(gc(&conn, &test_remote_root(), keep_long, 100 * 1024).await.unwrap(), (0, 2));
        assert!(!old.exists() && !mid.exists(), "最舊的先清");
        assert!(new.exists(), "最新那一份永遠留著");
    }

    /// 過期的先清；清完還超量才輪到按大小淘汰，兩個數字分開回報（同本機 `expiry_runs_before_the_size_cap`）。
    #[tokio::test]
    async fn remote_expiry_runs_before_the_size_cap() {
        let (env, root) = remote("trashbox-both").await;
        let conn = env.app.hosts.get("trashbox-both").await.unwrap();
        let trash = root.join("bots-trash");
        let now = now_ms();
        seed(&trash, "b1", now - 10_000, 64); // 過期
        let mid = seed(&trash, "b2", now - 2_000, 64);
        let new = seed(&trash, "b3", now - 1_000, 64);

        assert_eq!(gc(&conn, &test_remote_root(), Duration::from_millis(5_000), 100 * 1024).await.unwrap(), (1, 1));
        assert!(!mid.exists() && new.exists());
        assert_eq!(entries(&trash), vec![new.file_name().unwrap().to_string_lossy().into_owned()]);
    }

    /// 名字看不懂的（別人放進來的檔案、暫存目錄）一律不碰，兩道都一樣（同本機
    /// `entries_with_unparseable_names_are_never_touched`）。
    #[tokio::test]
    async fn remote_entries_with_unparseable_names_are_never_touched() {
        let (env, root) = remote("trashbox-alien").await;
        let conn = env.app.hosts.get("trashbox-alien").await.unwrap();
        let trash = root.join("bots-trash");
        std::fs::create_dir_all(trash.join("not-a-trash-entry")).unwrap();
        std::fs::write(trash.join("README"), "x").unwrap();

        assert_eq!(gc(&conn, &test_remote_root(), Duration::ZERO, 0).await.unwrap(), (0, 0));
        assert_eq!(entries(&trash), vec!["README".to_string(), "not-a-trash-entry".into()]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn remote_gc_does_not_follow_a_symlinked_trash_root_outside_the_instance() {
        use std::os::unix::fs::symlink;

        let (env, root) = remote("trashbox-root-link").await;
        let conn = env.app.hosts.get("trashbox-root-link").await.unwrap();
        let outside = root.join("outside");
        let victim = outside.join("b1.1");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("keep.txt"), "outside data").unwrap();
        symlink(&outside, root.join("bots-trash")).unwrap();

        gc(&conn, &test_remote_root(), Duration::ZERO, 0).await.unwrap();

        assert!(victim.join("keep.txt").exists(), "remote GC must never traverse the trash-root symlink");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn remote_gc_does_not_follow_a_symlinked_parent_outside_the_instance() {
        use std::os::unix::fs::symlink;

        let (env, root) = remote("trashbox-parent-link").await;
        let conn = env.app.hosts.get("trashbox-parent-link").await.unwrap();
        let home = root.parent().unwrap().parent().unwrap();
        let outside_config = env.dir.join("outside-config");
        let victim = outside_config.join("agents-manager/bots-trash/b1.1");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("keep.txt"), "outside data").unwrap();
        symlink(&outside_config, home.join(".config")).unwrap();

        gc(&conn, &test_remote_root(), Duration::ZERO, 0).await.unwrap();

        assert!(victim.join("keep.txt").exists(), "remote GC must never traverse a parent symlink");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn remote_restore_does_not_take_an_entry_through_a_symlinked_trash_root() {
        use std::os::unix::fs::symlink;

        let (env, root) = remote("trashbox-restore-root-link").await;
        let conn = env.app.hosts.get("trashbox-restore-root-link").await.unwrap();
        let outside = root.join("outside");
        let external = outside.join("b1.9999999999999");
        std::fs::create_dir_all(&external).unwrap();
        std::fs::write(external.join("keep.txt"), "outside data").unwrap();
        symlink(&outside, root.join("bots-trash")).unwrap();

        assert_eq!(restore(&conn, "b1", &format!("{}/bots/b1", root.display()), &test_remote_root()).await.unwrap(), None);

        assert!(external.join("keep.txt").exists(), "restore must not move an entry reached through the trash-root symlink");
        assert!(!root.join("bots/b1").exists(), "an outside entry must not be restored as a bot directory");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn remote_restore_does_not_follow_a_symlinked_parent_outside_the_instance() {
        use std::os::unix::fs::symlink;

        let (env, root) = remote("trashbox-parent-link-restore").await;
        let conn = env.app.hosts.get("trashbox-parent-link-restore").await.unwrap();
        let home = root.parent().unwrap().parent().unwrap();
        let outside_config = env.dir.join("outside-config");
        let external = outside_config.join("agents-manager/bots-trash/b1.9999999999999");
        std::fs::create_dir_all(&external).unwrap();
        std::fs::write(external.join("keep.txt"), "outside data").unwrap();
        symlink(&outside_config, home.join(".config")).unwrap();

        assert_eq!(restore(&conn, "b1", &format!("{}/bots/b1", root.display()), &test_remote_root()).await.unwrap(), None);

        assert!(external.join("keep.txt").exists(), "restore must not move an entry through a parent symlink");
        assert!(!outside_config.join("agents-manager/bots/b1").exists(), "outside entries must not be restored as bots");
    }

    /// 名字帶空白的（只可能是人手動放的，`move_in` 造的是 `<ULID>.<毫秒>`）：第二道的排序以行為單位，
    /// 這種跳過——不算進總量、也不會被淘汰。第一道（過期）跟本機一樣只看那串毫秒，這裡用很長的 `keep`
    /// 把它排除，單獨釘住第二道的行為。
    #[tokio::test]
    async fn a_remote_entry_whose_name_has_spaces_is_never_evicted_for_size() {
        let (env, root) = remote("trashbox-space").await;
        let conn = env.app.hosts.get("trashbox-space").await.unwrap();
        let trash = root.join("bots-trash");
        let now = now_ms();
        let spaced = trash.join(format!("has space.{}", now - 9_000));
        std::fs::create_dir_all(&spaced).unwrap();
        std::fs::write(spaced.join("blob"), vec![b'x'; 64 * 1024]).unwrap();
        let ours = seed(&trash, "b1", now - 1_000, 64);

        // 上限 0：我們自己的只剩最新那一份（永遠留著），帶空白的那個一個位元組都沒被動到。
        assert_eq!(gc(&conn, &test_remote_root(), Duration::from_secs(3600), 0).await.unwrap(), (0, 0));
        assert!(spaced.exists() && ours.exists());
    }

    /// **#431**：清理不能只掛在「主機連上」那一次。常駐連線的主機不會再連一次，以前就永遠不清、
    /// `KEEP_DAYS` 等於沒生效。這裡走的是每 5 分鐘那一輪真正呼叫的那支（`remote_purge::sweep`），
    /// 主機從頭到尾沒有重連、也沒有任何 bot 被刪（`pending` 是空的），過期的那份還是要消失。
    #[tokio::test]
    async fn a_host_that_never_reconnects_still_has_its_expired_trash_cleaned() {
        let (env, root) = remote("trashbox-poll").await;
        let trash = root.join("bots-trash");
        let old = now_ms() - Duration::from_secs((crate::bot_trash::KEEP_DAYS + 1) * 86_400).as_millis();
        let fresh = now_ms() - Duration::from_secs(86_400).as_millis();
        std::fs::create_dir_all(trash.join(format!("aaa.{old}"))).unwrap();
        std::fs::create_dir_all(trash.join(format!("bbb.{fresh}"))).unwrap();

        // 5 分鐘那一輪對每台已連線的遠端主機做的事，就是這一行（`remote_purge::spawn_poller`）。
        assert_eq!(crate::runners::remote_purge::sweep(&env.app, "trashbox-poll").await, (0, 0), "沒有欠著的目錄");

        assert_eq!(entries(&trash), vec!["bbb.".to_string() + &fresh.to_string()], "過期的清掉、沒過期的留著");
    }

    /// ssh 失敗：刪除記成欠著（`remote_purge` 補帳），目錄原地不動。
    #[tokio::test]
    async fn a_failed_move_stays_owed() {
        let (env, root) = remote("trashbox-down").await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
        let dir = root.join("bots").join(&bot.id);
        std::fs::create_dir_all(&dir).unwrap();
        crate::hosts::set_ssh_fake("trashbox-down", |_| bail!("ssh: connect to host: Connection refused"));
        assert!(!crate::runners::app_ports_p11::purge_bot_dir(&app, &bot.id, "trashbox-down").await);
        assert!(dir.exists());
        let (purged, attempts): (Option<String>, i64) =
            sqlx::query_as("SELECT purged_at, attempts FROM remote_bot_dir_purges WHERE bot_id = ?").bind(&bot.id).fetch_one(&app.db).await.unwrap();
        assert!(purged.is_none() && attempts == 1);
    }
