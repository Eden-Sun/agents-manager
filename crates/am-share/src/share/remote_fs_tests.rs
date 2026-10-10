//! R-S1 遠端檔案原語測試（SPEC §20、remote-share-design §3）。
//!
//! 全部測試使用 `local_sh_fake` 在 scratch 目錄跑本機 sh 替身，不碰真實 HOME。

use std::fs;
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use am_base::config::HostCfg;
use am_base::hosts::{HostConn, HostFence};
use am_base::outbox::ShareFileError;

use super::remote_fs::*;
use super::site::{FencedConn, RemoteSite};
use super::test_dirs;

fn make_test_remote_site(scratch: &Path, host_name: &str) -> RemoteSite {
    test_support::local_sh_fake(host_name, scratch);
    let cfg = HostCfg {
        name: host_name.to_string(),
        ssh: "fake-ssh".to_string(),
        ssh_port: 22,
        ssh_opts: vec![],
        herdr_session: "test-session".to_string(),
        shared_session: false,
        remote_path: String::new(),
    };
    let conn = HostConn::remote(cfg, None);
    conn.connected.store(true, Ordering::SeqCst);

    let home = scratch.to_string_lossy().to_string();
    let root = scratch.join(".config/agents-manager").to_string_lossy().to_string();
    let workspace = scratch.join("shared-bots/my-bot").to_string_lossy().to_string();
    let outbox = scratch.join(".config/agents-manager/outbox/my-bot").to_string_lossy().to_string();

    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&outbox).unwrap();

    RemoteSite {
        conn: FencedConn::new(HostFence::for_conn_for_test(&conn)),
        host: host_name.to_string(),
        home,
        root,
        workspace,
        outbox,
    }
}

#[tokio::test]
async fn resolve_folder_normal_and_physical() {
    let scratch = test_dirs::scratch_dir("rfs-resolve");
    let site = make_test_remote_site(&scratch, "host-resolve-1");
    let res = RemoteSite::resolve_folder(&site.conn, &site.workspace).await.unwrap();
    assert_eq!(res.physical, site.workspace);
    assert!(res.owned_by_me);
    assert!(!res.home_physical.is_empty());
}

#[tokio::test]
async fn resolve_folder_symlink_is_untrusted() {
    let scratch = test_dirs::scratch_dir("rfs-resolve-symlink");
    let site = make_test_remote_site(&scratch, "host-resolve-2");
    let real_dir = scratch.join("real-folder");
    let sym_dir = scratch.join("sym-folder");
    fs::create_dir_all(&real_dir).unwrap();
    unix_fs::symlink(&real_dir, &sym_dir).unwrap();

    let err = RemoteSite::resolve_folder(&site.conn, &sym_dir.to_string_lossy()).await.unwrap_err();
    assert_eq!(err, RfsError::Untrusted);
}

#[tokio::test]
async fn resolve_folder_not_found() {
    let scratch = test_dirs::scratch_dir("rfs-resolve-notfound");
    let site = make_test_remote_site(&scratch, "host-resolve-3");
    let non_existent = scratch.join("does-not-exist");

    let err = RemoteSite::resolve_folder(&site.conn, &non_existent.to_string_lossy()).await.unwrap_err();
    assert_eq!(err, RfsError::NotFound);
}

#[tokio::test]
async fn create_folder_normal_and_exists() {
    let scratch = test_dirs::scratch_dir("rfs-create");
    let site = make_test_remote_site(&scratch, "host-create-1");
    let root = scratch.join("new-bots");

    let created = RemoteSite::create_folder(&site.conn, &root.to_string_lossy(), "bot-a").await.unwrap();
    assert!(created.ends_with("bot-a"));
    assert!(Path::new(&created).is_dir());

    // 再次建立同名資料夾 → Exists
    let err = RemoteSite::create_folder(&site.conn, &root.to_string_lossy(), "bot-a").await.unwrap_err();
    assert_eq!(err, RfsError::Exists);
}

#[tokio::test]
async fn remove_created_folder_only_rmdir_inbox_and_self() {
    let scratch = test_dirs::scratch_dir("rfs-rm-created");
    let site = make_test_remote_site(&scratch, "host-rm-1");
    let inbox = Path::new(&site.workspace).join("inbox");
    fs::create_dir_all(&inbox).unwrap();

    // 空的 inbox 與資料夾本體應被移除
    site.remove_created_folder().await.unwrap();
    assert!(!Path::new(&site.workspace).exists());

    // 若非空（有檔案），不應刪除非空資料夾
    let site2 = make_test_remote_site(&scratch, "host-rm-2");
    let file = Path::new(&site2.workspace).join("keep.txt");
    fs::write(&file, b"content").unwrap();
    site2.remove_created_folder().await.unwrap();
    assert!(Path::new(&site2.workspace).exists());
    assert!(file.exists());
}

#[tokio::test]
async fn ensure_inbox_normal_and_folder_missing() {
    let scratch = test_dirs::scratch_dir("rfs-ensure-inbox");
    let site = make_test_remote_site(&scratch, "host-inbox-1");

    site.ensure_inbox().await.unwrap();
    let inbox = Path::new(&site.workspace).join("inbox");
    assert!(inbox.is_dir());

    // 資料夾不見時回 NotFound，不重建
    fs::remove_dir_all(&site.workspace).unwrap();
    let err = site.ensure_inbox().await.unwrap_err();
    assert_eq!(err, RfsError::NotFound);
}

#[tokio::test]
async fn ensure_inbox_symlink_is_untrusted() {
    let scratch = test_dirs::scratch_dir("rfs-inbox-symlink");
    let site = make_test_remote_site(&scratch, "host-inbox-2");
    let outside = scratch.join("outside");
    fs::create_dir_all(&outside).unwrap();
    let inbox = Path::new(&site.workspace).join("inbox");
    unix_fs::symlink(&outside, &inbox).unwrap();

    let err = site.ensure_inbox().await.unwrap_err();
    assert_eq!(err, RfsError::Untrusted);
}

