//! #955 R-S2：遠端專案的分享 bot 怎麼建、怎麼啟動、怎麼刪（remote-share-design §4、§7）。
//!
//! 假主機：`remote_fs::test_support::local_sh_fake` 把 ssh 腳本交給本機 `/bin/sh` 跑，`HOME` 指到 scratch 目錄；
//! 只有 `claude --version` 與 managed settings 的檢查另外回測試給的值（可在測試中途換）。所有送出去的腳本都記下來。
//! 不碰真實 HOME。

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::*;
use crate::config::HostCfg;
use crate::db;
use crate::lifecycle::LcError;
use crate::share::remote_fs::{self, PreflightError};
use crate::testing as tt;

/// 假的遠端主機：連線、家目錄（scratch）、版本與 managed 狀態（測試中途可換），以及送出過的腳本。
struct FakeHost {
    host: String,
    home: PathBuf,
    version: Arc<Mutex<String>>,
    managed: Arc<Mutex<bool>>,
    scripts: Arc<Mutex<Vec<String>>>,
}

impl FakeHost {
    fn sent(&self) -> Vec<String> {
        self.scripts.lock().unwrap().clone()
    }
}

/// `version`：空字串＝找不到 claude。
async fn fake_host(e: &tt::Env, tag: &str, version: &str, managed: bool) -> FakeHost {
    let home = tt::scratch_dir(&format!("rs2-{tag}"));
    // 主機名稱上限 32 字（`[a-z][a-z0-9_-]{0,31}`）：標籤＋ULID 前 8 碼。
    let host = format!("rs2-{tag}-{}", &db::ulid().to_lowercase()[..8]);
    remote_fs::test_support::local_sh_fake(&host, &home);
    let version = Arc::new(Mutex::new(version.to_string()));
    let managed = Arc::new(Mutex::new(managed));
    let scripts = Arc::new(Mutex::new(Vec::new()));
    let (v, m, rec, sh_home) = (version.clone(), managed.clone(), scripts.clone(), home.clone());
    am_base::hosts::set_ssh_fake(&host, move |script: &str| {
        rec.lock().unwrap().push(script.to_string());
        if script.contains("claude --version") {
            let v = v.lock().unwrap().clone();
            return Ok(if v.is_empty() { "NOCLAUDE".to_string() } else { format!("{v} (Claude Code)\n") });
        }
        if script.contains("managed-settings") {
            return Ok(if *m.lock().unwrap() { "FOUND\n".to_string() } else { "NONE\n".to_string() });
        }
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .env("HOME", &sh_home)
            .env("TMPDIR", &sh_home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| anyhow::anyhow!("exec /bin/sh: {e}"))?;
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    });
    let cfg = HostCfg {
        name: host.clone(),
        ssh: "fake-ssh".into(),
        ssh_port: 22,
        ssh_opts: vec![],
        herdr_session: "test-session".into(),
        shared_session: false,
        remote_path: String::new(),
    };
    let conn = e.app.hosts.insert_remote_for_test(cfg).await;
    conn.connected.store(true, Ordering::SeqCst);
    FakeHost { host, home, version, managed, scripts }
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

/// `LcError` 的 `reason`（409 的 `reason`、400 的 `reason`）。
fn reason(e: &LcError) -> String {
    match e {
        LcError::Conflict(v) | LcError::BadValue(v) => v["reason"].as_str().unwrap_or_default().to_string(),
        other => format!("{other:?}"),
    }
}

async fn bind_project_host(e: &tt::Env, host: &str) {
    sqlx::query("UPDATE projects SET host = ? WHERE id = ?").bind(host).bind(&e.project_id).execute(&e.app.db).await.unwrap();
}

// ───────────────────────── 版本與 managed settings（純函式） ─────────────────────────

#[test]
fn claude_versions_compare_segment_by_segment() {
    assert_eq!(remote_fs::parse_claude_version("2.1.288 (Claude Code)\n").as_deref(), Some("2.1.288"));
    assert_eq!(remote_fs::parse_claude_version("NOCLAUDE"), None);
    assert!(remote_fs::version_at_least("2.1.288", "2.1.288"));
    assert!(remote_fs::version_at_least("2.1.289", "2.1.288"));
    assert!(remote_fs::version_at_least("2.10.0", "2.9.9"), "逐段比，不是字串比");
    assert!(!remote_fs::version_at_least("2.1.287", "2.1.288"), "2.1.287 是舊版（R-S2 §8 的例子）");
    assert!(!remote_fs::version_at_least("", "2.1.288"));
    assert!(!remote_fs::version_at_least("abc", "2.1.288"));
}

// ───────────────────────── 建資料夾 ─────────────────────────

/// 新資料夾建在遠端家目錄的 `shared-bots/` 底下，inbox 0700，`shared_bots` 記的是遠端的實體路徑。
#[tokio::test]
async fn a_new_remote_folder_is_made_in_the_remote_home_with_its_inbox() {
    let e = tt::env().await;
    let f = fake_host(&e, "new-ok", "2.1.288", false).await;
    let b = tt::claude_bot(&e.app, &e.project_id, "rs2-new").await;
    let folder = folder::ShareFolderIn::New { name: "support".into() };
    let (ws, made) = admin::reserve_share_bot_on(&e.app, &f.host, &b.id, store::PROFILE_RESTRICTED, &folder, false).await.unwrap();
    assert!(made);
    let dir = std::fs::canonicalize(f.home.join("shared-bots/support")).unwrap();
    assert_eq!(ws, dir.to_string_lossy());
    assert_eq!(mode(&dir.join("inbox")), 0o700);
    assert_eq!(store::workspace(&e.app.db, &b.id).await.unwrap().as_deref(), Some(ws.as_str()));
}

/// 既有資料夾：一般的專案目錄可以；家目錄本身、帳號目錄、daemon 的資料目錄、符號連結指到 `~/.ssh`、相對路徑一律 400。
#[tokio::test]
async fn remote_existing_folders_are_checked_like_local_ones() {
    let e = tt::env().await;
    let f = fake_host(&e, "exist", "2.1.288", false).await;
    let site = f.home.join("project/docs");
    std::fs::create_dir_all(&site).unwrap();
    let b = tt::claude_bot(&e.app, &e.project_id, "rs2-exist").await;
    let (ws, made) = admin::reserve_share_bot_on(
        &e.app,
        &f.host,
        &b.id,
        store::PROFILE_RESTRICTED,
        &folder::ShareFolderIn::Existing { path: site.to_string_lossy().into() },
        false,
    )
    .await
    .unwrap();
    assert!(!made, "既有資料夾不算這次建的");
    assert_eq!(ws, std::fs::canonicalize(&site).unwrap().to_string_lossy());

    std::fs::create_dir_all(f.home.join(".ssh")).unwrap();
    std::fs::create_dir_all(f.home.join(".config/agents-manager")).unwrap();
    std::os::unix::fs::symlink(f.home.join(".ssh"), f.home.join("sneaky")).unwrap();
    let refused: Vec<(String, PathBuf)> = vec![
        (".".into(), f.home.clone()),
        (".ssh".into(), f.home.join(".ssh")),
        (".config/agents-manager (資料目錄)".into(), f.home.join(".config/agents-manager")),
        ("符號連結指到 ~/.ssh".into(), f.home.join("sneaky")),
        ("相對路徑".into(), PathBuf::from("relative/dir")),
    ];
    for (i, (what, path)) in refused.iter().enumerate() {
        let bi = tt::claude_bot(&e.app, &e.project_id, &format!("rs2-bad-{i}")).await;
        let err = admin::reserve_share_bot_on(
            &e.app,
            &f.host,
            &bi.id,
            store::PROFILE_RESTRICTED,
            &folder::ShareFolderIn::Existing { path: path.to_string_lossy().into() },
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(reason(&err), "bad_share_folder", "{what} 應該被擋：{err:?}");
        assert_eq!(store::workspace(&e.app.db, &bi.id).await.unwrap(), None, "{what} 不留列");
    }
}

/// 新資料夾名字已經有了：409 `folder_exists`，不悄悄共用；重送（`replay`）才拿回那一個，而且不算這次建的。
#[tokio::test]
async fn an_existing_remote_name_is_409_unless_it_is_a_replay() {
    let e = tt::env().await;
    let f = fake_host(&e, "dup", "2.1.288", false).await;
    std::fs::create_dir_all(f.home.join("shared-bots/dup")).unwrap();
    let b = tt::claude_bot(&e.app, &e.project_id, "rs2-dup").await;
    let folder = folder::ShareFolderIn::New { name: "dup".into() };
    let err = admin::reserve_share_bot_on(&e.app, &f.host, &b.id, store::PROFILE_RESTRICTED, &folder, false).await.unwrap_err();
    assert_eq!(reason(&err), "folder_exists", "{err:?}");
    assert_eq!(store::workspace(&e.app.db, &b.id).await.unwrap(), None);
    let (ws, made) = admin::reserve_share_bot_on(&e.app, &f.host, &b.id, store::PROFILE_RESTRICTED, &folder, true).await.unwrap();
    assert!(!made);
    assert!(ws.ends_with("shared-bots/dup"), "{ws}");
}

/// 寫不進 `shared_bots`（同一顆 bot 已經有一列）：這次建的空資料夾只 `rmdir`，而且沒有任何 `rm -rf` 送出去。
#[tokio::test]
async fn a_failed_reservation_only_rmdirs_the_folder_it_made() {
    let e = tt::env().await;
    let f = fake_host(&e, "rollback", "2.1.288", false).await;
    let b = tt::claude_bot(&e.app, &e.project_id, "rs2-roll").await;
    admin::reserve_share_bot_on(&e.app, &f.host, &b.id, store::PROFILE_RESTRICTED, &folder::ShareFolderIn::New { name: "first".into() }, false)
        .await
        .unwrap();
    let err = admin::reserve_share_bot_on(&e.app, &f.host, &b.id, store::PROFILE_RESTRICTED, &folder::ShareFolderIn::New { name: "second".into() }, false).await;
    assert!(err.is_err(), "同一顆 bot 第二次寫入 shared_bots 會失敗");
    assert!(!f.home.join("shared-bots/second").exists(), "這次建的空資料夾要收掉");
    assert!(f.home.join("shared-bots/first").exists(), "之前那一顆不動");
    assert!(f.sent().iter().all(|s| !s.contains("rm -rf") && !s.contains("rm -r ")), "遠端從不 rm -rf");
}

/// 主機斷線：409 `share_host_unreachable`，`shared_bots` 沒有列、資料夾也沒建。
#[tokio::test]
async fn an_unreachable_host_is_409_and_writes_nothing() {
    let e = tt::env().await;
    let f = fake_host(&e, "down", "2.1.288", false).await;
    e.app.hosts.get(&f.host).await.unwrap().connected.store(false, Ordering::SeqCst);
    let b = tt::claude_bot(&e.app, &e.project_id, "rs2-down").await;
    let err = admin::reserve_share_bot_on(&e.app, &f.host, &b.id, store::PROFILE_RESTRICTED, &folder::ShareFolderIn::New { name: "x".into() }, false)
        .await
        .unwrap_err();
    assert_eq!(reason(&err), "share_host_unreachable", "{err:?}");
    assert_eq!(store::workspace(&e.app.db, &b.id).await.unwrap(), None);
    assert!(!f.home.join("shared-bots/x").exists());
}

// ───────────────────────── preflight（受限才查 claude 與 managed） ─────────────────────────

/// claude 太舊或讀不到、有 managed settings：受限建立 409；信任分享不查（本來就沒有籠子）。
#[tokio::test]
async fn preflight_refuses_old_claude_and_managed_settings_for_restricted_only() {
    let e = tt::env().await;
    let old = fake_host(&e, "pf-old", "2.1.287", false).await;
    let b = tt::claude_bot(&e.app, &e.project_id, "rs2-pf").await;
    let err = admin::reserve_share_bot_on(&e.app, &old.host, &b.id, store::PROFILE_RESTRICTED, &folder::ShareFolderIn::New { name: "a".into() }, false)
        .await
        .unwrap_err();
    assert_eq!(reason(&err), "share_claude_too_old", "{err:?}");
    assert!(!old.home.join("shared-bots/a").exists(), "preflight 不過就不建資料夾");
    let site = old.home.join("site");
    std::fs::create_dir_all(&site).unwrap();
    admin::reserve_share_bot_on(&e.app, &old.host, &b.id, store::PROFILE_TRUSTED, &folder::ShareFolderIn::Existing { path: site.to_string_lossy().into() }, false)
        .await
        .expect("信任分享不查 claude 版本");

    let missing = fake_host(&e, "pf-none", "", false).await;
    let conn = e.app.hosts.get(&missing.host).await.unwrap();
    assert!(matches!(remote_fs::preflight_restricted(&conn, cage::MIN_CLAUDE).await, Err(PreflightError::ClaudeTooOld { .. })), "讀不到 claude 也不行");

    let managed = fake_host(&e, "pf-managed", "2.1.288", true).await;
    let b2 = tt::claude_bot(&e.app, &e.project_id, "rs2-pf2").await;
    let err = admin::reserve_share_bot_on(&e.app, &managed.host, &b2.id, store::PROFILE_RESTRICTED, &folder::ShareFolderIn::New { name: "m".into() }, false)
        .await
        .unwrap_err();
    assert_eq!(reason(&err), "share_managed_settings", "{err:?}");
    let site2 = managed.home.join("site2");
    std::fs::create_dir_all(&site2).unwrap();
    admin::reserve_share_bot_on(&e.app, &managed.host, &b2.id, store::PROFILE_TRUSTED, &folder::ShareFolderIn::Existing { path: site2.to_string_lossy().into() }, false)
        .await
        .expect("信任分享不查 managed settings");
}

// ───────────────────────── 啟動 ─────────────────────────

/// 每次啟動都重跑 preflight：建好之後 claude 被換成舊版，這次啟動就不起來；換回新版又能起。
#[tokio::test]
async fn starting_a_remote_restricted_bot_reruns_the_preflight() {
    let e = tt::env().await;
    let f = fake_host(&e, "start", "2.1.288", false).await;
    bind_project_host(&e, &f.host).await;
    let b = tt::claude_bot(&e.app, &e.project_id, "rs2-start").await;
    let (ws, made) = admin::reserve_share_bot_on(&e.app, &f.host, &b.id, store::PROFILE_RESTRICTED, &folder::ShareFolderIn::New { name: "st".into() }, false)
        .await
        .unwrap();
    admin::finish_restricted(&e.app, &b.id, &ws, made, true).await;
    let bot = db::bot(&e.app.db, &b.id).await.unwrap().unwrap();

    assert_eq!(cage::prepare(&e.app, &bot, &f.host).await.unwrap().as_deref(), Some(ws.as_str()));
    assert!(f.sent().iter().any(|s| s.contains("claude --version")), "啟動時也查版本");

    *f.version.lock().unwrap() = "2.1.100".into();
    let err = cage::prepare(&e.app, &bot, &f.host).await.unwrap_err();
    assert_eq!(reason(&err), "share_claude_too_old", "{err:?}");
    *f.version.lock().unwrap() = "2.1.288".into();
    *f.managed.lock().unwrap() = true;
    let err = cage::prepare(&e.app, &bot, &f.host).await.unwrap_err();
    assert_eq!(reason(&err), "share_managed_settings", "{err:?}");
    *f.managed.lock().unwrap() = false;
    assert!(cage::prepare(&e.app, &bot, &f.host).await.unwrap().is_some());
}

/// 遠端的系統提示寫進遠端的 bot 目錄，寫完比對 sha256，檔案 0600，內容不帶結尾換行（shell 會吃掉結尾換行、驗不過）。
#[tokio::test]
async fn the_remote_system_prompt_is_written_privately_and_verified() {
    let e = tt::env().await;
    let f = fake_host(&e, "prompt", "2.1.288", false).await;
    bind_project_host(&e, &f.host).await;
    let b = tt::claude_bot(&e.app, &e.project_id, "rs2-prompt").await;
    let (ws, made) = admin::reserve_share_bot_on(&e.app, &f.host, &b.id, store::PROFILE_RESTRICTED, &folder::ShareFolderIn::New { name: "pr".into() }, false)
        .await
        .unwrap();
    admin::finish_restricted(&e.app, &b.id, &ws, made, true).await;
    let bot = db::bot(&e.app.db, &b.id).await.unwrap().unwrap();
    let env = json!({"AM_OUTBOX": f.home.join(".config/agents-manager/outbox").to_string_lossy()});
    let path = cage::install_prompt(&e.app, &bot, &f.host, &ws, &env).await.unwrap();
    let want_dir = f.home.join(".config/agents-manager/bots").join(&b.id);
    assert_eq!(PathBuf::from(&path), want_dir.join(cage::PROMPT_FILE));
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("你是透過分享連結開放給外部使用者的助理"), "{text:.60}");
    assert!(!text.ends_with('\n'));
    assert!(text.contains(&ws), "工作目錄寫的是遠端的路徑");
    assert_eq!(mode(Path::new(&path)), 0o600);
    assert_eq!(mode(&want_dir), 0o700);
}

// ───────────────────────── 刪 bot ─────────────────────────

/// 刪遠端分享 bot：遠端資料夾不搬、不刪，也沒有任何腳本碰到它（替身斷言）。
#[tokio::test]
async fn deleting_a_remote_share_bot_never_touches_its_folder() {
    let e = tt::env().await;
    let f = fake_host(&e, "delete", "2.1.288", false).await;
    bind_project_host(&e, &f.host).await;
    let b = tt::claude_bot(&e.app, &e.project_id, "rs2-del").await;
    let (ws, made) = admin::reserve_share_bot_on(&e.app, &f.host, &b.id, store::PROFILE_RESTRICTED, &folder::ShareFolderIn::New { name: "keep-me".into() }, false)
        .await
        .unwrap();
    admin::finish_restricted(&e.app, &b.id, &ws, made, true).await;
    std::fs::write(Path::new(&ws).join("notes.txt"), b"mine").unwrap();
    let before = f.sent().len();
    let _ = crate::lifecycle::purge_bot_dir(&e.app, &b.id, &f.host).await;
    let after = f.sent();
    assert!(after[before..].iter().all(|s| !s.contains(&ws)), "沒有任何腳本碰到遠端的資料夾");
    assert_eq!(std::fs::read(Path::new(&ws).join("notes.txt")).unwrap(), b"mine", "資料夾與裡面的檔案都還在");
}

// ───────────────────────── HTTP：建 bot 的 409 ─────────────────────────

async fn serve(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await;
    });
    format!("http://{addr}")
}

