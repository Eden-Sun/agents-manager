//! R-S4：遠端專案分享 bot 的預算量測、outbox 保留政策與巡邏（remote-share-design §6、SPEC §20.5）。
//!
//! 腳本部分用 `local_sh_fake` 讓本機 `/bin/sh` 扮演遠端（腳本的每一條擋法都真的被執行），HOME／TMPDIR 指到 scratch 目錄，不碰真實 HOME。
//! `is_full`／`sweep`／keep 標記要整個 `App`，放在 `with_app`（`daemon-test-harness`）。

use std::fs;
use std::os::unix::fs as unix_fs;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};

use am_base::config::HostCfg;
use am_base::hosts::HostConn;

use super::remote_fs::*;
use super::site::RemoteSite;
use super::test_dirs;

fn site(scratch: &Path, host: &str) -> RemoteSite {
    test_support::local_sh_fake(host, scratch);
    let cfg = HostCfg {
        name: host.to_string(),
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
    RemoteSite { conn, host: host.to_string(), home, root, workspace, outbox }
}

fn set_mtime(path: &Path, when: SystemTime) {
    fs::File::options().write(true).open(path).unwrap().set_modified(when).unwrap();
}

fn epoch(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs()
}

const DAY: Duration = Duration::from_secs(86_400);

/// 在假遠端跑保留政策腳本，`now` 往後撥（ctime 沒辦法往回改，只能撥「現在」）。
async fn prune_at(site: &RemoteSite, days: u64, cap_bytes: u64, cap_files: usize, now: Option<u64>) -> Result<u64, RfsError> {
    let script = prune_outbox_script_at(&site.outbox, days, cap_bytes, cap_files, now);
    let out = site.conn.ssh_exec_timeout(&script, Duration::from_secs(60)).await.unwrap();
    parse_prune_outbox(out.as_bytes())
}

fn frame(tag: &str, payload: &str) -> String {
    format!("AM_RFS1\n{tag} {}\n{payload}\nAM_RFS_DONE\n", payload.len())
}

// ───────────────────────── measure ─────────────────────────

#[tokio::test]
async fn measure_counts_workspace_and_outbox_but_not_the_keep_mark_or_symlink_targets() {
    let scratch = test_dirs::scratch_dir("rs4-measure");
    let site = site(&scratch, "rs4-measure-host");
    let (ws, ob) = (Path::new(&site.workspace), Path::new(&site.outbox));
    fs::write(ws.join("a.bin"), vec![1u8; 8192]).unwrap();
    fs::create_dir_all(ws.join("sub")).unwrap();
    fs::write(ws.join("sub/b.bin"), vec![2u8; 4096]).unwrap();
    // 指到工作目錄外的大檔與目錄：既不跟、也不算。
    let outside = test_dirs::scratch_dir("rs4-outside");
    fs::write(outside.join("huge.bin"), vec![3u8; 2 * 1024 * 1024]).unwrap();
    unix_fs::symlink(outside.join("huge.bin"), ws.join("link-file")).unwrap();
    unix_fs::symlink(&outside, ws.join("link-dir")).unwrap();
    fs::write(ob.join("o.bin"), vec![4u8; 4096]).unwrap();
    fs::write(ob.join(".am-share-keep"), b"keep").unwrap();

    let with_mark = site.measure().await.unwrap();
    fs::remove_file(ob.join(".am-share-keep")).unwrap();
    let without_mark = site.measure().await.unwrap();

    assert_eq!(with_mark, without_mark, ".am-share-keep 的位元組與檔數都不算");
    assert_eq!(with_mark.files, 3, "a.bin、b.bin、o.bin；符號連結不算");
    assert!(with_mark.workspace_bytes >= 12_288 && with_mark.workspace_bytes < 200_000, "2 MiB 的符號連結目標沒算進來：{with_mark:?}");
    assert!(with_mark.outbox_bytes >= 4096 && with_mark.outbox_bytes < 200_000, "{with_mark:?}");
    assert!(!with_mark.truncated && !with_mark.full());
}

#[tokio::test]
async fn measure_of_a_missing_workspace_is_zero_but_a_symlinked_one_is_refused() {
    let scratch = test_dirs::scratch_dir("rs4-measure-missing");
    let site = site(&scratch, "rs4-measure-missing-host");
    fs::write(Path::new(&site.outbox).join("o.bin"), vec![0u8; 4096]).unwrap();
    fs::remove_dir_all(&site.workspace).unwrap();
    let m = site.measure().await.unwrap();
    assert_eq!(m.workspace_bytes, 0, "工作目錄不見了算 0（跟本機一樣）");
    assert_eq!(m.files, 1);

    let elsewhere = test_dirs::scratch_dir("rs4-elsewhere");
    unix_fs::symlink(&elsewhere, &site.workspace).unwrap();
    assert_eq!(site.measure().await.unwrap_err(), RfsError::Untrusted, "工作目錄是符號連結：不量、fail closed");
}

#[test]
fn the_measure_output_is_parsed_strictly() {
    let m = parse_measure(frame("MEASURE", "1048576 2048 300 0").as_bytes()).unwrap();
    assert_eq!((m.workspace_bytes, m.outbox_bytes, m.files, m.truncated), (1_048_576, 2048, 300, false));
    assert!(parse_measure(frame("MEASURE", "1 2 3 1").as_bytes()).unwrap().truncated, "超過 20 萬項＝truncated（下限值）");
    assert_eq!(parse_measure(frame("MEASURE", "1 2 three 0").as_bytes()).unwrap_err(), RfsError::Unavailable, "數字讀不出來不當 0");
    assert_eq!(parse_measure(frame("MEASURE", "1 2").as_bytes()).unwrap_err(), RfsError::Unavailable);
    assert_eq!(parse_measure(b"AM_RFS1\nMEASURE 7\n1 2 3 0\n").unwrap_err(), RfsError::Unavailable, "少 AM_RFS_DONE 作廢");
    assert_eq!(parse_measure(frame("ERR", "UNTRUSTED").as_bytes()).unwrap_err(), RfsError::Untrusted);
    assert_eq!(parse_measure(frame("EVIL", "1 2 3 0").as_bytes()).unwrap_err(), RfsError::Untrusted, "沒宣告的 TAG 作廢");
}

// ───────────────────────── prune_outbox ─────────────────────────

#[tokio::test]
async fn a_file_is_pruned_only_when_both_mtime_and_ctime_are_past_the_keep_days() {
    let scratch = test_dirs::scratch_dir("rs4-prune-age");
    let site = site(&scratch, "rs4-prune-age-host");
    let ob = Path::new(&site.outbox);
    let now = SystemTime::now();
    fs::write(ob.join("old-mtime.txt"), b"x").unwrap();
    set_mtime(&ob.join("old-mtime.txt"), now - 30 * DAY);
    fs::write(ob.join("fresh.txt"), b"x").unwrap();
    fs::write(ob.join("future.txt"), b"x").unwrap();
    set_mtime(&ob.join("future.txt"), now + 20 * DAY);
    fs::write(ob.join(".am-share-keep"), b"keep").unwrap();
    set_mtime(&ob.join(".am-share-keep"), now - 30 * DAY);

    // 現在：mtime 是舊的，但 ctime 是剛剛（改 mtime 本身就更新 ctime）→ 兩個都要過，所以什麼都不刪。
    assert_eq!(prune_at(&site, 14, u64::MAX, 100, None).await.unwrap(), 0);
    assert!(ob.join("old-mtime.txt").exists() && ob.join("fresh.txt").exists());

    // 15 天後：ctime 與 mtime 都超過 14 天的刪；mtime 在未來的不刪；標記檔不刪不算。
    let later = epoch(now) + 15 * 86_400;
    assert_eq!(prune_at(&site, 14, u64::MAX, 100, Some(later)).await.unwrap(), 2);
    assert!(!ob.join("old-mtime.txt").exists() && !ob.join("fresh.txt").exists());
    assert!(ob.join("future.txt").exists(), "mtime 還沒過期的留下");
    assert!(ob.join(".am-share-keep").exists(), "標記檔不刪");
}

#[tokio::test]
async fn an_overfull_outbox_is_trimmed_from_the_oldest_by_count_and_by_bytes() {
    let scratch = test_dirs::scratch_dir("rs4-prune-cap");
    let site = site(&scratch, "rs4-prune-cap-host");
    let ob = Path::new(&site.outbox);
    let now = SystemTime::now();
    fs::write(ob.join(".am-share-keep"), vec![0u8; 5000]).unwrap(); // 標記檔不算進上限
    for i in 1..=5u64 {
        let p = ob.join(format!("f{i}.bin"));
        fs::write(&p, vec![i as u8; 100]).unwrap();
        set_mtime(&p, now - Duration::from_secs((6 - i) * 3600)); // f5 最新
    }
    // 數量上限 2：留 f5、f4，刪 f1～f3。
    assert_eq!(prune_at(&site, 14, u64::MAX, 2, None).await.unwrap(), 3);
    let left = |dir: &Path| {
        let mut v: Vec<String> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    };
    assert_eq!(left(ob), [".am-share-keep", "f4.bin", "f5.bin"]);

    // 位元組上限 150：新的排前面累計，f5（100）進得去，f4 一加就 200 > 150，它和更舊的都刪。
    assert_eq!(prune_at(&site, 14, 150, 100, None).await.unwrap(), 1);
    assert_eq!(left(ob), [".am-share-keep", "f5.bin"]);
}

#[tokio::test]
async fn pruning_deletes_newline_names_recurses_but_never_removes_directories_or_follows_symlinks() {
    let scratch = test_dirs::scratch_dir("rs4-prune-misc");
    let site = site(&scratch, "rs4-prune-misc-host");
    let ob = Path::new(&site.outbox);
    let now = SystemTime::now();
    fs::write(ob.join("evil\nname.txt"), b"x").unwrap();
    fs::write(ob.join("keep.txt"), b"x").unwrap();
    fs::create_dir_all(ob.join("sub")).unwrap();
    fs::write(ob.join("sub/deep.txt"), b"x").unwrap();
    // 符號連結：不是一般檔，不刪、不算，連結指到的檔更不能動。
    let outside = test_dirs::scratch_dir("rs4-prune-outside");
    fs::write(outside.join("precious.txt"), b"x").unwrap();
    unix_fs::symlink(outside.join("precious.txt"), ob.join("link.txt")).unwrap();
    unix_fs::symlink(&outside, ob.join("link-dir")).unwrap();

    let later = epoch(now) + 15 * 86_400;
    assert_eq!(prune_at(&site, 14, u64::MAX, 100, Some(later)).await.unwrap(), 3, "換行名、keep.txt、sub/deep.txt（子目錄也處理）");
    assert!(outside.join("precious.txt").exists(), "符號連結指到的檔沒被動");
    assert!(ob.join("link.txt").symlink_metadata().is_ok() && ob.join("link-dir").symlink_metadata().is_ok(), "連結本身不刪");
    assert!(ob.join("sub").is_dir(), "遠端從不 rm -rf：空目錄留著");
}

#[tokio::test]
async fn pruning_a_missing_outbox_is_nothing_and_a_symlinked_one_is_refused() {
    let scratch = test_dirs::scratch_dir("rs4-prune-missing");
    let site = site(&scratch, "rs4-prune-missing-host");
    fs::remove_dir_all(&site.outbox).unwrap();
    assert_eq!(prune_at(&site, 14, 1, 1, None).await.unwrap(), 0, "outbox 還沒建立：沒事做");
    let elsewhere = test_dirs::scratch_dir("rs4-prune-elsewhere");
    fs::write(elsewhere.join("old.txt"), b"x").unwrap();
    unix_fs::symlink(&elsewhere, &site.outbox).unwrap();
    assert_eq!(prune_at(&site, 0, 0, 0, Some(epoch(SystemTime::now()) + 86_400)).await.unwrap_err(), RfsError::Untrusted);
    assert!(elsewhere.join("old.txt").exists(), "符號連結的目標不能被清");
}

#[test]
fn the_prune_script_never_uses_rm_dash_r_and_keeps_the_mark() {
    let s = prune_outbox_script("/home/u/.config/agents-manager/outbox/B", 14, 500 * 1024 * 1024, 1000);
    assert!(!s.contains("rm -rf") && !s.contains("rm -r "), "遠端從不 rm -rf：{s}");
    assert!(s.contains("! -name .am-share-keep") && s.contains("-newercm"), "{s}");
}

#[tokio::test]
async fn the_keep_mark_script_replaces_a_symlinked_mark_instead_of_writing_through_it() {
    let scratch = test_dirs::scratch_dir("rs4-keep-link");
    let site = site(&scratch, "rs4-keep-link-host");
    let victim = scratch.join("victim.txt");
    fs::write(&victim, b"important").unwrap();
    unix_fs::symlink(&victim, Path::new(&site.outbox).join(".am-share-keep")).unwrap();
    site.mark_share_keep(true).await.unwrap();
    assert_eq!(fs::read(&victim).unwrap(), b"important", "沒有寫穿符號連結");
    let mark = Path::new(&site.outbox).join(".am-share-keep");
    assert!(mark.symlink_metadata().unwrap().file_type().is_file(), "標記檔換成真的檔");
    site.mark_share_keep(false).await.unwrap();
    assert!(!mark.exists());
}

// ───────────────────────── 需要整個 App 的：is_full／sweep／keep 標記 ─────────────────────────

#[cfg(feature = "daemon-test-harness")]
mod with_app {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::share::budget::{self, Measured, Unavailable};
    use crate::testing as tt;

    /// 把測試 app 的專案搬到一台假遠端主機（`local_sh_fake`：本機 sh 扮演它，家目錄＝scratch）。
    async fn remote_host(e: &tt::Env, host: &str, scratch: &Path) -> Arc<HostConn> {
        let cfg = HostCfg {
            name: host.into(),
            ssh: "fake-ssh".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
            shared_session: false,
        };
        let conn = e.app.hosts.insert_remote_for_test(cfg).await;
        *conn.remote_home.lock().await = Some(scratch.to_string_lossy().into_owned());
        conn.connected.store(true, Ordering::SeqCst);
        test_support::local_sh_fake(host, scratch);
        sqlx::query("UPDATE projects SET host = ? WHERE id = ?").bind(host).bind(&e.project_id).execute(&e.app.db).await.unwrap();
        conn
    }

    /// 一顆遠端專案的分享 bot：`shared_bots` 一列、遠端工作目錄與 outbox 都建好。回 `(bot id, 工作目錄, outbox)`。
    async fn share_bot(e: &tt::Env, scratch: &Path, name: &str, profile: &str) -> (String, std::path::PathBuf, std::path::PathBuf) {
        let bot = tt::claude_bot(&e.app, &e.project_id, name).await;
        let ws = scratch.join("shared-bots").join(name);
        fs::create_dir_all(ws.join("inbox")).unwrap();
        crate::share::store::insert_share_bot(&e.app.db, &bot.id, profile, &ws.to_string_lossy()).await.unwrap();
        let outbox = scratch.join(".config/agents-manager/outbox").join(&bot.id);
        fs::create_dir_all(&outbox).unwrap();
        (bot.id, ws, outbox)
    }

    #[tokio::test]
    async fn a_remote_restricted_bot_is_measured_over_ssh_and_remembered() {
        let e = tt::env().await;
        let scratch = test_dirs::scratch_dir("rs4-app-measure");
        remote_host(&e, "rs4-measure-app", &scratch).await;
        let (id, ws, _) = share_bot(&e, &scratch, "far-restricted", "restricted").await;
        fs::write(ws.join("inbox/up.bin"), vec![0u8; 8192]).unwrap();
        budget::clear_cached_for_test(&id);
        let (m, before) = budget::refresh(&e.app, &id).await.unwrap().expect("遠端受限 bot 量得到");
        assert!(before.is_none() && m.files == 1 && m.workspace_bytes >= 8192, "{m:?}");
        assert_eq!(budget::cached(&id).map(|(c, _)| c), Some(m), "量測值有記下來");
        assert_eq!(budget::is_full(&e.app, &id).await, Ok(false));
    }

    #[tokio::test]
    async fn remote_is_full_fails_closed_when_there_is_no_fresh_value_and_no_way_to_measure() {
        let e = tt::env().await;
        let scratch = test_dirs::scratch_dir("rs4-app-closed");
        let conn = remote_host(&e, "rs4-closed-app", &scratch).await;
        let (id, _, _) = share_bot(&e, &scratch, "far-closed", "restricted").await;

        // 有新鮮的量測值：主機斷了也照用。
        budget::set_cached_for_test(&id, Measured::default());
        conn.connected.store(false, Ordering::SeqCst);
        assert_eq!(budget::is_full(&e.app, &id).await, Ok(false), "新鮮值照用");
        // 沒有值又量不到：不當作沒滿。
        budget::clear_cached_for_test(&id);
        assert_eq!(budget::is_full(&e.app, &id).await, Err(Unavailable), "主機斷線＋沒有量測值 → Unavailable（分享頁 503）");
        // 連上了、但腳本框不完整（被截斷）：同樣量不到。
        conn.connected.store(true, Ordering::SeqCst);
        am_base::hosts::set_ssh_fake("rs4-closed-app", |_| Ok("AM_RFS1\nMEASURE 7\n1 2 3 0\n".into()));
        assert_eq!(budget::is_full(&e.app, &id).await, Err(Unavailable), "框不完整 → Unavailable");
        // ssh 本身失敗。
        am_base::hosts::set_ssh_fake("rs4-closed-app", |_| Err(anyhow::anyhow!("ssh: connection reset")));
        assert_eq!(budget::is_full(&e.app, &id).await, Err(Unavailable));
        assert!(budget::cached(&id).is_none(), "量不到不記值");
    }

    #[tokio::test]
    async fn the_sweep_prunes_then_measures_remote_bots_skips_down_hosts_and_announces_a_full_one_once() {
        let e = tt::env().await;
        let scratch = test_dirs::scratch_dir("rs4-app-sweep");
        let conn = remote_host(&e, "rs4-sweep-app", &scratch).await;
        let (restricted, _, _) = share_bot(&e, &scratch, "far-sweep-r", "restricted").await;
        let (trusted, _, _) = share_bot(&e, &scratch, "far-sweep-t", "trusted").await;

        let calls: Arc<Mutex<Vec<&'static str>>> = Arc::default();
        let log = calls.clone();
        let full = format!("{} 0 1 0", 2u64 * 1024 * 1024 * 1024);
        am_base::hosts::set_ssh_fake("rs4-sweep-app", move |script| {
            if script.contains("MEASURE") {
                log.lock().unwrap().push("measure");
                Ok(frame("MEASURE", &full))
            } else if script.contains("PRUNED") {
                log.lock().unwrap().push("prune");
                Ok("AM_RFS1\nPRUNED 1\n0\nAM_RFS_DONE\n".into())
            } else {
                Err(anyhow::anyhow!("unexpected script"))
            }
        });
        budget::clear_cached_for_test(&restricted);

        let newly = budget::sweep(&e.app).await;
        assert_eq!(newly.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(), [restricted.as_str()], "只有受限的那顆被量、而且剛變滿");
        assert!(newly[0].1.full());
        // 受限：先 prune 再 measure；信任分享：只 prune（不算預算）。順序不保證在兩顆之間，但每顆各自 prune 在 measure 之前。
        let seen = calls.lock().unwrap().clone();
        assert_eq!(seen.iter().filter(|c| **c == "prune").count(), 2, "受限與信任分享各做一次保留政策：{seen:?}");
        assert_eq!(seen.iter().filter(|c| **c == "measure").count(), 1, "信任分享不量：{seen:?}");
        let last_measure = seen.iter().rposition(|c| *c == "measure").unwrap();
        assert!(seen[..last_measure].contains(&"prune"), "量之前先做過保留政策：{seen:?}");
        assert!(budget::cached(&trusted).is_none());

        // 第二輪：已經是滿的，不再宣告。
        assert!(budget::sweep(&e.app).await.is_empty(), "同一次變滿只通知一次");

        // 主機斷線：整台跳過（不打 ssh、不通知、不清快取）。
        conn.connected.store(false, Ordering::SeqCst);
        let before = calls.lock().unwrap().len();
        assert!(budget::sweep(&e.app).await.is_empty());
        assert_eq!(calls.lock().unwrap().len(), before, "斷線的主機一個 ssh 都不打");
        assert!(budget::cached(&restricted).is_some(), "快取沒被清");
    }

    #[tokio::test]
    async fn remote_keep_marks_follow_startup_and_revoke_and_a_down_host_only_warns() {
        let e = tt::env().await;
        let scratch = test_dirs::scratch_dir("rs4-app-keep");
        let conn = remote_host(&e, "rs4-keep-app", &scratch).await;
        let (id, _, outbox) = share_bot(&e, &scratch, "far-keep", "restricted").await;
        let mark = outbox.join(".am-share-keep");

        conn.connected.store(false, Ordering::SeqCst);
        crate::share::keep_share_outboxes(&e.app).await;
        assert!(!mark.exists(), "主機斷線：只記 warning，沒有標記也沒有錯");

        conn.connected.store(true, Ordering::SeqCst);
        crate::share::keep_share_outboxes(&e.app).await;
        assert!(mark.is_file(), "開機補標記：遠端 outbox 放 .am-share-keep");

        // bot 已標成刪除之後收尾，仍找得到它在哪台主機並拿掉標記。
        sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?").bind(crate::db::now()).bind(&id).execute(&e.app.db).await.unwrap();
        crate::share::revoke_bot_share(&e.app, &id).await.unwrap();
        assert!(!mark.exists(), "revoke 拿掉遠端標記");
    }
}