#[tokio::test]
async fn instructions_normal_and_limits() {
    let scratch = test_dirs::scratch_dir("rfs-instructions");
    let site = make_test_remote_site(&scratch, "host-instructions-1");
    let ws = Path::new(&site.workspace);

    fs::write(ws.join("CLAUDE.md"), "instructions for claude").unwrap();
    let mem_dir = ws.join("memory");
    fs::create_dir_all(&mem_dir).unwrap();
    fs::write(mem_dir.join("MEMORY.md"), "memory index").unwrap();
    fs::write(mem_dir.join("user.md"), "user details").unwrap();

    let text = site.instructions().await.unwrap();
    assert!(text.contains("instructions for claude"));
    assert!(text.contains("memory index"));
    assert!(text.contains("user details"));
}

#[tokio::test]
async fn instructions_symlink_ignored() {
    let scratch = test_dirs::scratch_dir("rfs-instructions-symlink");
    let site = make_test_remote_site(&scratch, "host-instructions-2");
    let ws = Path::new(&site.workspace);
    let secret = scratch.join("secret.txt");
    fs::write(&secret, "super-secret").unwrap();
    unix_fs::symlink(&secret, ws.join("CLAUDE.md")).unwrap();

    let text = site.instructions().await.unwrap();
    assert!(!text.contains("super-secret"));
}

#[tokio::test]
async fn write_private_files_normal_and_sha_verification() {
    let scratch = test_dirs::scratch_dir("rfs-write-private");
    let site = make_test_remote_site(&scratch, "host-write-1");
    let target_dir = scratch.join("settings-dir");

    let files = [
        ("claude-settings.json", b"{\"key\": 123}".as_slice()),
        ("share-system-prompt.md", b"system prompt".as_slice()),
    ];

    RemoteSite::write_private_files(&site.conn, &target_dir.to_string_lossy(), &files).await.unwrap();

    let content1 = fs::read(target_dir.join("claude-settings.json")).unwrap();
    assert_eq!(content1, b"{\"key\": 123}");
    let content2 = fs::read(target_dir.join("share-system-prompt.md")).unwrap();
    assert_eq!(content2, b"system prompt");
}

#[tokio::test]
async fn mark_share_keep_lifecycle() {
    let scratch = test_dirs::scratch_dir("rfs-keep");
    let site = make_test_remote_site(&scratch, "host-keep-1");
    let keep_file = Path::new(&site.outbox).join(".am-share-keep");

    site.mark_share_keep(true).await.unwrap();
    assert!(keep_file.is_file());

    site.mark_share_keep(false).await.unwrap();
    assert!(!keep_file.exists());
}

#[tokio::test]
async fn inbox_write_normal_and_quota() {
    let scratch = test_dirs::scratch_dir("rfs-inbox-write");
    let site = make_test_remote_site(&scratch, "host-inbox-write-1");
    site.ensure_inbox().await.unwrap();

    // 正常寫入
    site.inbox_write("file1.png", b"image-data", 1000, 10).await.unwrap();
    let written = Path::new(&site.workspace).join("inbox/file1.png");
    assert_eq!(fs::read(&written).unwrap(), b"image-data");

    // 超過配額 (max_bytes)
    let err = site.inbox_write("file2.png", b"too-big-data", 15, 10).await.unwrap_err();
    assert_eq!(err, InboxError::Full);
}

#[tokio::test]
async fn inbox_write_dangling_symlink_refused() {
    let scratch = test_dirs::scratch_dir("rfs-inbox-dangling");
    let site = make_test_remote_site(&scratch, "host-inbox-write-2");
    site.ensure_inbox().await.unwrap();

    let dangling = Path::new(&site.workspace).join("inbox/dangling.png");
    unix_fs::symlink(scratch.join("non-existent"), &dangling).unwrap();

    let err = site.inbox_write("dangling.png", b"data", 1000, 10).await.unwrap_err();
    assert_eq!(err, InboxError::Unavailable);
}

/// #1030：身分驗證之後、寫入之前，同一個 SSH 使用者把 inbox 裡的檔換成指向 workspace 外的連結（符號連結、硬連結），
/// 或改名、放一個新檔。寫入只能經過建檔時握著的那個 fd：canary 的位元組不能變，上傳回失敗；攻擊者換進來的檔也不能被清掉。
#[tokio::test]
async fn inbox_write_never_follows_a_name_swapped_after_the_identity_check() {
    let scratch = test_dirs::scratch_dir("rfs-inbox-race");
    let site = make_test_remote_site(&scratch, "host-inbox-race-1");
    site.ensure_inbox().await.unwrap();
    let canary = scratch.join("canary.txt");
    fs::write(&canary, b"CANARY").unwrap();
    let q = am_base::hosts::sh_quote(&canary.to_string_lossy());
    let name = "upload.bin";
    let written = Path::new(&site.workspace).join("inbox").join(name);

    // 1. 換成指向 canary 的符號連結
    let race = format!("rm -f \"$N\"; ln -s {q} \"$N\"");
    let err = site.inbox_write_racing(name, b"PAYLOAD", 1000, 10, &race).await.unwrap_err();
    assert_eq!(err, InboxError::Unavailable);
    assert_eq!(fs::read(&canary).unwrap(), b"CANARY", "符號連結不能讓寫入穿到 workspace 外");
    assert!(fs::symlink_metadata(&written).unwrap().file_type().is_symlink(), "攻擊者換進來的連結不是我們建的檔：清理不碰它");
    fs::remove_file(&written).unwrap();

    // 2. 換成 canary 的硬連結（同一個 inode）
    let race = format!("rm -f \"$N\"; ln {q} \"$N\"");
    let err = site.inbox_write_racing(name, b"PAYLOAD", 1000, 10, &race).await.unwrap_err();
    assert_eq!(err, InboxError::Unavailable);
    assert_eq!(fs::read(&canary).unwrap(), b"CANARY", "硬連結也不能讓寫入改到 canary");
    fs::remove_file(&written).unwrap();

    // 3. 改名，再放一個新檔：上傳回失敗，而且清理不刪攻擊者放的檔
    let race = "mv \"$N\" \"$N.moved\"; printf attacker > \"$N\"";
    let err = site.inbox_write_racing(name, b"PAYLOAD", 1000, 10, race).await.unwrap_err();
    assert_eq!(err, InboxError::Unavailable);
    assert_eq!(fs::read(&canary).unwrap(), b"CANARY");
    assert_eq!(fs::read(&written).unwrap(), b"attacker", "名稱已經不指向我們建的檔：清理不碰它");
}

