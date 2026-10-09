
    /// 串流下載：黑名單檔頭與超大檔在送出前就擋下（只看檔頭／fstat，不整份讀進記憶體）。
    #[tokio::test]
    async fn share_file_stream_rejects_withheld_head_and_oversize_without_buffering() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "alfa").await;
        let outbox = ensure(&env.app.data_dir, &bot.id).unwrap();

        // 名字看起來無害、內容是私鑰：檔頭黑名單擋下，回 NotFound。
        std::fs::write(outbox.join("notes.txt"), b"-----BEGIN RSA PRIVATE KEY-----\nabc\n-----END RSA PRIVATE KEY-----\n").unwrap();
        assert!(matches!(share_file_stream(&env.app, &bot.id, "notes.txt").await, Err(ShareFileError::NotFound)));
        // 真的 .pem 副檔名也一樣不給。
        std::fs::write(outbox.join("key.pem"), b"-----BEGIN PRIVATE KEY-----\nabc\n").unwrap();
        assert!(matches!(share_file_stream(&env.app, &bot.id, "key.pem").await, Err(ShareFileError::NotFound)));

        // MAX_BYTES + 1 的稀疏檔：fstat 就擋下，回 TooLarge。
        let f = std::fs::File::create(outbox.join("huge.bin")).unwrap();
        f.set_len(MAX_BYTES + 1).unwrap();
        drop(f);
        assert!(matches!(share_file_stream(&env.app, &bot.id, "huge.bin").await, Err(ShareFileError::TooLarge)));

        // 一般檔：200、Content-Length 對、body 是完整內容。
        std::fs::write(outbox.join("ok.txt"), b"hello stream").unwrap();
        let res = share_file_stream(&env.app, &bot.id, "ok.txt").await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers().get(header::CONTENT_LENGTH).unwrap(), "12");
        let bytes = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
        assert_eq!(&bytes[..], b"hello stream");
    }

    /// 驗證完的 fd 就是送出的 fd：回應拿到之後把路徑換成別的檔，收到的仍是驗證當下那個檔的內容。
    #[tokio::test]
    async fn share_file_stream_serves_the_validated_fd() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "alfa").await;
        let outbox = ensure(&env.app.data_dir, &bot.id).unwrap();
        std::fs::write(outbox.join("a.txt"), b"original").unwrap();

        let res = share_file_stream(&env.app, &bot.id, "a.txt").await.unwrap();
        std::fs::remove_file(outbox.join("a.txt")).unwrap();
        std::fs::write(outbox.join("a.txt"), b"swapped-in-later").unwrap();

        assert_eq!(res.headers().get(header::CONTENT_LENGTH).unwrap(), "8");
        let bytes = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
        assert_eq!(&bytes[..], b"original");
    }

    use crate::app_ports_p10::{file, list};
    use crate::state::App;
    use axum::extract::{Path as UrlPath, Query, State};
    use super::*;

    #[test]
    fn the_bot_id_is_the_only_thing_that_picks_the_directory() {
        let data = Path::new("/data");
        assert_eq!(dir_for(data, "01M2MC36YBQZWXCQN83RKD61TE"), Some(PathBuf::from("/data/outbox/01M2MC36YBQZWXCQN83RKD61TE")));
        for bad in ["", "..", "../x", "a/b", "b1 ", "b.1"] {
            assert_eq!(dir_for(data, bad), None, "{bad:?} 不能拼進路徑");
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let base = crate::testing::track(std::env::temp_dir().join(format!("am-outbox-{tag}-{}", crate::db::ulid())));
        std::fs::create_dir_all(&base).unwrap();
        std::fs::canonicalize(base).unwrap()
    }

    /// 舊 `servable(root, requested) -> Option<PathBuf>` 的測試替身：現在整條路徑驗證＋open 都是
    /// fd-bound（[`open_outbox_entry`]），沒有中間的 `PathBuf` 可以比對，所以直接回讀出來的內容。
    fn servable(root: &Path, requested: &str) -> Option<Vec<u8>> {
        let (mut file, _name) = open_outbox_entry(root, &[], requested, None)?;
        use std::io::Read;
        let mut data = Vec::new();
        file.read_to_end(&mut data).ok()?;
        if content_is_withheld(&data) {
            return None;
        }
        Some(data)
    }

    /// outbox 裡的檔案（相對或絕對）放行；`..`、指到外面的符號連結、目錄、不存在都擋。
    #[test]
    fn only_files_inside_the_outbox_resolve() {
        let base = scratch("resolve");
        let root = base.join("outbox");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("tracking.tsv"), b"a\tb\n").unwrap();
        std::fs::write(root.join("sub/report.md"), b"# hi").unwrap();
        std::fs::write(base.join("outside.txt"), b"secret").unwrap();
        std::os::unix::fs::symlink(base.join("outside.txt"), root.join("link.txt")).unwrap();

        assert!(servable(&root, "tracking.tsv").is_some());
        assert!(servable(&root, "sub/report.md").is_some(), "子目錄也算在裡面");
        assert!(servable(&root, root.join("tracking.tsv").to_str().unwrap()).is_some(), "絕對路徑但在裡面");
        assert!(servable(&root, "../outside.txt").is_none(), "用 .. 逃出去");
        assert!(servable(&root, base.join("outside.txt").to_str().unwrap()).is_none(), "絕對路徑在外面");
        assert!(servable(&root, "link.txt").is_none(), "符號連結指到外面");
        assert!(servable(&root, "sub").is_none(), "目錄不是檔案");
        assert!(servable(&root, "missing.tsv").is_none());
        assert!(servable(&root, "  ").is_none());
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 資料庫、金鑰、隱藏檔不管名字怎麼取都不列、不給下載；一般檔案照常。
    #[test]
    fn databases_and_keys_are_never_listed_or_served() {
        let base = scratch("withheld");
        let root = base.join("outbox");
        std::fs::create_dir_all(root.join(".secret")).unwrap();
        let put = |name: &str, body: &[u8]| std::fs::write(root.join(name), body).unwrap();
        put("report.md", b"# ok");
        put("migrate-check.sqlite3", b"SQLite format 3\0....");
        put("bak1644.db", b"SQLite format 3\0....");
        put("bak1644.db-wal", b"x");
        put("agents-manager.sqlite3.bak-20260916", b"x");
        put("innocent.bin", b"SQLite format 3\0 renamed");
        put("server.pem", b"-----BEGIN CERTIFICATE-----");
        put("notes.txt", b"-----BEGIN OPENSSH PRIVATE KEY-----\nabc");
        put("id_ed25519", b"x");
        put("prod.env", b"TOKEN=x");
        put(".env", b"TOKEN=x");
        put("auth.json", b"{\"tokens\":{}}");
        put("application_default_credentials.json", b"{}");
        put("hosts.yml", b"github.com:\n  oauth_token: x");
        put("ui-token", b"abc");
        put("gh.token", b"abc");
        std::fs::write(root.join(".secret/plain.txt"), b"hi").unwrap();

        let fd = trusted_open::open_bound_dir(&root, &[], None).unwrap();
        let listed: Vec<String> = scan(&fd, 0).iter().map(|f| f["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(listed, vec!["report.md".to_string()], "只剩一般檔案");
        assert!(servable(&root, "report.md").is_some());
        for p in ["migrate-check.sqlite3", "bak1644.db", "bak1644.db-wal", "agents-manager.sqlite3.bak-20260916", "innocent.bin", "server.pem", "notes.txt", "id_ed25519", "prod.env", ".env", ".secret/plain.txt", "auth.json", "application_default_credentials.json", "hosts.yml", "ui-token", "gh.token"] {
            assert!(servable(&root, p).is_none(), "{p} 不能下載");
        }
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 每個檔案帶到期時間與剩餘秒數：mtime + 1 小時，過了是 0（不是負數，也不會溢位）。
    #[test]
    fn every_listed_file_says_how_long_it_has_left() {
        let base = scratch("ttl");
        std::fs::write(base.join("fresh.txt"), b"x").unwrap();
        std::fs::create_dir_all(base.join("folder")).unwrap();
        let fd = trusted_open::open_bound_dir(&base, &[], None).unwrap();
        let files = scan(&fd, 0);
        assert_eq!(files.len(), 1, "子目錄不列：{files:?}");
        let modified = files[0]["modified"].as_u64().unwrap();
        assert!(modified > 0);
        assert_eq!(files[0]["expires_at"], json!(modified + TTL_SECS));
        assert_eq!(scan(&fd, modified + 600)[0]["remaining_secs"], json!(TTL_SECS - 600), "放了十分鐘剩五十分鐘");
        assert_eq!(scan(&fd, modified + TTL_SECS + 1)[0]["remaining_secs"], json!(0), "過期是 0，等清理");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn checked_scan_reports_directory_read_failures_instead_of_a_complete_empty_list() {
        let base = scratch("checked-scan-error");
        let file_path = base.join("not-a-directory");
        std::fs::write(&file_path, b"x").unwrap();
        let fd = std::fs::File::open(file_path).unwrap();
        assert!(scan_checked(&fd, 0).is_err(), "a failed enumeration is not a verified empty outbox");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 保留期從「搬進 outbox」起算：`mv`／`cp -p` 進來的檔 mtime 還是很久以前，ctime 才是搬入那一刻。
    /// 只看 mtime 的話，清單說「已過期」、清理（mtime 與 ctime 都過了才刪）卻還沒動，兩邊對不上。
    #[test]
    fn a_file_moved_in_with_an_old_mtime_expires_from_when_it_landed() {
        let base = scratch("landed");
        let f = std::fs::File::create(base.join("moved-in.pdf")).unwrap();
        let two_hours_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
        f.set_modified(two_hours_ago).unwrap(); // mtime 兩小時前；ctime 是現在
        drop(f);
        let fd = trusted_open::open_bound_dir(&base, &[], None).unwrap();
        let now = now_secs();
        let listed = &scan(&fd, now)[0];
        assert!(listed["modified"].as_u64().unwrap() <= now - 7000, "modified 仍是檔案內容的 mtime：{listed}");
        assert!(listed["remaining_secs"].as_u64().unwrap() > TTL_SECS - 60, "剛搬進來，剩的是整個保留期：{listed}");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 下載一律是附件，而且中文檔名帶得回去。
    #[test]
    fn every_download_is_an_attachment_with_a_usable_filename() {
        assert_eq!(mime_of(Path::new("a.tsv")), "text/plain; charset=utf-8");
        assert_eq!(mime_of(Path::new("a.pdf")), "application/pdf");
        // 白名單以外一律 octet-stream：bot 寫出來的 HTML 不在這個 origin 跑起來。
        assert_eq!(mime_of(Path::new("evil.html")), "application/octet-stream");
        assert_eq!(mime_of(Path::new("evil.svg")), "application/octet-stream");
        assert_eq!(mime_of(Path::new("noext")), "application/octet-stream");
        let d = content_disposition("出貨追蹤 v2.tsv");
        assert!(d.starts_with("attachment; "), "{d}");
        assert!(d.contains("%E5%87%BA"), "中文要 percent-encode：{d}");
        assert!(!d.contains("出貨"), "ASCII 的那份不能夾原字元：{d}");
        assert!(content_disposition("a\"; rm -rf /.txt").contains("filename=\"a__"), "引號不能逃出去");
    }

    async fn body(resp: Response) -> (StatusCode, Vec<u8>) {
        let status = resp.status();
        (status, axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap().to_vec())
    }

    async fn get_file(app: &Arc<App>, bot: &str, path: &str) -> (StatusCode, Vec<u8>) {
        let q = Query([("path".to_string(), path.to_string())].into_iter().collect());
        match file(State(app.clone()), UrlPath(bot.to_string()), q).await {
            Ok(r) => body(r).await,
            Err(e) => body(e.into_response()).await,
        }
    }

    /// 端到端：列表與下載只看 outbox。bot 的 scratchpad 就算有檔案、就算從 outbox 用符號連結指過去、
    /// 就算直接給絕對路徑，一律 404；舊的 scratchpad 路徑也是 404。
    #[tokio::test]
    async fn the_endpoints_serve_the_outbox_and_never_the_scratchpad() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "alfa").await;
        let outbox = ensure(&env.app.data_dir, &bot.id).unwrap();
        std::fs::write(outbox.join("report.md"), b"# for you").unwrap();
        std::fs::write(outbox.join("dump.sqlite3"), b"SQLite format 3\0").unwrap();
        let scratchpad = env.dir.join("claude-501/-slug/0004cea2-a8cd-4c0c-aeb5-6ddaa7fd480c/scratchpad");
        std::fs::create_dir_all(&scratchpad).unwrap();
        std::fs::write(scratchpad.join("w1.py"), b"print(1)").unwrap();
        std::os::unix::fs::symlink(scratchpad.join("w1.py"), outbox.join("w1.py")).unwrap();

        let (status, bytes) = body(list(State(env.app.clone()), UrlPath(bot.id.clone())).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let names: Vec<&str> = v["files"].as_array().unwrap().iter().map(|f| f["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["report.md"], "符號連結與 DB 都不列：{v}");
        assert_eq!(v["ttl_secs"], json!(TTL_SECS));
        assert!(v["files"][0]["remaining_secs"].as_u64().unwrap() > TTL_SECS - 60);

        let (status, bytes) = get_file(&env.app, &bot.id, "report.md").await;
        assert_eq!((status, bytes.as_slice()), (StatusCode::OK, &b"# for you"[..]));
        for p in ["w1.py", scratchpad.join("w1.py").to_str().unwrap(), "../../../claude-501", "dump.sqlite3"] {
            assert_eq!(get_file(&env.app, &bot.id, p).await.0, StatusCode::NOT_FOUND, "{p}");
        }
        assert_eq!(scratchpad_gone().await.into_response().status(), StatusCode::NOT_FOUND);

        // outbox 還沒建（或被清理收掉了）：空清單，不是錯誤。
        std::fs::remove_dir_all(&outbox).unwrap();
        let (status, bytes) = body(list(State(env.app.clone()), UrlPath(bot.id.clone())).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["files"], json!([]));
    }

    /// review 2026-09-16 core 11 洞 1：bot 把自己的 outbox 換成指向別處（例如 `~/.codex`）的符號連結，界線不能跟著搬過去。
    #[tokio::test]
    async fn a_symlinked_outbox_is_neither_listed_nor_served() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "alfa").await;
        let elsewhere = env.dir.join("dot-codex");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("session-notes.md"), b"# not for download").unwrap();
        let outbox = dir_for(&env.app.data_dir, &bot.id).unwrap();
        std::fs::create_dir_all(outbox.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &outbox).unwrap();

        assert!(matches!(open_trusted_dir(&env.app.data_dir, &outbox), Err(())));
        let (status, bytes) = body(list(State(env.app.clone()), UrlPath(bot.id.clone())).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!((v["files"].clone(), v["reason"].clone()), (json!([]), json!("outbox_untrusted")));
        assert_eq!(get_file(&env.app, &bot.id, "session-notes.md").await.0, StatusCode::NOT_FOUND);

        // 真的目錄照常；還沒建也不算不安全。
        std::fs::remove_file(&outbox).unwrap();
        assert!(matches!(open_trusted_dir(&env.app.data_dir, &outbox), Ok(None)), "還沒建");
        ensure(&env.app.data_dir, &bot.id).unwrap();
        assert!(matches!(open_trusted_dir(&env.app.data_dir, &outbox), Ok(Some(_))));
    }

    /// issue #96：`list()` 舊實作是「可信檢查（[`open_trusted_dir`]，fd-bound）→ 之後再用路徑 `read_dir`
    /// 重新列一次」，檢查通過之後、真正列舉之前，這顆 bot 自己能把整個目錄換成指到界線外的符號連結，讓
    /// 清單改列出界線外的檔名／大小。現在 [`open_trusted_dir`] 拿到的目錄 fd 直接交給 [`scan`]，兩者是
    /// 同一次 [`list`] 呼叫裡同一個 `spawn_blocking` 用的同一個 fd——這裡重現「檢查通過拿到 fd 之後、
    /// 真正列舉之前，把路徑換掉」這個時序：列出來的還是拿到 fd 當下那個目錄的內容，不是換過去的目標。
    #[test]
    fn listing_follows_the_fd_from_the_trust_check_not_a_path_swapped_afterward() {
        let base = scratch("list-race");
        let data_dir = base.join("data");
        let outbox_dir = data_dir.join("outbox").join("BOT01");
        std::fs::create_dir_all(&outbox_dir).unwrap();
        std::fs::write(outbox_dir.join("report.md"), b"# safe").unwrap();

        // list() 的第一步：可信檢查，拿到已經打開的目錄 fd。
        let fd = open_trusted_dir(&data_dir, &outbox_dir).unwrap().expect("目錄存在，該給 fd");

        // 檢查通過之後、真正列舉之前：整個目錄搬到旁邊（內容不動），原本的名字換成指到界線外的符號連結。
        std::fs::rename(&outbox_dir, data_dir.join("outbox").join("moved-aside")).unwrap();
        let elsewhere = base.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("secret.txt"), b"host secret").unwrap();
        std::os::unix::fs::symlink(&elsewhere, &outbox_dir).unwrap();

        // list() 的第二步：拿著同一個 fd 去列，不重新解一次路徑。
        let listed: Vec<String> = scan(&fd, 0).into_iter().map(|f| f["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(listed, vec!["report.md".to_string()], "列到的是拿到 fd 當下那個目錄，不是換過去的 elsewhere");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// issue #89：舊實作是「查完路徑安全 → 再用路徑名字重新 open 一次」，兩次 open 之間，這顆 bot 自己
    /// 就能把驗證通過的檔案換成指到界線外的符號連結。現在整條鏈是 fd-bound（[`open_outbox_entry`]），
    /// 驗證用的就是真正拿去讀的那個 fd——這裡重現「先下載成功一次、換成符號連結、再下載」這個時序，
    /// 第二次一定拿不到界線外的內容。
    #[tokio::test]
    async fn a_file_swapped_for_a_symlink_between_downloads_never_leaks_the_target() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "alfa").await;
        let outbox = ensure(&env.app.data_dir, &bot.id).unwrap();
        std::fs::write(outbox.join("report.md"), b"# safe").unwrap();

        let (status, bytes) = get_file(&env.app, &bot.id, "report.md").await;
        assert_eq!((status, bytes.as_slice()), (StatusCode::OK, &b"# safe"[..]), "第一次下載，正常檔案");

        let secret = env.dir.join("host-secret.txt");
        std::fs::write(&secret, b"host secret").unwrap();
        std::fs::remove_file(outbox.join("report.md")).unwrap();
        std::os::unix::fs::symlink(&secret, outbox.join("report.md")).unwrap();

        let (status, bytes) = get_file(&env.app, &bot.id, "report.md").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "換成符號連結之後不能再拿到任何內容");
        assert_ne!(bytes, b"host secret".to_vec());
    }

    /// 遠端主機的 bot 而那台連不上（這裡根本沒設定 `box`）：清單回空＋原因，下載不給。連得上的情形見 `outbox_remote`。
    #[tokio::test]
    async fn a_remote_bot_on_an_unreachable_host_lists_nothing_and_says_why() {
        let env = crate::testing::env().await;
        let pid = crate::db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, '/r', 'r', 'box', ?)")
            .bind(&pid)
            .bind(crate::db::now())
            .execute(&env.app.db)
            .await
            .unwrap();
        let bot = crate::testing::claude_bot(&env.app, &pid, "remote").await;
        let (status, bytes) = body(list(State(env.app.clone()), UrlPath(bot.id.clone())).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!((v["files"].clone(), v["reason"].clone(), v["host"].clone()), (json!([]), json!("outbox_remote_unreachable"), json!("box")));
        assert_ne!(get_file(&env.app, &bot.id, "x.txt").await.0, StatusCode::OK);
        assert_eq!(get_file(&env.app, "nope", "x.txt").await.0, StatusCode::NOT_FOUND);
    }

    /// 分享入口維持單層：就算相對路徑合法，子資料夾裡的檔也不給（SPEC §20）。
    #[tokio::test]
    async fn share_file_stays_single_layer() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "alfa").await;
        let outbox = ensure(&env.app.data_dir, &bot.id).unwrap();
        std::fs::create_dir_all(outbox.join("sub")).unwrap();
        std::fs::write(outbox.join("sub/report.pdf"), b"%PDF-1.4").unwrap();
        std::fs::write(outbox.join("top.txt"), b"top").unwrap();
        assert!(share_file_bytes_with_limit(&env.app, &bot.id, "top.txt", 1024).await.is_ok());
        let nested = share_file_bytes_with_limit(&env.app, &bot.id, "sub/report.pdf", 1024).await;
        assert!(matches!(nested, Err(ShareFileError::NotFound)), "{nested:?}");
    }

    #[tokio::test]
    async fn share_file_bytes_with_limit_rejects_oversized_file_without_reading() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "alfa").await;
        let outbox = ensure(&env.app.data_dir, &bot.id).unwrap();

        // 建立 5 MiB 稀疏檔案，不佔硬碟空間
        let f = std::fs::File::create(outbox.join("sparse.bin")).unwrap();
        f.set_len(5 * 1024 * 1024).unwrap();
        drop(f);

        // 指定上限 4 MiB：stat 即擋下，回傳 TooLarge
        let res = share_file_bytes_with_limit(&env.app, &bot.id, "sparse.bin", 4 * 1024 * 1024).await;
        assert!(matches!(res, Err(ShareFileError::TooLarge)));

        // 建立 100 bytes 檔案
        std::fs::write(outbox.join("small.bin"), vec![b'a'; 100]).unwrap();
        // 指定上限 50 bytes：回傳 TooLarge
        let res_small_too_large = share_file_bytes_with_limit(&env.app, &bot.id, "small.bin", 50).await;
        assert!(matches!(res_small_too_large, Err(ShareFileError::TooLarge)));
        // 指定上限 200 bytes：回傳 Ok
        let res_small_ok = share_file_bytes_with_limit(&env.app, &bot.id, "small.bin", 200).await;
        assert!(res_small_ok.is_ok());
        assert_eq!(res_small_ok.unwrap().1.len(), 100);

        // 既有的 symlink 安全防護：指到外面的符號連結不可繞過（違反可信邊界回 Unavailable）
        let secret = env.dir.join("secret.bin");
        std::fs::write(&secret, b"secret").unwrap();
        std::os::unix::fs::symlink(&secret, outbox.join("link_to_secret.bin")).unwrap();
        let res_escape = share_file_bytes_with_limit(&env.app, &bot.id, "link_to_secret.bin", 4 * 1024 * 1024).await;
        assert!(matches!(res_escape, Err(ShareFileError::Unavailable)));
    }

    /// 巢狀資料夾支援：
    /// 1. 巢狀檔會列出，name 帶相對路徑
    /// 2. symlink 子目錄不跟
    /// 3. .. 下載被拒
    /// 4. 深度上限超過不列不給
    /// 5. withheld 檔在子目錄仍被擋（名字與檔頭）
    #[test]
    fn nested_outbox_files_and_boundary_guards() {
        let base = scratch("nested-guards");
        let root = base.join("outbox");
        let outside = base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"outside secret").unwrap();

        // 正常巢狀檔案
        std::fs::create_dir_all(root.join("sub/nested")).unwrap();
        std::fs::write(root.join("top.txt"), b"top").unwrap();
        std::fs::write(root.join("sub/report.pdf"), b"%PDF-1.4").unwrap();
        std::fs::write(root.join("sub/nested/data.csv"), b"a,b,c").unwrap();

        // symlink 子目錄：指向外部目錄
        std::os::unix::fs::symlink(&outside, root.join("sym_dir")).unwrap();

        // 子目錄裡的 withheld 檔（名字與內容）
        std::fs::write(root.join("sub/id_rsa"), b"key").unwrap();
        std::fs::write(root.join("sub/auth.json"), b"{}").unwrap();
        std::fs::write(root.join("sub/data.sqlite3"), b"SQLite format 3\0...").unwrap();
        std::fs::write(root.join("sub/disguised.bin"), b"SQLite format 3\0...").unwrap();
        std::fs::write(root.join("sub/.hidden"), b"hidden").unwrap();

        // withheld 名稱的子目錄
        std::fs::create_dir_all(root.join(".hidden_dir")).unwrap();
        std::fs::write(root.join(".hidden_dir/file.txt"), b"hidden").unwrap();
        std::fs::create_dir_all(root.join("id_rsa")).unwrap();
        std::fs::write(root.join("id_rsa/file.txt"), b"withheld dir").unwrap();

        // 深度超過 WALK_MAX_DEPTH (32) 的深層目錄
        let mut deep_path = root.clone();
        for i in 0..=WALK_MAX_DEPTH {
            deep_path = deep_path.join(format!("d{i}"));
        }
        std::fs::create_dir_all(&deep_path).unwrap();
        std::fs::write(deep_path.join("too_deep.txt"), b"too deep").unwrap();

        let fd = trusted_open::open_bound_dir(&root, &[], None).unwrap();
        let listed = scan(&fd, 0);
        let names: Vec<String> = listed.iter().map(|f| f["name"].as_str().unwrap().to_string()).collect();

        // 1. 巢狀檔會列出，且帶相對路徑
        assert!(names.contains(&"top.txt".to_string()), "最上層檔案有列出: {names:?}");
        assert!(names.contains(&"sub/report.pdf".to_string()), "第一層子目錄檔案有列出: {names:?}");
        assert!(names.contains(&"sub/nested/data.csv".to_string()), "第二層子目錄檔案有列出: {names:?}");

        // 2. symlink 子目錄不跟
        assert!(!names.iter().any(|n| n.starts_with("sym_dir")), "symlink 子目錄不該列出: {names:?}");
        assert!(servable(&root, "sym_dir/secret.txt").is_none(), "symlink 子目錄裡的檔案不可下載");

        // 3. .. 下載被拒
        assert!(servable(&root, "../outside/secret.txt").is_none());
        assert!(servable(&root, "sub/../../outside/secret.txt").is_none());
        assert!(servable(&root, "sub/..").is_none());

        // 4. 深度上限：超過 WALK_MAX_DEPTH 的深層檔案不列出、不可下載
        assert!(!names.iter().any(|n| n.ends_with("too_deep.txt")), "超過深度上限不該列出: {names:?}");
        let deep_rel = deep_path.strip_prefix(&root).unwrap().join("too_deep.txt");
        assert!(servable(&root, deep_rel.to_str().unwrap()).is_none(), "超過深度上限不可下載");

        // 5. withheld 檔在子目錄仍被擋
        for withheld in ["sub/id_rsa", "sub/auth.json", "sub/data.sqlite3", "sub/disguised.bin", "sub/.hidden", ".hidden_dir/file.txt", "id_rsa/file.txt"] {
            assert!(!names.contains(&withheld.to_string()), "{withheld} 不該列出: {names:?}");
            assert!(servable(&root, withheld).is_none(), "{withheld} 不可下載");
        }

        // 下載正常巢狀檔案成功
        assert_eq!(servable(&root, "sub/report.pdf"), Some(b"%PDF-1.4".to_vec()));
        assert_eq!(servable(&root, "sub/nested/data.csv"), Some(b"a,b,c".to_vec()));

        // 分享入口維持單層：只列最上層的檔，子資料夾整個不列
        let shared: Vec<String> = scan_checked(&fd, 0).unwrap().iter().map(|f| f["name"].as_str().unwrap().to_string()).collect();
        assert!(shared.contains(&"top.txt".to_string()), "分享清單有最上層檔: {shared:?}");
        assert!(!shared.iter().any(|n| n.contains('/')), "分享清單不含子資料夾裡的檔: {shared:?}");

        std::fs::remove_dir_all(&base).unwrap();
    }

