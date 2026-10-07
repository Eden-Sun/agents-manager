
    use super::*;
    use crate::state::App;
    use std::sync::Arc;
    use crate::testing as tt;

    /// 清附件問「有沒有訊息點名它」時，只能看帶附件的那幾則（局部索引），不能對每個候選附件把 messages 全表 LIKE 一遍。
    #[tokio::test]
    async fn the_named_by_a_message_check_uses_the_attachment_message_index() {
        let env = tt::env().await;
        let sql = format!("EXPLAIN QUERY PLAN SELECT a.id FROM attachments a WHERE {NAMED_BY_A_MESSAGE}");
        let plan: Vec<(i64, i64, i64, String)> = sqlx::query_as(&sql).fetch_all(&env.app.db).await.unwrap();
        let details: Vec<&str> = plan.iter().map(|r| r.3.as_str()).collect();
        assert!(details.iter().any(|d| d.contains("USING INDEX messages_with_attachments")), "{details:#?}");
        assert!(!details.iter().any(|d| d.starts_with("SCAN m") && !d.contains("messages_with_attachments")), "{details:#?}");
    }

    /// #471：上傳時的 mime 是呼叫端自己給的，送回去不能原樣照用——白名單外一律 octet-stream。
    /// **SVG 在白名單裡而且 inline**（瀏覽器對 SVG 不嗅探，落成 octet-stream 會讓現有縮圖變破圖），
    /// 靠回應的 `nosniff` ＋ `Content-Security-Policy: sandbox` 擋；只有 `image/*` inline，
    /// 其餘（含 pdf）都是 attachment。
    #[test]
    fn the_served_mime_is_whitelisted_and_only_images_are_inline() {
        for (given, want) in [
            ("image/png", "image/png"),
            ("image/jpeg; charset=binary", "image/jpeg"),
            ("IMAGE/PNG", "image/png"),
            ("application/pdf", "application/pdf"),
            ("text/plain", "text/plain; charset=utf-8"),
            ("text/html", "application/octet-stream"),
            ("application/xhtml+xml", "application/octet-stream"),
            ("", "application/octet-stream"),
            // svg 留在白名單而且 inline：瀏覽器對 SVG 不嗅探，改成 octet-stream 會讓現有縮圖變破圖。
            // 安全性由回應的 nosniff ＋ `Content-Security-Policy: sandbox` 擔（見 `get_attachment`）。
            ("image/svg+xml", "image/svg+xml"),
        ] {
            assert_eq!(served_mime(given), want, "{given}");
        }
        assert!(is_inline(served_mime("image/png")));
        assert!(is_inline(served_mime("image/svg+xml")), "svg 要能 inline，否則縮圖破圖");
        assert!(!is_inline(served_mime("text/html")), "HTML 不能在這個 origin 內嵌算繪");
        assert!(!is_inline(served_mime("application/pdf")), "白名單管型別，內嵌是另一回事");
    }

    /// #465 的刪除側：附件搬進回收區之後，已刪 bot 的對話仍讀得到縮圖（API.md §10.4），
    /// 所以 `read` 原地讀不到時要去回收區找同一個檔名。
    #[tokio::test]
    async fn a_remote_attachment_is_still_readable_after_its_dir_moved_to_trash() {
        let env = tt::env().await;
        let app = &env.app;
        let bot = tt::claude_bot(app, &env.project_id, "trashy").await;
        let bot_id = bot.id.as_str();
        let dir = crate::bot_trash::attachments_dir(&app.data_dir, bot_id);
        std::fs::create_dir_all(&dir).unwrap();
        let local_path = dir.join("a.png");
        std::fs::write(&local_path, b"bytes").unwrap();
        sqlx::query(
            "INSERT INTO attachments (id, bot_id, name, mime, size, local_path, agent_path, host, state, created_at)
             VALUES ('att1', ?, 'a.png', 'image/png', 5, ?, '/remote/a.png', 'zz', 'ready', ?)",
        )
        .bind(bot_id)
        .bind(local_path.to_string_lossy().into_owned())
        .bind(crate::db::now())
        .execute(&app.db)
        .await
        .unwrap();

        assert_eq!(read(app, "att1").await.unwrap().1, b"bytes", "搬走之前照舊讀得到");
        crate::bot_trash::move_in_kind(&app.data_dir, bot_id, Some(crate::bot_trash::ATTACHMENTS), &dir).unwrap().unwrap();
        assert!(!local_path.exists(), "原地已經沒有了");
        assert_eq!(read(app, "att1").await.unwrap().1, b"bytes", "回收區裡那份仍要讀得到，不能變破圖");
    }

    /// 本機 bot 的附件就放在專案目錄裡（agent 寫得到的地方）：它把那個檔案換成符號連結、指到界線外（私鑰、別的 bot 的檔案），
    /// `GET /api/attachments/:id` 不能照單全收（outbox／local-image 的 #89 同一個形狀）。換成指到 `/dev/zero` 之類也不能把記憶體讀爆。
    #[tokio::test]
    async fn a_local_attachment_swapped_for_a_symlink_is_not_served() {
        let env = tt::env().await;
        let app = &env.app;
        let bot = tt::claude_bot(app, &env.project_id, "planter").await;
        let project = crate::testing::track(std::env::temp_dir().join(format!("am-attach-sym-{}", crate::db::ulid())));
        let dir = project.join(SUBDIR);
        std::fs::create_dir_all(&dir).unwrap();
        let secret = project.join("outside-secret.txt");
        std::fs::write(&secret, b"TOP SECRET").unwrap();
        let file = dir.join("a.png");
        std::fs::write(&file, b"png-bytes").unwrap();
        sqlx::query(
            "INSERT INTO attachments (id, bot_id, name, mime, size, local_path, agent_path, host, state, created_at)
             VALUES ('att-sym', ?, 'a.png', 'image/png', 9, ?, ?, 'local', 'ready', ?)",
        )
        .bind(&bot.id)
        .bind(file.to_string_lossy().into_owned())
        .bind(file.to_string_lossy().into_owned())
        .bind(crate::db::now())
        .execute(&app.db)
        .await
        .unwrap();
        assert_eq!(read(app, "att-sym").await.unwrap().1, b"png-bytes", "正常的本機附件照舊讀得到");

        std::fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink(&secret, &file).unwrap();
        assert!(read(app, "att-sym").await.is_err(), "被換成符號連結：不給");
        // 附件目錄本身被換成指到別處的連結也一樣。
        std::fs::remove_file(&file).unwrap();
        let elsewhere = project.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("a.png"), b"other").unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &dir).unwrap();
        assert!(read(app, "att-sym").await.is_err(), "目錄被換成符號連結：不給");
        std::fs::remove_dir_all(&project).unwrap();
    }

    /// 寫入側：專案目錄是 agent 寫得到的地方，它把 `.agents-manager`（或底下的 `attachments`）換成指到別處的符號連結，
    /// 上傳不能跟著連結把使用者的檔案寫到界線外（讀取側早就逐層 `O_NOFOLLOW`，寫入也要一致）。
    #[tokio::test]
    async fn an_upload_never_writes_through_a_symlinked_attachment_dir() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        let project: String = sqlx::query_scalar("SELECT path FROM projects WHERE id = ?").bind(&env.project_id).fetch_one(&env.app.db).await.unwrap();
        let project = PathBuf::from(project);
        let elsewhere = crate::testing::track(std::env::temp_dir().join(format!("am-attach-write-{}", crate::db::ulid())));
        std::fs::create_dir_all(&elsewhere).unwrap();
        // `.agents-manager` 本身是連結。
        std::fs::create_dir_all(&project).unwrap();
        std::os::unix::fs::symlink(&elsewhere, project.join(".agents-manager")).unwrap();
        assert!(save(&env.app, &bot.id, "x.service", "text/plain", b"[Service]").await.is_err(), "連結目錄：不寫");
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0, "界線外什麼都不能出現");
        std::fs::remove_file(project.join(".agents-manager")).unwrap();
        // 只有 `attachments` 是連結。
        std::fs::create_dir_all(project.join(".agents-manager")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, project.join(SUBDIR)).unwrap();
        assert!(save(&env.app, &bot.id, "x.service", "text/plain", b"[Service]").await.is_err(), "連結的 attachments：不寫");
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
        std::fs::remove_file(project.join(SUBDIR)).unwrap();
        // 正常的還是通。
        let a = save(&env.app, &bot.id, "ok.txt", "text/plain", b"fine").await.unwrap();
        assert_eq!(read(&env.app, &a.id).await.unwrap().1, b"fine");
    }

    /// 最多 50 MiB 的寫檔不能在 tokio worker 上做（會卡住同一條 worker 上的其他請求）：寫的那條執行緒不是跑這個測試的執行緒。
    #[tokio::test]
    async fn the_local_write_runs_on_the_blocking_pool_not_the_async_worker() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        let a = save(&env.app, &bot.id, "big.bin", "application/octet-stream", &vec![1u8; 4 * 1024 * 1024]).await.unwrap();
        let file = a.path.rsplit('/').next().unwrap().to_string();
        let writer = WRITE_THREADS.lock().unwrap().get(&file).copied().expect("the write was recorded");
        assert_ne!(writer, std::thread::current().id(), "寫檔在跑 async 測試的這條執行緒上做了");
        assert_eq!(read(&env.app, &a.id).await.unwrap().1.len(), 4 * 1024 * 1024);
    }

    /// 存進 DB、回給 UI 的檔名：控制字元與雙向覆寫字元（`evil\u{202E}gnp.exe` 會顯示成 `evilexe.png`）拿掉，長度設上限。
    #[test]
    fn a_stored_attachment_name_has_no_control_or_bidi_characters_and_is_bounded() {
        assert_eq!(clean_name("evil\u{202E}gnp.exe"), "evilgnp.exe");
        assert_eq!(clean_name("a\0b\nc\rd\te.png"), "abcde.png");
        assert_eq!(clean_name("\u{2066}x\u{2069}\u{200E}.txt"), "x.txt");
        assert_eq!(clean_name("  \n "), "file");
        assert_eq!(clean_name(&"長".repeat(1000)).chars().count(), 255);
        assert_eq!(clean_name("報告 final.pdf"), "報告 final.pdf", "一般的 Unicode 檔名不動");
    }

    #[tokio::test]
    async fn the_name_in_the_row_is_the_cleaned_one() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        let a = save(&env.app, &bot.id, "..\\..\u{202E}/ev\0il.png", "image/png", b"png").await.unwrap();
        let stored: String = sqlx::query_scalar("SELECT name FROM attachments WHERE id = ?").bind(&a.id).fetch_one(&env.app.db).await.unwrap();
        assert!(!stored.chars().any(|c| c.is_control() || ('\u{202A}'..='\u{202E}').contains(&c)), "{stored:?}");
        assert_eq!(a.name, stored);
        assert!(a.path.contains("/.agents-manager/attachments/") && !a.path.contains(".."), "{}", a.path);
    }

    #[tokio::test]
    async fn local_attachment_copy_rejects_unsafe_bot_ids() {
        let env = tt::env().await;
        let protected = env.app.data_dir.join("attachments").join("keep");
        std::fs::create_dir_all(&protected).unwrap();

        for id in ["../..", "x/y", r"..\..", ""] {
            assert!(local_copy_dir(&env.app, id).is_err(), "unsafe id was accepted: {id:?}");
            assert!(protected.exists(), "path construction touched the protected directory for {id:?}");
        }
    }

    /// 2026-09-14 使用者：暫存區要收任意檔。附件不再限圖片，所以延伸檔名與那句提示都得跟著走。
    #[test]
    fn a_non_image_keeps_its_own_name_and_extension() {
        assert_eq!(ext_for("report.PDF", "application/pdf"), "pdf");
        // 沒有副檔名時才看 mime。
        assert_eq!(ext_for("report", "application/pdf"), "pdf");
        assert_eq!(ext_for("blob", "application/octet-stream"), "bin");
        assert_eq!(safe_stem("../../etc/passwd"), "passwd");
        assert_eq!(safe_stem("???"), "file");
    }

    #[test]
    fn the_prompt_says_files_unless_everything_is_an_image() {
        let a = |mime: &str, path: &str| Attachment {
            id: "1".into(),
            name: "n".into(),
            mime: mime.into(),
            size: 1,
            path: path.into(),
        };
        let shot = a("image/png", "/p/shot.png");
        let log = a("text/plain", "/p/run.log");
        assert!(deliver_text("看這個", &[shot.clone()]).contains("附加圖片（請讀取這個檔案來查看）"));
        assert!(deliver_text("看這個", &[log.clone()]).contains("附加檔案（請讀取這個檔案來查看）"));
        let both = deliver_text("看這些", &[shot, log]);
        assert!(both.contains("附加檔案（請讀取這些檔案來查看）"), "{both}");
        assert!(both.contains("/p/shot.png") && both.contains("/p/run.log"));
    }

    #[tokio::test]
    async fn save_lands_a_ready_row_that_resolves_and_reads_back() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        let a = save(&env.app, &bot.id, "shot.png", "image/png", b"pngbytes").await.unwrap();

        let state: String = sqlx::query_scalar("SELECT state FROM attachments WHERE id = ?").bind(&a.id).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(state, "ready");

        let resolved = resolve(&env.app, &bot.id, &[a.id.clone()]).await.unwrap();
        assert_eq!(resolved.len(), 1);
        let (mime, data) = read(&env.app, &a.id).await.unwrap();
        assert_eq!((mime.as_str(), data.as_slice()), ("image/png", &b"pngbytes"[..]));
    }

    /// issue #88：寫檔失敗（這裡用「父目錄其實是個檔案」逼 `create_dir_all` 失敗，不靠平台權限假設）
    /// 不能讓一個沒有 DB 紀錄的孤兒檔案消失在系統裡——staging row 先落地，失敗時轉成 `failed`，
    /// `reconcile_orphans` 找得回來清掉。
    #[tokio::test]
    async fn a_write_failure_marks_the_row_failed_and_reconcile_cleans_it_up() {
        let env = tt::env().await;
        let not_a_dir = env.dir.join("not-a-directory");
        std::fs::write(&not_a_dir, b"x").unwrap();
        let pid = db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?,?,?, 'local', ?)")
            .bind(&pid)
            .bind(not_a_dir.to_string_lossy().to_string())
            .bind("bad")
            .bind(db::now())
            .execute(&env.app.db)
            .await
            .unwrap();
        let bot = tt::claude_bot(&env.app, &pid, "alfa").await;

        assert!(save(&env.app, &bot.id, "shot.png", "image/png", b"pngbytes").await.is_err(), "writing under a file must fail");

        let state: String =
            sqlx::query_scalar("SELECT state FROM attachments WHERE bot_id = ?").bind(&bot.id).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(state, "failed", "the durable row survives the write failure instead of vanishing without a trace");

        assert_eq!(reconcile_orphans(&env.app).await, 1);
        let remaining: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM attachments WHERE bot_id = ?").bind(&bot.id).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(remaining, 0);
        assert_eq!(reconcile_orphans(&env.app).await, 0, "already cleaned up: a second pass is a no-op");
    }

    /// 上傳了卻從沒送出的附件（`ready`、沒有任何訊息引用）以前永遠不清：檔案留在專案的 `.agents-manager/attachments/`、
    /// row 留在 DB（正式庫 50 筆、近 50 MB）。依保留期清掉；**被訊息引用的絕不刪**——`message_id` 有值的、
    /// 或訊息的 `attachments_json` 點名它的（舊版兩步綁定留下的）、還在 `staging`／`failed` 的（那是 `reconcile_orphans` 的事）都不碰。
    #[tokio::test]
    async fn unreferenced_ready_attachments_are_swept_after_the_retention_but_referenced_ones_never() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        let conv = db::conversation_id(&env.app.db, &bot.id).await.unwrap();
        let mut made = std::collections::HashMap::new();
        for name in ["old-unbound", "fresh-unbound", "old-bound", "old-named", "old-staging"] {
            let a = save(&env.app, &bot.id, &format!("{name}.png"), "image/png", name.as_bytes()).await.unwrap();
            made.insert(name, a);
        }
        let three_days_ago = db::iso_in(-3 * 86_400);
        for name in ["old-unbound", "old-bound", "old-named", "old-staging"] {
            sqlx::query("UPDATE attachments SET created_at = ? WHERE id = ?").bind(&three_days_ago).bind(&made[name].id).execute(&env.app.db).await.unwrap();
        }
        sqlx::query("UPDATE attachments SET state = 'staging' WHERE id = ?").bind(&made["old-staging"].id).execute(&env.app.db).await.unwrap();
        let message = |content: String| {
            let (db_, conv) = (env.app.db.clone(), conv.clone());
            async move {
                let id = db::ulid();
                sqlx::query("INSERT INTO messages (id, conversation_id, role, content, source, created_at) VALUES (?,?,'user',?,'web',?)")
                    .bind(&id).bind(conv).bind(content).bind(db::now()).execute(&db_).await.unwrap();
                id
            }
        };
        let bound_msg = message("with a bound attachment".into()).await;
        bind(&env.app, &bound_msg, &[made["old-bound"].clone()]).await.unwrap();
        // 舊版兩步綁定：訊息的 attachments_json 點名了它，attachments.message_id 卻沒設到。
        let named_msg = message("legacy".into()).await;
        sqlx::query("UPDATE messages SET attachments_json = ? WHERE id = ?")
            .bind(serde_json::to_string(&[made["old-named"].clone()]).unwrap())
            .bind(&named_msg)
            .execute(&env.app.db)
            .await
            .unwrap();

        assert_eq!(sweep_unreferenced(&env.app, 24 * 3600).await, 1, "只有 old-unbound");
        let alive = |name: &str| {
            let (db_, id, path) = (env.app.db.clone(), made[name].id.clone(), made[name].path.clone());
            async move {
                let row: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attachments WHERE id = ?").bind(id).fetch_one(&db_).await.unwrap();
                (row == 1, std::path::Path::new(&path).exists())
            }
        };
        assert_eq!(alive("old-unbound").await, (false, false), "row 與檔案都清掉");
        for kept in ["fresh-unbound", "old-bound", "old-named", "old-staging"] {
            assert_eq!(alive(kept).await, (true, true), "{kept} 不能動");
        }
        assert_eq!(sweep_unreferenced(&env.app, 24 * 3600).await, 0, "冪等");
    }

    /// 掃的是「放了多久沒人用」，不是「上傳多久了」：一個三天前上傳的附件，使用者今天才送出——`resolve` 讀到它、`bind` 綁上它
    /// 之間若剛好輪到清理，就會被當成孤兒刪掉，這一次送出變成 `attachment is missing`（而且檔案也沒了）。
    #[tokio::test]
    async fn resolving_an_old_upload_protects_it_from_the_sweep_until_it_is_bound() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        let a = save(&env.app, &bot.id, "old.png", "image/png", b"aaa").await.unwrap();
        sqlx::query("UPDATE attachments SET created_at = ? WHERE id = ?").bind(db::iso_in(-3 * 86_400)).bind(&a.id).execute(&env.app.db).await.unwrap();
        let msg_id = seed_message(&env.app, &bot.id).await;

        let files = resolve(&env.app, &bot.id, &[a.id.clone()]).await.unwrap();
        assert_eq!(sweep_unreferenced(&env.app, 24 * 3600).await, 0, "剛被 resolve 的附件正要綁上訊息，不能當孤兒");
        bind(&env.app, &msg_id, &files).await.expect("resolve 到 bind 之間被清掉的話，這裡會失敗");
        assert!(std::path::Path::new(&a.path).exists());
    }

    /// A candidate can be resolved after the sweep's initial SELECT but before its DELETE; the
    /// refresh must fence that stale snapshot so an upload being sent is not collected.
    #[tokio::test]
    async fn resolving_after_the_sweep_snapshot_keeps_the_attachment_until_bind() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        let a = save(&env.app, &bot.id, "old-after-snapshot.png", "image/png", b"aaa").await.unwrap();
        sqlx::query("UPDATE attachments SET created_at = ? WHERE id = ?")
            .bind(db::iso_in(-3 * 86_400))
            .bind(&a.id)
            .execute(&env.app.db)
            .await
            .unwrap();
        let resolving_app = env.app.clone();
        let resolving_bot = bot.id.clone();
        let resolving_id = a.id.clone();
        crate::lifecycle::race_point::arm("attachment_sweep_after_candidates", &a.id, move || async move {
            let resolved = resolve(&resolving_app, &resolving_bot, &[resolving_id]).await.unwrap();
            assert_eq!(resolved.len(), 1);
        });

        assert_eq!(sweep_unreferenced(&env.app, 24 * 3600).await, 0, "a refreshed candidate is not expired");
        let row: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attachments WHERE id = ?")
            .bind(&a.id)
            .fetch_one(&env.app.db)
            .await
            .unwrap();
        assert_eq!(row, 1, "the row remains available for bind");
        assert!(std::path::Path::new(&a.path).exists(), "the bytes remain available for bind");
    }

    /// 同上，另一條路：送出後撤回（`retract_unsent_turn`）會把附件解綁，同一個 `client_request_id` 馬上原樣重送。
    /// 解綁當下要重新算「沒人用」的時間，不然舊上傳在重送之前就被清掉。
    #[tokio::test]
    async fn unbinding_an_old_attachment_restarts_its_sweep_clock() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        let a = save(&env.app, &bot.id, "old.png", "image/png", b"aaa").await.unwrap();
        sqlx::query("UPDATE attachments SET created_at = ? WHERE id = ?").bind(db::iso_in(-3 * 86_400)).bind(&a.id).execute(&env.app.db).await.unwrap();
        let msg_id = seed_message(&env.app, &bot.id).await;
        bind(&env.app, &msg_id, &[a.clone()]).await.unwrap();

        unbind_message(&mut *env.app.db.acquire().await.unwrap(), &msg_id, &msg_id).await.unwrap();
        sqlx::query("DELETE FROM messages WHERE id = ?").bind(&msg_id).execute(&env.app.db).await.unwrap();
        assert_eq!(sweep_unreferenced(&env.app, 24 * 3600).await, 0, "剛解綁、等著原樣重送的附件不能被清掉");
        let again = seed_message(&env.app, &bot.id).await;
        bind(&env.app, &again, &[a.clone()]).await.unwrap();
    }

    /// issue #88：daemon 在 `INSERT ... 'staging'` 之後、`UPDATE ... 'ready'` 之前死掉（或 `save()` 自己
    /// 標了 `failed`）留下的行——重開機之後不能被 resolve／read 當成 ready，`reconcile_orphans` 要能把
    /// 檔案跟 row 都收乾淨，而且收兩次是安全的（idempotent）。
    #[tokio::test]
    async fn staging_and_failed_rows_are_invisible_until_reconciled_away() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;

        let mut paths = Vec::new();
        for state in ["staging", "failed"] {
            let id = db::ulid();
            let path = env.dir.join(format!("orphan-{state}.bin"));
            std::fs::write(&path, b"orphan bytes").unwrap();
            sqlx::query(
                "INSERT INTO attachments (id, bot_id, name, mime, size, local_path, agent_path, host, state, created_at)
                 VALUES (?,?,?,?,?,?,?,?,?,?)",
            )
            .bind(&id)
            .bind(&bot.id)
            .bind("orphan")
            .bind("application/octet-stream")
            .bind(12i64)
            .bind(path.to_string_lossy().to_string())
            .bind(path.to_string_lossy().to_string())
            .bind("local")
            .bind(state)
            .bind(db::now())
            .execute(&env.app.db)
            .await
            .unwrap();

            assert!(resolve(&env.app, &bot.id, &[id.clone()]).await.is_err(), "{state} attachment must not resolve");
            assert!(read(&env.app, &id).await.is_err(), "{state} attachment must not be readable");
            paths.push(path);
        }

        assert_eq!(reconcile_orphans(&env.app).await, 2, "both staging and failed rows are orphans by now");
        for path in &paths {
            assert!(!path.exists(), "orphan file should be cleaned up: {}", path.display());
        }
        let remaining: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM attachments WHERE bot_id = ?").bind(&bot.id).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(remaining, 0);
        assert_eq!(reconcile_orphans(&env.app).await, 0, "nothing left to clean the second time");
    }

    async fn seed_message(app: &Arc<App>, bot_id: &str) -> String {
        let conv = db::conversation_id(&app.db, bot_id).await.unwrap();
        let id = db::ulid();
        sqlx::query("INSERT INTO messages (id, conversation_id, role, content, source, created_at) VALUES (?,?, 'user', 'hi', 'web', ?)")
            .bind(&id)
            .bind(&conv)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        id
    }

    #[tokio::test]
    async fn bind_sets_the_message_projection_and_every_attachment_atomically() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        let a = save(&env.app, &bot.id, "a.png", "image/png", b"aaa").await.unwrap();
        let b = save(&env.app, &bot.id, "b.png", "image/png", b"bbb").await.unwrap();
        let msg_id = seed_message(&env.app, &bot.id).await;

        bind(&env.app, &msg_id, &[a.clone(), b.clone()]).await.unwrap();

        let json: Option<String> =
            sqlx::query_scalar("SELECT attachments_json FROM messages WHERE id = ?").bind(&msg_id).fetch_one(&env.app.db).await.unwrap();
        assert!(json.is_some());
        for id in [&a.id, &b.id] {
            let mid: Option<String> =
                sqlx::query_scalar("SELECT message_id FROM attachments WHERE id = ?").bind(id).fetch_one(&env.app.db).await.unwrap();
            assert_eq!(mid.as_deref(), Some(msg_id.as_str()));
        }
    }

    /// issue #88：`bind()` 中任一 UPDATE 失敗（這裡是綁一個不存在的 attachment id），message 的
    /// projection 跟**已經**成功 UPDATE 過的那些 attachment rows 都要一起回滾，不留下半套 binding。
    #[tokio::test]
    async fn bind_rolls_back_everything_when_one_attachment_cannot_be_bound() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        let a = save(&env.app, &bot.id, "a.png", "image/png", b"aaa").await.unwrap();
        let missing = Attachment { id: "does-not-exist".into(), name: "x".into(), mime: "image/png".into(), size: 1, path: "/x".into() };
        let msg_id = seed_message(&env.app, &bot.id).await;

        assert!(bind(&env.app, &msg_id, &[a.clone(), missing]).await.is_err());

        // a 排在前面，它的 UPDATE 先成功、遇到第二筆才失敗——先成功的那筆也要被撤銷。
        let mid: Option<String> =
            sqlx::query_scalar("SELECT message_id FROM attachments WHERE id = ?").bind(&a.id).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(mid, None, "已經成功的那筆 UPDATE 也要跟著整批回滾");
        let json: Option<String> =
            sqlx::query_scalar("SELECT attachments_json FROM messages WHERE id = ?").bind(&msg_id).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(json, None, "message 的 projection 也要回滾");
    }

    /// 只有 `ready` 能被 bind：一個還在 `staging` 的 row 不該被綁上訊息。
    #[tokio::test]
    async fn bind_refuses_a_staging_attachment() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        let id = db::ulid();
        let path = env.dir.join("still-staging.bin");
        std::fs::write(&path, b"x").unwrap();
        sqlx::query(
            "INSERT INTO attachments (id, bot_id, name, mime, size, local_path, agent_path, host, state, created_at)
             VALUES (?,?,?,?,?,?,?,?,'staging',?)",
        )
        .bind(&id)
        .bind(&bot.id)
        .bind("n")
        .bind("application/octet-stream")
        .bind(1i64)
        .bind(path.to_string_lossy().to_string())
        .bind(path.to_string_lossy().to_string())
        .bind("local")
        .bind(db::now())
        .execute(&env.app.db)
        .await
        .unwrap();
        let msg_id = seed_message(&env.app, &bot.id).await;

        let item = Attachment { id: id.clone(), name: "n".into(), mime: "application/octet-stream".into(), size: 1, path: path.to_string_lossy().into_owned() };
        assert!(bind(&env.app, &msg_id, &[item]).await.is_err());

        let state: String = sqlx::query_scalar("SELECT state FROM attachments WHERE id = ?").bind(&id).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(state, "staging", "還沒 ready 不該被誤綁");
    }
