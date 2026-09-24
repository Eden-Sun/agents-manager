//! 已經在跑的遠端 bot 目錄補收權限（issue #501）。
//!
//! #494 的 `chmod` 在 `install_remote_hook` 裡，而那支只在**啟動 bot** 時跑。daemon 換版當下還在跑的
//! 遠端 bot 因此要等它自己重啟才會被收緊：目錄仍是 0755、spool 仍是舊 hook.sh 用預設 umask 建的 0644，
//! 而 daemon 這段期間一直在對它 drain。遠端 bot 可以跑很久不重啟，所以連上時掃一次。
//!
//! 冪等、best-effort：失敗只記一行 debug（權限收不了不該擋任何事），每台每次連上跑一次就好——
//! 掃描本身不會產生新的鬆權限檔案，新開的 bot 由安裝那一趟負責。

use crate::state::App;
use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

/// 連上那一趟失敗的主機。觸發時機正是 ssh 最不穩的時候（剛連上、可能還在重試），
/// 而一台連著好幾天不重連的主機不會有第二次機會——所以失敗的記下來，讓週期補跑接手（#501 複看）。
fn owed() -> &'static Mutex<HashSet<String>> {
    static M: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

/// 補跑的間隔。跟 `remote_purge` 的巡一樣是 5 分鐘：這是「修不到就一直欠著」的保底，不是熱路徑。
const RETRY_EVERY: std::time::Duration = std::time::Duration::from_secs(300);

/// 連上（含重連）之後在背景收一次。
pub fn spawn_tighten(app: Arc<App>, host: String) {
    tokio::spawn(async move {
        run_once(&app, &host).await;
    });
}

/// 欠著的主機每 5 分鐘補跑一次，成功就不再欠。連上那次就成功的主機不會進這個迴圈。
pub fn spawn_poller(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(RETRY_EVERY).await;
            let hosts: Vec<String> = owed().lock().unwrap().iter().cloned().collect();
            for host in hosts {
                run_once(&app, &host).await;
            }
        }
    });
}

