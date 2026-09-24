//! 已經在跑的遠端 bot 目錄補收權限（issue #501）。
//!
//! #494 的 `chmod` 在 `install_remote_hook` 裡，而那支只在**啟動 bot** 時跑。daemon 換版當下還在跑的
//! 遠端 bot 因此要等它自己重啟才會被收緊：目錄仍是 0755、spool 仍是舊 hook.sh 用預設 umask 建的 0644，
//! 而 daemon 這段期間一直在對它 drain。遠端 bot 可以跑很久不重啟，所以連上時掃一次。
//!
//! 冪等、best-effort：失敗只記一行 debug（權限收不了不該擋任何事），每台每次連上跑一次就好——
//! 掃描本身不會產生新的鬆權限檔案，新開的 bot 由安裝那一趟負責。

use crate::state::App;
use std::sync::Arc;

/// 連上（含重連）之後在背景收一次。
pub fn spawn_tighten(app: Arc<App>, host: String) {
    tokio::spawn(async move {
        match tighten(&app, &host).await {
            Ok(n) if n > 0 => tracing::info!(host, bots = n, "tightened remote bot directories"),
            Ok(_) => {}
            Err(e) => tracing::debug!(host, error = ?e, "could not tighten remote bot directories"),
        }
    });
}

/// 回傳掃到的 bot 目錄數。
pub(crate) async fn tighten(app: &Arc<App>, host: &str) -> anyhow::Result<usize> {
    let Some(conn) = app.hosts.get(host).await else { return Ok(0) };
    if conn.is_local() || !conn.is_connected() {
        return Ok(0);
    }
    let root = crate::startup::remote_root_for(app.instance().as_deref());
    let out = conn.ssh_exec(&tighten_script(&root)).await?;
    Ok(out.lines().filter(|l| l.trim() == "AM_TIGHTENED").count())
}

/// `bots` 本身也收：目錄擋得住 traversal，裡面的檔案就算還是 0644 也開不到。
/// 一個 bot 目錄失敗不影響其他的（`|| true`），最後一行的狀態永遠是 0。
pub(crate) fn tighten_script(root: &str) -> String {
    format!(
        "B=\"$HOME/{root}/bots\"\n\
         [ -d \"$B\" ] || exit 0\n\
         chmod 700 \"$B\" 2>/dev/null || true\n\
         for d in \"$B\"/*/; do\n\
         [ -d \"$d\" ] || continue\n\
         chmod 700 \"$d\" 2>/dev/null || true\n\
         chmod go-rwx \"$d\"/* 2>/dev/null || true\n\
         printf 'AM_TIGHTENED\\n'\n\
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
        assert_eq!(String::from_utf8_lossy(&out.stdout).lines().count(), 1, "一個 bot 目錄報一行");
        assert_eq!(mode_of(&bots), 0o700, "bots 本身也收：擋住 traversal");
        assert_eq!(mode_of(&old), 0o700);
        assert_eq!(mode_of(&spool), 0o600);
        assert_eq!(std::fs::read_to_string(&spool).unwrap(), "{\"payload\":\"secret\"}\n", "只動權限");
        let _ = std::fs::remove_dir_all(&home);
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
