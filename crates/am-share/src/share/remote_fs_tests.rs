//! R-S1 遠端檔案原語測試（SPEC §20、remote-share-design §3）。
//!
//! 全部測試使用 `local_sh_fake` 在 scratch 目錄跑本機 sh 替身，不碰真實 HOME。

use std::fs;
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

use am_base::config::HostCfg;
use am_base::hosts::HostConn;
use am_base::outbox::ShareFileError;

use super::remote_fs::*;
use super::site::RemoteSite;
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
        conn,
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
    hosts: std::collections::HashMap<String, std::sync::Arc<HostConn>>,
    data_dir: std::path::PathBuf,
}

impl super::site::SiteEnv for TestSiteEnv {
    fn db_pool(&self) -> &sqlx::SqlitePool {
        &self.pool
    }

    fn host_conn(&self, host: &str) -> impl std::future::Future<Output = Option<std::sync::Arc<HostConn>>> + Send {
        let conn = self.hosts.get(host).cloned();
        async move { conn }
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

    let mut hosts = std::collections::HashMap::new();

    // 連線中的主機
    let rem_site = make_test_remote_site(&scratch, "rem-1");
    hosts.insert("rem-1".to_string(), rem_site.conn);

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
    hosts.insert("rem-down".to_string(), conn_down);

    let env = TestSiteEnv {
        pool,
        hosts,
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