#[tokio::test]
async fn inbox_has_multiple_queries() {
    let scratch = test_dirs::scratch_dir("rfs-inbox-has");
    let site = make_test_remote_site(&scratch, "host-has-1");
    site.ensure_inbox().await.unwrap();

    let inbox = Path::new(&site.workspace).join("inbox");
    fs::write(inbox.join("a.png"), b"1").unwrap();
    fs::write(inbox.join("b.png"), b"2").unwrap();

    let query = vec!["a.png".into(), "ghost.png".into(), "b.png".into()];
    let res = site.inbox_has(&query).await.unwrap();
    assert_eq!(res, vec![true, false, true]);
}

#[tokio::test]
async fn outbox_list_normal_and_symlink_rejected() {
    let scratch = test_dirs::scratch_dir("rfs-outbox-list");
    let site = make_test_remote_site(&scratch, "host-list-1");
    let ob = Path::new(&site.outbox);

    fs::write(ob.join("report.pdf"), b"%PDF-1.4").unwrap();
    fs::write(ob.join(".hidden"), b"secret").unwrap();

    let list = site.outbox_list(100).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["name"], "report.pdf");
    assert_eq!(list[0]["kept"], true);
}

#[tokio::test]
async fn outbox_stream_and_read() {
    let scratch = test_dirs::scratch_dir("rfs-stream-read");
    let site = make_test_remote_site(&scratch, "host-stream-1");
    let ob = Path::new(&site.outbox);

    let test_data = b"Hello, stream and read test!";
    fs::write(ob.join("test.txt"), test_data).unwrap();

    // 1. outbox_read
    let read = site.outbox_read("test.txt", 100).await.unwrap();
    assert_eq!(read.name, "test.txt");
    assert_eq!(read.data, test_data);

    // 2. outbox_stream
    let stream_file = site.outbox_stream("test.txt", 100).await.unwrap();
    assert_eq!(stream_file.len, test_data.len() as u64);
    assert_eq!(stream_file.head, test_data.as_slice());
}

#[tokio::test]
async fn outbox_read_hardlink_blocked() {
    let scratch = test_dirs::scratch_dir("rfs-hardlink");
    let site = make_test_remote_site(&scratch, "host-hardlink-1");
    let ob = Path::new(&site.outbox);

    let f1 = ob.join("f1.txt");
    let f2 = ob.join("f2.txt");
    fs::write(&f1, b"hardlink-content").unwrap();
    fs::hard_link(&f1, &f2).unwrap();

    let err = site.outbox_read("f1.txt", 100).await.unwrap_err();
    assert_eq!(err, ShareFileError::Unavailable);
}

#[tokio::test]
async fn photo_stats_and_fetch() {
    let scratch = test_dirs::scratch_dir("rfs-photo");
    let site = make_test_remote_site(&scratch, "host-photo-1");
    let inbox = Path::new(&site.workspace).join("inbox");
    fs::create_dir_all(&inbox).unwrap();

    let img1 = inbox.join("photo1.jpg");
    fs::write(&img1, b"jpg-bytes-1").unwrap();

    let rels = vec![vec!["inbox".to_string(), "photo1.jpg".to_string()]];
    let stats = site.photo_stats(&rels).await.unwrap();
    assert!(stats[0].is_some());
    assert_eq!(stats[0].as_ref().unwrap().size, 11);

    let fetched = site.photo_fetch(&rels, 100, 200).await.unwrap();
    assert_eq!(fetched[0].as_ref().unwrap(), b"jpg-bytes-1");
}

#[tokio::test]
async fn measure_calculates_and_ignores_keep_mark() {
    let scratch = test_dirs::scratch_dir("rfs-measure");
    let site = make_test_remote_site(&scratch, "host-measure-1");

    fs::write(Path::new(&site.workspace).join("f.txt"), b"12345").unwrap();
    fs::write(Path::new(&site.outbox).join(".am-share-keep"), b"keep").unwrap();

    let m = site.measure().await.unwrap();
    assert!(m.workspace_bytes > 0);
    assert_eq!(m.files, 1); // .am-share-keep 排除
}

#[tokio::test]
async fn measure_fails_closed_when_workspace_or_outbox_has_unreadable_descendant() {
    let scratch = test_dirs::scratch_dir("rfs-measure-unreadable");
    let site = make_test_remote_site(&scratch, "host-measure-unreadable-1");
    let ws = Path::new(&site.workspace);
    let ob = Path::new(&site.outbox);

    fs::write(ws.join("ok.txt"), b"readable").unwrap();
    fs::write(ob.join("out.txt"), b"outbox-ok").unwrap();

    // 0. Fully readable workspace below limit is valid
    let m = site.measure().await.unwrap();
    assert!(!m.truncated && !m.full(), "正常可讀且未滿");
    assert_eq!(m.files, 2);

    // 1. Workspace contains an unreadable subdirectory
    let priv_ws = ws.join("private_ws");
    fs::create_dir_all(&priv_ws).unwrap();
    fs::write(priv_ws.join("large.bin"), vec![0u8; 1024 * 1024]).unwrap();
    let mut perms_ws = fs::metadata(&priv_ws).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms_ws, 0o000);
    fs::set_permissions(&priv_ws, perms_ws.clone()).unwrap();

    let err_ws = site.measure().await.unwrap_err();
    assert_eq!(err_ws, RfsError::Untrusted, "unreadable subtree in workspace must fail closed");

    // Restore permissions
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms_ws, 0o700);
    fs::set_permissions(&priv_ws, perms_ws).unwrap();

    // 2. Outbox contains an unreadable subdirectory
    let priv_ob = ob.join("private_ob");
    fs::create_dir_all(&priv_ob).unwrap();
    fs::write(priv_ob.join("secret.bin"), b"secret").unwrap();
    let mut perms_ob = fs::metadata(&priv_ob).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms_ob, 0o000);
    fs::set_permissions(&priv_ob, perms_ob.clone()).unwrap();

    let err_ob = site.measure().await.unwrap_err();
    assert_eq!(err_ob, RfsError::Untrusted, "unreadable subtree in outbox must fail closed");

    // Restore permissions
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms_ob, 0o700);
    fs::set_permissions(&priv_ob, perms_ob).unwrap();

    // 3. Restored again: measure succeeds
    let m_restored = site.measure().await.unwrap();
    assert!(!m_restored.truncated && !m_restored.full());
}