async fn run_once(app: &Arc<App>, host: &str) {
    match tighten(app, host).await {
        Ok(r) if r.failed > 0 => {
            // 有掃到、但有些 chmod 不成功：欠著下一輪再來。
            owed().lock().unwrap().insert(host.to_string());
            tracing::warn!(host, tightened = r.tightened, failed = r.failed, "some remote bot directories could not be tightened; will retry");
        }
        Ok(r) => {
            owed().lock().unwrap().remove(host);
            if r.tightened > 0 {
                tracing::info!(host, bots = r.tightened, "tightened remote bot directories");
            }
        }
        Err(e) => {
            owed().lock().unwrap().insert(host.to_string());
            // warn 而不是 debug：沒收成代表那台機器上的 prompt／回覆還是全機可讀（#501），不是無關緊要的雜訊。
            tracing::warn!(host, error = ?e, "could not tighten remote bot directories; will retry");
        }
    }
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Tightened {
    /// 真的收好了的 bot 目錄數。
    pub tightened: usize,
    /// 有 `chmod` 失敗的 bot 目錄數。
    pub failed: usize,
}

pub(crate) async fn tighten(app: &Arc<App>, host: &str) -> anyhow::Result<Tightened> {
    let Some(conn) = app.hosts.get(host).await else { return Ok(Tightened::default()) };
    if conn.is_local() || !conn.is_connected() {
        return Ok(Tightened::default());
    }
    let root = crate::startup::remote_root_for(app.instance().as_deref());
    let out = conn.ssh_exec(&tighten_script(&root)).await?;
    Ok(count(&out))
}

pub(crate) fn count(out: &str) -> Tightened {
    let mut t = Tightened::default();
    for l in out.lines().map(str::trim) {
        match l {
            "AM_TIGHTENED" => t.tightened += 1,
            "AM_TIGHTEN_FAILED" => t.failed += 1,
            _ => {}
        }
    }
    t
}

/// `bots` 本身也收：目錄擋得住 traversal，裡面的檔案就算還是 0644 也開不到。
/// 一個 bot 目錄失敗不影響其他的，最後一行的狀態永遠是 0。
///
/// 每個 bot 目錄印一行說**這一顆**收成了沒有（#501 複看）：以前不管 `chmod` 成不成功都印 `AM_TIGHTENED`，
/// 全部失敗時 daemon 還是記一行 info 說「tightened」。空目錄（glob 沒展開）不算失敗。
pub(crate) fn tighten_script(root: &str) -> String {
    format!(
        "B=\"$HOME/{root}/bots\"\n\
         [ -d \"$B\" ] || exit 0\n\
         chmod 700 \"$B\" 2>/dev/null || true\n\
         for d in \"$B\"/*/; do\n\
         [ -d \"$d\" ] || continue\n\
         ok=1\n\
         chmod 700 \"$d\" 2>/dev/null || ok=0\n\
         for f in \"$d\"/*; do\n\
         [ -e \"$f\" ] || continue\n\
         chmod go-rwx \"$f\" 2>/dev/null || ok=0\n\
         done\n\
         if [ \"$ok\" = 1 ]; then printf 'AM_TIGHTENED\\n'; else printf 'AM_TIGHTEN_FAILED\\n'; fi\n\
         done\n\
         exit 0\n"
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn mode_of(p: &std::path::Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
    }

    /// 換版前就在跑的那些：目錄 0755、spool 0644（舊 hook.sh 建的）。掃一次要全部收回來，內容不准動。
    #[test]
    fn a_sweep_tightens_directories_that_were_left_wide_open() {
        let home = std::env::temp_dir().join(format!("am-tighten-{}", crate::db::ulid()));
        let bots = home.join(crate::startup::REMOTE_ROOT).join("bots");
        let old = bots.join("b-old");
        std::fs::create_dir_all(&old).unwrap();
        let spool = old.join("hook-spool.jsonl");
        std::fs::write(&spool, "{\"payload\":\"secret\"}\n").unwrap();
        std::fs::set_permissions(&spool, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&bots, std::fs::Permissions::from_mode(0o755)).unwrap();

        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(tighten_script(crate::startup::REMOTE_ROOT))
            .env("HOME", &home)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(count(&String::from_utf8_lossy(&out.stdout)), Tightened { tightened: 1, failed: 0 });
        assert_eq!(mode_of(&bots), 0o700, "bots 本身也收：擋住 traversal");
        assert_eq!(mode_of(&old), 0o700);
        assert_eq!(mode_of(&spool), 0o600);
        assert_eq!(std::fs::read_to_string(&spool).unwrap(), "{\"payload\":\"secret\"}\n", "只動權限");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// #501 複看：`chmod` 失敗的那一顆要報 `AM_TIGHTEN_FAILED`，不能照樣說「收好了」——以前不管成不成功
    /// 都印 `AM_TIGHTENED`，全部失敗時 daemon 還是記一行 info 說「tightened」。
    ///
    /// 真實的失敗原因（唯讀檔案系統、檔案屬於別的使用者）在單元測試裡做不出來（測試不是 root），
    /// 所以用 PATH 上的 `chmod` 替身讓它一定失敗——測的是腳本怎麼處理失敗，不是 chmod 本身。
    #[test]
    fn a_directory_whose_chmod_fails_is_reported_as_failed() {
        let home = std::env::temp_dir().join(format!("am-tighten-{}", crate::db::ulid()));
        let bad = home.join(crate::startup::REMOTE_ROOT).join("bots").join("b-bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("hook-spool.jsonl"), "x\n").unwrap();
        let bin = home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("chmod"), "#!/bin/sh\necho 'chmod: Read-only file system' >&2\nexit 1\n").unwrap();
        std::fs::set_permissions(&bin.join("chmod"), std::fs::Permissions::from_mode(0o755)).unwrap();

        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(tighten_script(crate::startup::REMOTE_ROOT))
            .env("HOME", &home)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .output()
            .unwrap();
        assert!(out.status.success(), "一顆失敗不能讓整趟變成錯");
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert_eq!(count(&stdout), Tightened { tightened: 0, failed: 1 }, "{stdout}");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// #501 複看：連上那趟失敗時要記著欠這一台——觸發時機正是 ssh 最不穩的時候，而一台連著好幾天
    /// 不重連的主機不會有第二次機會。成功之後就不再欠（不然週期會一直空跑）。
    #[tokio::test]
    async fn a_host_whose_sweep_failed_stays_owed_until_it_succeeds() {
        let env = crate::testing::env().await;
        let host = format!("owed-{}", crate::db::ulid());
        let conn = env
            .app
            .hosts
            .insert_remote_for_test(crate::config::HostCfg {
                name: host.clone(),
                ssh: "unused".into(),
                ssh_port: 22,
                ssh_opts: vec![],
                herdr_session: "am-test".into(),
                remote_path: String::new(),
            })
            .await;
        conn.connected.store(true, std::sync::atomic::Ordering::SeqCst);

        crate::hosts::set_ssh_fake(&host, |_| anyhow::bail!("ssh: connection reset"));
        run_once(&env.app, &host).await;
        assert!(owed().lock().unwrap().contains(&host), "失敗要欠著，等週期補跑");

        crate::hosts::set_ssh_fake(&host, |_| Ok("AM_TIGHTENED\n".into()));
        run_once(&env.app, &host).await;
        assert!(!owed().lock().unwrap().contains(&host), "收好了就不再欠");

        // 有掃到但有 chmod 失敗：一樣欠著，下一輪再試。
        crate::hosts::set_ssh_fake(&host, |_| Ok("AM_TIGHTENED\nAM_TIGHTEN_FAILED\n".into()));
        run_once(&env.app, &host).await;
        assert!(owed().lock().unwrap().contains(&host), "有一顆沒收成就還沒完");
        owed().lock().unwrap().remove(&host);
    }

    /// 沒有 bots 目錄（這台還沒跑過遠端 bot）不是錯。
    #[test]
    fn a_host_with_no_bot_directory_is_not_an_error() {
        let home = std::env::temp_dir().join(format!("am-tighten-{}", crate::db::ulid()));
        std::fs::create_dir_all(&home).unwrap();
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(tighten_script(crate::startup::REMOTE_ROOT))
            .env("HOME", &home)
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "");
        let _ = std::fs::remove_dir_all(&home);
    }
}