/// 建 bot 的 HTTP 路徑：設定裡沒有的主機 409 `unsupported_host`（找不到，不是「不支援遠端」）；設定裡有、但沒連線 409
/// `share_host_unreachable`。兩種都沒有寫 `shared_bots`，config 也沒有新的 bot。
#[tokio::test]
async fn creating_a_remote_share_bot_over_http_refuses_unknown_and_unreachable_hosts() {
    let e = tt::env().await;
    let pid = e.project_id.clone();
    let f = fake_host(&e, "http", "2.1.288", false).await;
    e.app
        .cfg
        .update(|c| {
            c.hosts.push(HostCfg {
                name: f.host.clone(),
                ssh: "fake-ssh".into(),
                ssh_port: 22,
                ssh_opts: vec![],
                herdr_session: "s".into(),
                shared_session: false,
                remote_path: String::new(),
            });
            c.projects.push(crate::config::ProjectCfg {
                id: Some(pid.clone()),
                path: "/srv/app".into(),
                label: "proj".into(),
                host: f.host.clone(),
                bots: vec![],
                handed_off_to: None,
            });
            Ok(())
        })
        .await
        .unwrap();
    let base = serve(crate::api::router(e.app.clone())).await;
    let c = reqwest::Client::builder().no_proxy().build().unwrap();
    let create = |body: Value| c.post(format!("{base}/api/projects/{pid}/bots")).header("X-AM-Token", "test-token").json(&body).send();

    bind_project_host(&e, "ghost-host").await;
    let res = create(json!({"name": "g1", "kind": "claude", "share_profile": "restricted"})).await.unwrap();
    assert_eq!(res.status(), 409);
    assert_eq!(res.json::<Value>().await.unwrap()["reason"], "unsupported_host");

    bind_project_host(&e, &f.host).await;
    e.app.hosts.get(&f.host).await.unwrap().connected.store(false, Ordering::SeqCst);
    let res = create(json!({"name": "g2", "kind": "claude", "share_profile": "restricted", "share_folder": {"kind": "new", "name": "g2"}})).await.unwrap();
    assert_eq!(res.status(), 409);
    assert_eq!(res.json::<Value>().await.unwrap()["reason"], "share_host_unreachable");
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM shared_bots").fetch_one(&e.app.db).await.unwrap();
    assert_eq!(n, 0, "沒有 shared_bots 列");
    assert!(e.app.cfg.get().await.projects.iter().all(|p| p.bots.is_empty()), "config 不寫");
}

/// 遠端受限 bot 的 PATH：那台的 `remote_path`（`$HOME` 展開）加標準目錄，沒有 shim 目錄（不能開子 agent）。
#[test]
fn the_remote_cage_path_expands_home_and_has_no_shim_dir() {
    assert_eq!(
        cage::remote_cage_path("$HOME/.local/bin:/opt/x", "/home/m4p"),
        "/home/m4p/.local/bin:/opt/x:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
    );
    assert_eq!(cage::remote_cage_path("", "/home/m4p"), "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin");
}