#[tokio::test]
async fn measure_fails_closed_when_permissions_change_between_du_and_find() {
    let scratch = test_dirs::scratch_dir("rfs-measure-race");
    let site = make_test_remote_site(&scratch, "host-measure-race-1");
    let ws = Path::new(&site.workspace);

    let sub = ws.join("sub");
    fs::create_dir_all(&sub).unwrap();
    fs::write(sub.join("f.txt"), b"file-content").unwrap();

    // Race hook changes permission on sub after du and before find
    let race = "chmod 000 sub 2>/dev/null || true";
    let err = site.measure_racing(race).await.unwrap_err();
    assert_eq!(err, RfsError::Untrusted, "find error must fail closed even if du succeeded");

    let mut perms = fs::metadata(&sub).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o700);
    fs::set_permissions(&sub, perms).unwrap();
}

#[tokio::test]
async fn prune_outbox_policy() {
    let scratch = test_dirs::scratch_dir("rfs-prune");
    let site = make_test_remote_site(&scratch, "host-prune-1");
    let ob = Path::new(&site.outbox);

    // 建立 keep 標記檔
    fs::write(ob.join(".am-share-keep"), b"keep").unwrap();
    // 建立普通檔
    fs::write(ob.join("f1.txt"), b"file1").unwrap();

    let removed = site.prune_outbox(14, 1000, 100).await.unwrap();
    assert_eq!(removed, 0);
    assert!(ob.join(".am-share-keep").exists());
    assert!(ob.join("f1.txt").exists());
}

#[test]
fn protocol_framing_fail_closed() {
    // 缺少 AM_RFS_DONE
    let incomplete = b"AM_RFS1\nPATH 4\n/abc\n";
    assert_eq!(parse_rfs_frames(incomplete, &["PATH"]).unwrap_err(), RfsError::Unavailable);

    // 長度不符
    let bad_len = b"AM_RFS1\nPATH 10\n/abc\nAM_RFS_DONE\n";
    assert_eq!(parse_rfs_frames(bad_len, &["PATH"]).unwrap_err(), RfsError::Unavailable);

    // 未宣告的 TAG
    let unknown_tag = b"AM_RFS1\nEVIL 4\n/abc\nAM_RFS_DONE\n";
    assert_eq!(parse_rfs_frames(unknown_tag, &["PATH"]).unwrap_err(), RfsError::Untrusted);
}

#[tokio::test]
async fn host_slots_limit_and_download_rejection() {
    let host = "host-slots-test";
    // 拿光 4 個 permit
    let p1 = acquire_slot(host, Duration::from_millis(100)).await.unwrap();
    let p2 = acquire_slot(host, Duration::from_millis(100)).await.unwrap();
    let p3 = acquire_slot(host, Duration::from_millis(100)).await.unwrap();
    let p4 = acquire_slot(host, Duration::from_millis(100)).await.unwrap();

    // 第 5 個 slot 拿不到立即 429
    let sem = host_slot(host);
    assert!(sem.try_acquire_owned().is_err());

    // 逾時測試
    let err = acquire_slot(host, Duration::from_millis(50)).await.unwrap_err();
    assert_eq!(err, RfsError::Unavailable);

    drop(p1);
    drop(p2);
    drop(p3);
    drop(p4);
}

#[tokio::test]
async fn macos_local_bsd_helpers_check() {
    let script = script_common_header();
    assert!(script.contains("am_same"));
    assert!(script.contains("am_singlelink"));
    assert!(script.contains("am_enter_dir"));
}

struct TestSiteEnv {
    pool: sqlx::SqlitePool,
    fences: std::collections::HashMap<String, HostFence>,
    data_dir: std::path::PathBuf,
}

impl super::site::SiteEnv for TestSiteEnv {
    fn db_pool(&self) -> &sqlx::SqlitePool {
        &self.pool
    }

    fn host_conn(&self, host: &str) -> impl std::future::Future<Output = Option<std::sync::Arc<HostConn>>> + Send {
        let conn = self.fences.get(host).map(|f| f.conn().clone());
        async move { conn }
    }

    fn host_fence(&self, host: &str) -> impl std::future::Future<Output = Option<HostFence>> + Send {
        let fence = self.fences.get(host).cloned();
        async move { fence }
    }

    fn instance(&self) -> Option<String> {
        None
    }

    fn data_dir(&self) -> &Path {
        &self.data_dir
    }
}

#[tokio::test]
async fn site_resolve_tests() {
    let scratch = test_dirs::scratch_dir("site-resolve-test");
    let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();

    sqlx::query("CREATE TABLE projects (id TEXT PRIMARY KEY, host TEXT NOT NULL, path TEXT NOT NULL, deleted_at TEXT)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE bots (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, cwd TEXT, deleted_at TEXT)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE shared_bots (bot_id TEXT PRIMARY KEY, profile TEXT NOT NULL, workspace TEXT NOT NULL, created_at TEXT NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();

    // 1. 本地分享 bot
    sqlx::query("INSERT INTO projects VALUES ('p-local', 'local', '/tmp/p-local', NULL)").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO bots VALUES ('blocal1', 'p-local', '/tmp/p-local', NULL)").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO shared_bots VALUES ('blocal1', 'restricted', '/tmp/ws-local', 'now')").execute(&pool).await.unwrap();

    // 2. 遠端正常分享 bot
    sqlx::query("INSERT INTO projects VALUES ('p-remote', 'rem-1', '/home/u/p-rem', NULL)").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO bots VALUES ('bremote1', 'p-remote', '/home/u/p-rem', NULL)").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO shared_bots VALUES ('bremote1', 'restricted', '/home/u/ws-rem', 'now')").execute(&pool).await.unwrap();

    // 3. 遠端斷線分享 bot
    sqlx::query("INSERT INTO projects VALUES ('p-down', 'rem-down', '/home/u/p-down', NULL)").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO bots VALUES ('bdown1', 'p-down', '/home/u/p-down', NULL)").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO shared_bots VALUES ('bdown1', 'restricted', '/home/u/ws-down', 'now')").execute(&pool).await.unwrap();

    let mut fences = std::collections::HashMap::new();

    // 連線中的主機
    let rem_site = make_test_remote_site(&scratch, "rem-1");
    fences.insert("rem-1".to_string(), rem_site.conn.fence().clone());

    // 斷線的主機
    let cfg_down = HostCfg {
        name: "rem-down".to_string(),
        ssh: "fake-down".to_string(),
        ssh_port: 22,
        ssh_opts: vec![],
        herdr_session: "sess-down".to_string(),
        shared_session: false,
        remote_path: String::new(),
    };
    let conn_down = HostConn::remote(cfg_down, None);
    conn_down.connected.store(false, Ordering::SeqCst);
    fences.insert("rem-down".to_string(), HostFence::for_conn_for_test(&conn_down));

    let env = TestSiteEnv {
        pool,
        fences,
        data_dir: scratch.join("data_dir"),
    };

    // 解析本機 bot
    let site_local = super::site::resolve(&env, "blocal1").await.unwrap();
    match site_local {
        super::site::ShareSite::Local { workspace, .. } => {
            assert_eq!(workspace, PathBuf::from("/tmp/ws-local"));
        }
        _ => panic!("expected Local site"),
    }

    // 解析遠端 bot
    let site_remote = super::site::resolve(&env, "bremote1").await.unwrap();
    match site_remote {
        super::site::ShareSite::Remote(r) => {
            assert_eq!(r.host, "rem-1");
            assert_eq!(r.workspace, "/home/u/ws-rem");
            assert!(r.outbox.contains("outbox/bremote1"));
        }
        _ => panic!("expected Remote site"),
    }

    // 斷線遠端主機
    let err_down = super::site::resolve(&env, "bdown1").await.unwrap_err();
    assert_eq!(err_down, super::site::SiteError::Unavailable);

    // 不存在的 bot
    let err_missing = super::site::resolve(&env, "bghost1").await.unwrap_err();
    assert_eq!(err_missing, super::site::SiteError::NotShareBot);
}

/// 組一段 `parse_outbox_list` 吃的 RFS 框（每筆 `ENTRY <len>` 後面接那一行）。
fn outbox_frames(lines: &[&str]) -> Vec<u8> {
    let mut out = format!("{RFS_VERSION_TAG}\n").into_bytes();
    for line in lines {
        out.extend(format!("ENTRY {}\n{}\n", line.len(), line).as_bytes());
    }
    out.extend(format!("{RFS_DONE_TAG}\n").as_bytes());
    out
}

#[test]
fn remote_outbox_withholds_uppercase_secret_names() {
    // 大小寫混寫的憑證檔名不能靠大小寫敏感的黑名單漏過去（#981）。
    let out = outbox_frames(&[
        "10 1 1\t00\tID_RSA",
        "10 1 1\t00\tKey.PEM",
        "10 1 1\t00\tAUTH.JSON",
        "10 1 1\t00\tok.txt",
    ]);
    let listed = parse_outbox_list(&out, 0).unwrap();
    let names: Vec<&str> = listed.iter().map(|f| f["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["ok.txt"]);
}

#[tokio::test]
async fn remote_outbox_download_refuses_uppercase_secret_names() {
    let scratch = test_dirs::scratch_dir("rfs-upper-secret");
    let site = make_test_remote_site(&scratch, "host-upper-secret-1");
    fs::write(Path::new(&site.outbox).join("SECRET.PEM"), b"hi").unwrap();

    // `RemoteFile`／`ReadWithStat` 沒有 Debug，不能用 unwrap_err，改成比對錯誤種類。
    assert!(matches!(site.outbox_stream("SECRET.PEM", 100).await, Err(ShareFileError::NotFound)));
    assert!(matches!(site.outbox_read("SECRET.PEM", 100).await, Err(ShareFileError::NotFound)));
}

#[tokio::test]
async fn remote_instructions_survive_a_cut_multibyte_char() {
    // 36000 位元組的中文檔：`head -c 32768` 會切在多位元組字中間（#983）。
    let scratch = test_dirs::scratch_dir("rfs-instr-utf8");
    let site = make_test_remote_site(&scratch, "host-instr-utf8-1");
    fs::write(Path::new(&site.workspace).join("CLAUDE.md"), "中".repeat(12000)).unwrap();

    let s = site.instructions().await.unwrap();
    assert!(s.contains("中中中"));
}

#[tokio::test]
async fn remote_outbox_read_returns_non_utf8_bytes_verbatim() {
    let scratch = test_dirs::scratch_dir("rfs-read-nonutf8");
    let site = make_test_remote_site(&scratch, "host-read-nonutf8-1");
    fs::write(Path::new(&site.outbox).join("bad.svg"), b"<svg>\xff</svg>").unwrap();

    let r = site.outbox_read("bad.svg", 1 << 20).await.unwrap();
    assert_eq!(r.data, b"<svg>\xff</svg>");
}

#[tokio::test]
async fn remote_outbox_list_skips_only_the_non_utf8_name() {
    use std::os::unix::ffi::OsStrExt as _;
    let scratch = test_dirs::scratch_dir("rfs-list-nonutf8");
    let site = make_test_remote_site(&scratch, "host-list-nonutf8-1");
    let ob = Path::new(&site.outbox);
    fs::write(ob.join("ok.txt"), b"ok").unwrap();
    fs::write(ob.join(std::ffi::OsStr::from_bytes(b"bad\xff.txt")), b"x").unwrap();

    let list = site.outbox_list(1_000).await.unwrap();
    let names: Vec<&str> = list.iter().map(|f| f["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"ok.txt"), "{names:?}");
    assert!(!names.iter().any(|n| n.contains("bad")), "{names:?}");
}

#[tokio::test]
async fn remote_photo_fetch_stops_at_the_total_before_sending() {
    // 合計上限 7000：前兩張（各 3000）送得出去，第三張會超過，遠端就不能把它送出來（#987）。
    let scratch = test_dirs::scratch_dir("rfs-photo-total");
    let site = make_test_remote_site(&scratch, "host-photo-total-1");
    let inbox = Path::new(&site.workspace).join("inbox");
    fs::create_dir_all(&inbox).unwrap();
    for name in ["a.jpg", "b.jpg", "c.jpg"] {
        fs::write(inbox.join(name), vec![b'x'; 3000]).unwrap();
    }
    let rels: Vec<Vec<String>> = ["a.jpg", "b.jpg", "c.jpg"].iter().map(|n| vec!["inbox".to_string(), n.to_string()]).collect();

    let got = site.photo_fetch(&rels, 5000, 7000).await.unwrap();
    let sizes: Vec<Result<usize, &str>> = got.iter().map(|r| r.as_ref().map(|v| v.len()).map_err(|e| *e)).collect();
    assert_eq!(sizes, vec![Ok(3000), Ok(3000), Err("source_too_large")]);

    // 腳本本身：第三張根本沒有送出來，stdout 只剩前兩張＋框頭。
    let script = photo_fetch_script(&site.workspace, &rels, 5000, 7000);
    let out = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(&script)
        .env("HOME", &scratch)
        .env("TMPDIR", &scratch)
        .output()
        .unwrap();
    assert!(out.stdout.len() < 7000 + 256, "stdout {} bytes，第三張不該被送出", out.stdout.len());
}

/// 組一筆 `ENTRY` 框（與 `outbox_list_script` 的輸出同格式）。
fn outbox_entry_frame(payload: &str) -> String {
    format!("ENTRY {}\n{payload}\n", payload.len())
}

#[test]
fn remote_outbox_list_carries_an_opaque_version() {
    let frames = format!(
        "AM_RFS1\n{}{}AM_RFS_DONE\n",
        outbox_entry_frame("10 100 100 42 100.000000001 100.000000002\t00\ta.svg"),
        outbox_entry_frame("10 100 100\t00\tb.svg"),
    );
    let entries = parse_outbox_list(frames.as_bytes(), 0).unwrap();
    let a = entries.iter().find(|e| e["name"] == "a.svg").expect("a.svg 要列出");
    assert_eq!(a["version"], "42-10-100.000000001-100.000000002");
    // 舊腳本只有前三欄：仍列出，版本是空字串（字串型別）。
    let b = entries.iter().find(|e| e["name"] == "b.svg").expect("b.svg 要列出");
    assert_eq!(b["version"], "");
}

#[tokio::test]
async fn remote_outbox_list_version_changes_on_same_second_rewrite() {
    let scratch = test_dirs::scratch_dir("rfs-outbox-version");
    let site = make_test_remote_site(&scratch, "host-version-1");
    let ob = Path::new(&site.outbox);

    fs::write(ob.join("a.svg"), b"<svg>one</svg>").unwrap();
    let before = site.outbox_list(100).await.unwrap();
    let v1 = before[0]["version"].as_str().unwrap().to_string();
    assert!(!v1.is_empty(), "{before:?}");

    // 同長度、同一秒內改寫（新 inode、新奈秒時間）：版本要跟著變。
    fs::remove_file(ob.join("a.svg")).unwrap();
    fs::write(ob.join("a.svg"), b"<svg>two</svg>").unwrap();
    let after = site.outbox_list(100).await.unwrap();
    let v2 = after[0]["version"].as_str().unwrap();
    assert_ne!(v1, v2, "{before:?} / {after:?}");
}

#[tokio::test]
async fn remote_downloads_leave_a_slot_for_other_share_ops() {
    let scratch = test_dirs::scratch_dir("rfs-slot-reserve");
    let site = make_test_remote_site(&scratch, "host-reserve-1");
    let ob = Path::new(&site.outbox);
    fs::write(ob.join("big.bin"), vec![7u8; 4096]).unwrap();

    // 下載串流最多拿到 HOST_SHARE_SLOTS - HOST_STREAM_RESERVE 格；之後的下載拿不到，但清單等其他動作仍能用。
    let mut held = Vec::new();
    for _ in 0..(HOST_SHARE_SLOTS - HOST_STREAM_RESERVE) {
        held.push(site.outbox_stream("big.bin", 1 << 20).await.unwrap());
    }
    assert!(matches!(site.outbox_stream("big.bin", 1 << 20).await, Err(ShareFileError::Busy)));
    let list = tokio::time::timeout(Duration::from_secs(1), site.outbox_list(100)).await.expect("清單不能被下載占住");
    assert!(list.is_ok());
    drop(held);
}

#[tokio::test]
async fn concurrent_remote_downloads_bounded_to_stream_reserve() {
    let scratch = test_dirs::scratch_dir("rfs-concurrent-reserve");
    let site = make_test_remote_site(&scratch, "host-concurrent-reserve-1");
    let ob = Path::new(&site.outbox);
    fs::write(ob.join("concurrent.bin"), vec![9u8; 4096]).unwrap();

    let site = Arc::new(site);
    let barrier = Arc::new(tokio::sync::Barrier::new(4));
    let mut handles = Vec::new();

    for _ in 0..4 {
        let site = site.clone();
        let barrier = barrier.clone();
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            site.outbox_stream("concurrent.bin", 1 << 20).await
        }));
    }

    let mut successes = 0;
    let mut busies = 0;
    let mut held = Vec::new();
    for h in handles {
        match h.await.unwrap() {
            Ok(file) => {
                successes += 1;
                held.push(file);
            }
            Err(ShareFileError::Busy) => {
                busies += 1;
            }
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    assert_eq!(successes, HOST_SHARE_SLOTS - HOST_STREAM_RESERVE, "最多只能有 3 個下載成功");
    assert_eq!(busies, 1, "第 4 個併發下載必須回傳 Busy");

    // 此時仍有 1 個 slot 留給非下載操作（如 outbox_list）
    let list = tokio::time::timeout(Duration::from_secs(1), site.outbox_list(100))
        .await
        .expect("清單操作不應被併發下載占滿阻塞");
    assert!(list.is_ok());

    drop(held);
}

#[test]
fn resolve_folder_length_is_bytes_under_a_utf8_bash() {
    let tmp = test_dirs::scratch_dir("rfs-utf8-bash");
    let target = tmp.join("中文資料夾");
    fs::create_dir_all(&target).unwrap();
    let path = target.to_str().unwrap();

    let script = resolve_folder_script(path, None);
    let output = match std::process::Command::new("bash")
        .arg("--posix")
        .arg("-c")
        .arg(&script)
        .env("LC_ALL", "C.UTF-8")
        .env("HOME", &tmp)
        .output()
    {
        Ok(out) => out,
        Err(_) => return,
    };

    let r = parse_resolve_folder(&output.stdout).expect("parse resolve folder under utf-8 bash");
    assert!(r.physical.ends_with("中文資料夾"));
}

#[tokio::test]
async fn remote_photo_fd_bound_traversal_prevents_symlink_swap_and_hardlink() {
    let scratch = test_dirs::scratch_dir("rfs-photo-fd-bound");
    let site = make_test_remote_site(&scratch, "host-photo-fd-1");
    let ws = Path::new(&site.workspace);

    // 1. 外部目錄與 canary 照片
    let outside = scratch.join("outside");
    fs::create_dir_all(&outside).unwrap();
    let canary = outside.join("canary.png");
    fs::write(&canary, b"CANARY_SECRET_BYTES").unwrap();

    // 2. 正常兩層巢狀目錄與圖片
    let nested = ws.join("photos/2026");
    fs::create_dir_all(&nested).unwrap();
    let nested_img = nested.join("pic.png");
    fs::write(&nested_img, b"nested-ok-bytes").unwrap();

    // 3. 一層與兩層 ancestor symlink
    let sym_ancestor = ws.join("sym_dir");
    unix_fs::symlink(&outside, &sym_ancestor).unwrap();

    let nested_outside = ws.join("nested_sym");
    fs::create_dir_all(&nested_outside).unwrap();
    unix_fs::symlink(&outside, nested_outside.join("sub")).unwrap();

    // 4. Leaf symlink
    let sym_leaf = ws.join("sym_leaf.png");
    unix_fs::symlink(&canary, &sym_leaf).unwrap();

    // 5. Hardlink (st_nlink > 1)
    let hl1 = ws.join("hardlink1.png");
    let hl2 = ws.join("hardlink2.png");
    fs::write(&hl1, b"hardlink-bytes").unwrap();
    fs::hard_link(&hl1, &hl2).unwrap();

    // 執行 photo_stats 與 photo_fetch
    let rel_ok = vec!["photos".to_string(), "2026".to_string(), "pic.png".to_string()];
    let rel_sym_anc = vec!["sym_dir".to_string(), "canary.png".to_string()];
    let rel_sym_anc2 = vec!["nested_sym".to_string(), "sub".to_string(), "canary.png".to_string()];
    let rel_sym_leaf = vec!["sym_leaf.png".to_string()];
    let rel_hardlink = vec!["hardlink1.png".to_string()];

    let all_rels = vec![
        rel_ok.clone(),
        rel_sym_anc.clone(),
        rel_sym_anc2.clone(),
        rel_sym_leaf.clone(),
        rel_hardlink.clone(),
    ];

    let stats = site.photo_stats(&all_rels).await.unwrap();
    assert!(stats[0].is_some(), "正常巢狀圖片必須 stat 成功");
    assert_eq!(stats[0].as_ref().unwrap().size, 15);
    assert!(stats[1].is_none(), "ancestor symlink 必須被拒絕");
    assert!(stats[2].is_none(), "兩層 ancestor symlink 必須被拒絕");
    assert!(stats[3].is_none(), "leaf symlink 必須被拒絕");
    assert!(stats[4].is_none(), "hardlink 必須被拒絕");

    let fetched = site.photo_fetch(&all_rels, 100, 200).await.unwrap();
    assert_eq!(fetched[0].as_ref().unwrap(), b"nested-ok-bytes");
    assert_eq!(fetched[1], Err("not_found"), "絕不讀取外部 canary");
    assert_eq!(fetched[2], Err("not_found"), "絕不讀取外部 canary (兩層)");
    assert_eq!(fetched[3], Err("not_found"), "絕不讀取 leaf symlink");
    assert_eq!(fetched[4], Err("not_found"), "絕不讀取 hardlink");
}

#[test]
fn parse_photo_stats_subsecond_gnu_and_bsd_formats() {
    // 1. 純整數奈秒
    let p_int = b"0\t42 100 1700000000123456789";
    let raw_int = format!("AM_RFS1\nSTAT {}\n{}\nAM_RFS_DONE\n", p_int.len(), std::str::from_utf8(p_int).unwrap());
    let stats = parse_photo_stats(raw_int.as_bytes(), 1).unwrap();
    assert_eq!(stats[0].as_ref().unwrap().mtime_ns, 1700000000123456789);

    // 2. GNU stat %.9Y 小數格式
    let p_gnu = b"0\t42 100 1700000000.123456789";
    let raw_gnu = format!("AM_RFS1\nSTAT {}\n{}\nAM_RFS_DONE\n", p_gnu.len(), std::str::from_utf8(p_gnu).unwrap());
    let stats = parse_photo_stats(raw_gnu.as_bytes(), 1).unwrap();
    assert_eq!(stats[0].as_ref().unwrap().mtime_ns, 1700000000123456789);

    // 3. BSD stat %Fm 小數格式（較少位數需補齊到 9 位）
    let p_bsd = b"0\t42 100 1700000000.5";
    let raw_bsd = format!("AM_RFS1\nSTAT {}\n{}\nAM_RFS_DONE\n", p_bsd.len(), std::str::from_utf8(p_bsd).unwrap());
    let stats = parse_photo_stats(raw_bsd.as_bytes(), 1).unwrap();
    assert_eq!(stats[0].as_ref().unwrap().mtime_ns, 1700000000500000000);
}

#[tokio::test]
async fn remote_photo_same_second_rewrite_different_subsecond_invalidates_cache() {
    let scratch = test_dirs::scratch_dir("rfs-photo-subsecond");
    let site = make_test_remote_site(&scratch, "host-photo-subsecond-1");
    let inbox = Path::new(&site.workspace).join("inbox");
    fs::create_dir_all(&inbox).unwrap();

    let img_a = {
        let img = image::RgbImage::from_fn(10, 10, |_, _| image::Rgb([255, 0, 0]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(img).write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Jpeg).unwrap();
        out
    };
    let img_b = {
        let img = image::RgbImage::from_fn(10, 10, |_, _| image::Rgb([0, 0, 255]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(img).write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Jpeg).unwrap();
        out
    };
    assert_eq!(img_a.len(), img_b.len(), "兩張圖片大小必須相同");
    assert_ne!(img_a, img_b, "兩張圖片內容必須不同");

    let pic_path = inbox.join("pic.jpg");
    fs::write(&pic_path, &img_a).unwrap();
    let _ = std::process::Command::new("touch")
        .arg("-m")
        .arg("-d")
        .arg("@1700000000.100000000")
        .arg(&pic_path)
        .status();

    let svg_content = r#"<svg xmlns="http://www.w3.org/2000/svg"><image href="inbox/pic.jpg"/></svg>"#.as_bytes().to_vec();

    // 第一次 embed
    let embedded1 = crate::share::remote_io::embed_remote(&site, svg_content.clone()).await;
    let s1 = String::from_utf8(embedded1).unwrap();
    assert!(!s1.contains("data-am-embed"), "第一次 embed 必須成功: {s1}");

    // 同一秒改寫為 img_b，設定不同奈秒
    fs::write(&pic_path, &img_b).unwrap();
    let _ = std::process::Command::new("touch")
        .arg("-m")
        .arg("-d")
        .arg("@1700000000.200000000")
        .arg(&pic_path)
        .status();

    // 第二次 embed：必須快取失效並輸出 img_b
    let embedded2 = crate::share::remote_io::embed_remote(&site, svg_content).await;
    let s2 = String::from_utf8(embedded2).unwrap();
    assert!(!s2.contains("data-am-embed"), "第二次 embed 必須成功: {s2}");
    assert_ne!(s1, s2, "同秒改寫照片縮圖必須失效並換成新圖");
}

#[tokio::test]
async fn standalone_compose_is_cached_authority_isolation() {
    let scratch_a = test_dirs::scratch_dir("rfs-cache-auth-a");
    let site_a = make_test_remote_site(&scratch_a, "host-auth-test");

    let scratch_b = test_dirs::scratch_dir("rfs-cache-auth-b");
    let site_b = make_test_remote_site(&scratch_b, "host-auth-test");

    let fence_a = site_a.conn.fence();
    let fence_b = site_b.conn.fence();

    let scope_a = crate::share::compose::PrefetchedSource::scope_for(fence_a, "/home/u/ws");
    let scope_b = crate::share::compose::PrefetchedSource::scope_for(fence_b, "/home/u/ws");
    assert_ne!(scope_a, scope_b, "不同連線實例的 scope 必須隔離");

    let rel = vec!["inbox".to_string(), "a.png".to_string()];
    let meta = crate::share::compose::PhotoMeta { ino: 1234, len: 5678, mtime_ns: 999999 };

    // 尚未快取
    assert!(!crate::share::compose::is_cached(&scope_a, &rel, &meta));
    assert!(!crate::share::compose::is_cached(&scope_b, &rel, &meta));

    // 使用 scope_a 執行一次 embed
    let img_bytes = {
        let img = image::RgbImage::from_fn(10, 10, |_, _| image::Rgb([100, 100, 100]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(img).write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Jpeg).unwrap();
        out
    };
    let src_a = crate::share::compose::PrefetchedSource::new(
        &fence_a.authority_scope(),
        "/home/u/ws",
        std::slice::from_ref(&rel),
        &[Some(meta)],
        vec![Some(Ok(img_bytes))],
    );
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><image href="inbox/a.png"/></svg>"#.as_bytes();
    let _ = crate::share::compose::embed_with(svg, &src_a);

    // scope_a 命中快取，但 scope_b 絕不命中
    assert!(crate::share::compose::is_cached(&scope_a, &rel, &meta), "同一個 authority 必須 cache hit");
    assert!(!crate::share::compose::is_cached(&scope_b, &rel, &meta), "不同 authority 絕不可誤命中舊快取");
}

#[tokio::test]
async fn repoint_host_invalidates_remote_svg_photo_cache() {
    let scratch_a = test_dirs::scratch_dir("rfs-repoint-a");
    let site_a = make_test_remote_site(&scratch_a, "repoint-host");

    let scratch_b = test_dirs::scratch_dir("rfs-repoint-b");
    let site_b = make_test_remote_site(&scratch_b, "repoint-host");

    let img_a = {
        let img = image::RgbImage::from_fn(10, 10, |_, _| image::Rgb([255, 0, 0]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(img).write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Jpeg).unwrap();
        out
    };
    let img_b = {
        let img = image::RgbImage::from_fn(10, 10, |_, _| image::Rgb([0, 255, 0]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(img).write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Jpeg).unwrap();
        out
    };

    let p_a = Path::new(&site_a.workspace).join("inbox/pic.jpg");
    let p_b = Path::new(&site_b.workspace).join("inbox/pic.jpg");
    fs::create_dir_all(p_a.parent().unwrap()).unwrap();
    fs::create_dir_all(p_b.parent().unwrap()).unwrap();
    fs::write(&p_a, &img_a).unwrap();
    fs::write(&p_b, &img_b).unwrap();

    // 刻意將兩台主機上的檔案時間設定為完全相同
    let _ = std::process::Command::new("touch").arg("-m").arg("-d").arg("@1700000000.500000000").arg(&p_a).status();
    let _ = std::process::Command::new("touch").arg("-m").arg("-d").arg("@1700000000.500000000").arg(&p_b).status();

    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><image href="inbox/pic.jpg"/></svg>"#.as_bytes().to_vec();

    // 主機 A render
    let out_a = crate::share::remote_io::embed_remote(&site_a, svg.clone()).await;
    let s_a = String::from_utf8(out_a).unwrap();
    assert!(!s_a.contains("data-am-embed"), "主機 A embed 成功: {s_a}");

    // Repoint 到主機 B render：即便路徑、大小、mtime 完全一樣，但主機不同，必須輸出主機 B 的圖片，絕不可沿用主機 A 的快取
    let out_b = crate::share::remote_io::embed_remote(&site_b, svg).await;
    let s_b = String::from_utf8(out_b).unwrap();
    assert!(!s_b.contains("data-am-embed"), "主機 B embed 成功: {s_b}");
    assert_ne!(s_a, s_b, "repoint 後的圖片必須來自新主機 B，不得沿用舊主機 A 的快取");
}




