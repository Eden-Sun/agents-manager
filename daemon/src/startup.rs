//! 啟動前的隔離（SPEC §3.1、§6.1）：資料目錄跟著 `--config` 走，同一個資料目錄只准一顆 daemon。
//!
//! 2026-09-14 的事故：有人用 `--config /tmp/am-iso/config.toml` 把第二顆 daemon 起在 7799 想隔離測試，
//! 但資料目錄仍是 `~/.config/agents-manager` → 它開的是**正式 DB**，再用那份幾乎空的 config 做投影，
//! 8 秒內把 15 顆 bot、6 個專案標成 `deleted_at`。所以：非預設的 `--config` 一律把資料目錄搬到設定檔旁邊
//! （或設定檔裡明寫 `[server] data_dir`），而且開 DB 之前先對資料目錄拿檔案鎖。

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// 沒有 `--config` 也沒有 `AM_DATA_DIR` 時的資料目錄。
pub fn default_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".config/agents-manager")
}

/// `AM_DATA_DIR`（空字串當沒設）。`hook_cmd.rs` 的 spool 也讀同一個變數。
pub fn env_dir() -> Option<PathBuf> {
    std::env::var_os("AM_DATA_DIR")
        .map(PathBuf::from)
        .filter(|d| !d.as_os_str().is_empty())
        .map(|d| absolute(&d))
}

/// 設定檔位置：`--config` 給了就用它，否則是資料目錄底下那一份。
pub fn config_path(config_arg: Option<PathBuf>, env_dir: Option<PathBuf>) -> PathBuf {
    match config_arg {
        Some(p) => absolute(&p),
        None => env_dir.unwrap_or_else(default_dir).join("config.toml"),
    }
}

/// 資料目錄：`[server] data_dir` > `--config` 所在目錄 > `AM_DATA_DIR` > 預設。
///
/// `AM_DATA_DIR` 與算出來的不一致就拒絕啟動——那正是「以為在隔離、其實開到正式 DB」的形狀，
/// 與其挑一邊猜，不如讓人把話講清楚。
pub fn data_dir(
    cfg_path: &Path,
    config_arg_given: bool,
    cfg_data_dir: Option<&str>,
    env_dir: Option<PathBuf>,
) -> Result<PathBuf> {
    let beside_config = cfg_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = match cfg_data_dir.map(str::trim).filter(|s| !s.is_empty()) {
        Some(d) => {
            let d = expand_tilde(d);
            if d.is_absolute() {
                d
            } else {
                beside_config.join(d)
            }
        }
        // 非預設的設定檔＝另一套環境：資料跟著設定檔，不准沿用 ~/.config/agents-manager。
        None if config_arg_given => beside_config,
        None => env_dir.clone().unwrap_or_else(default_dir),
    };
    let dir = absolute(&dir);
    if let Some(env) = env_dir {
        if !same_dir(&env, &dir) {
            bail!(
                "AM_DATA_DIR={} 與設定檔 {} 算出來的資料目錄 {} 不一致：\
                 這正是「以為在隔離、其實開到正式 DB」的形狀，拒絕啟動。\
                 要隔離測試就讓兩者一致（或在設定檔寫 [server] data_dir），不要只換 --config 與 port。",
                env.display(),
                cfg_path.display(),
                dir.display()
            );
        }
    }
    Ok(dir)
}

/// 資料目錄的獨佔鎖，活到行程結束為止（`flock` 綁在 fd 上，關檔／行程死掉就自動放開）。
#[derive(Debug)]
pub struct DirLock {
    file: std::fs::File,
    path: PathBuf,
}

impl DirLock {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// 重啟時前一顆可能還在收攤（ops 的重啟是 `kill` + `sleep 2`），等這麼久再判定失敗。
pub const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// 對 `<dir>/daemon.lock` 拿獨佔鎖。拿不到就回錯，呼叫端在**碰 DB 之前**就停住。
pub fn lock_dir(dir: &Path, wait: std::time::Duration) -> Result<DirLock> {
    use std::io::{Seek, Write};
    use std::os::unix::io::AsRawFd;

    let path = dir.join("daemon.lock");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    // flock 綁的是「開啟的檔案描述」，同一個行程再開一次也會擋——所以測試不必真的起第二顆 daemon。
    let deadline = std::time::Instant::now() + wait;
    let mut taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    while !taken && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(200));
        taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    }
    if !taken {
        let err = std::io::Error::last_os_error();
        let who = std::fs::read_to_string(&path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "pid 不明".to_string());
        bail!(
            "資料目錄 {} 已經有一顆 daemon 佔著（{}；{}）。\
             同一個資料目錄只能有一顆 daemon：要隔離測試請把 --config 指到另一個目錄，\
             資料目錄會跟著設定檔走。",
            dir.display(),
            who,
            err
        );
    }
    // 拿到鎖才寫 pid：下一個人被擋下來時看得到是誰佔著。
    file.set_len(0)?;
    file.rewind()?;
    writeln!(file, "pid {}", std::process::id())?;
    file.flush()?;
    Ok(DirLock { file, path })
}

impl Drop for DirLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(rest);
    }
    PathBuf::from(s)
}

/// 目錄還不存在時 `canonicalize` 會失敗，所以只做「補上工作目錄」這一步。
fn absolute(p: &Path) -> PathBuf {
    if p.is_absolute() {
        return p.to_path_buf();
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(p)
}

/// `/tmp` 在 macOS 是 `/private/tmp` 的 symlink，字串比會誤判成不一致。
fn same_dir(a: &Path, b: &Path) -> bool {
    let ca = std::fs::canonicalize(a);
    let cb = std::fs::canonicalize(b);
    match (ca, cb) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("am-startup-{name}-{}", crate::db::ulid()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn config_in_another_dir_takes_its_db_with_it() {
        let dir = tmp("iso");
        let cfg = config_path(Some(dir.join("config.toml")), None);
        let got = data_dir(&cfg, true, None, None).unwrap();
        assert!(same_dir(&got, &dir), "{} != {}", got.display(), dir.display());
        assert!(!same_dir(&got, &default_dir()), "不得沿用 ~/.config/agents-manager");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn no_config_flag_keeps_the_default_dir() {
        let cfg = config_path(None, None);
        assert_eq!(cfg, default_dir().join("config.toml"));
        assert_eq!(data_dir(&cfg, false, None, None).unwrap(), default_dir());
    }

    #[test]
    fn explicit_data_dir_in_the_config_wins() {
        let dir = tmp("explicit");
        let data = dir.join("state");
        let cfg = dir.join("config.toml");
        let got = data_dir(&cfg, true, Some(data.to_str().unwrap()), None).unwrap();
        assert_eq!(got, data);
        // 相對路徑以設定檔所在目錄為準。
        assert_eq!(data_dir(&cfg, true, Some("state"), None).unwrap(), data);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn env_data_dir_that_disagrees_with_config_refuses() {
        let iso = tmp("env-iso");
        let cfg = iso.join("config.toml");
        let err = data_dir(&cfg, true, None, Some(default_dir())).unwrap_err().to_string();
        assert!(err.contains("AM_DATA_DIR"), "{err}");
        assert!(err.contains("拒絕啟動"), "{err}");
        // 一致時放行（/tmp 的 symlink 不算不一致）。
        data_dir(&cfg, true, None, Some(iso.clone())).unwrap();
        std::fs::remove_dir_all(&iso).ok();
    }

    #[test]
    fn a_second_daemon_on_the_same_dir_is_refused() {
        let dir = tmp("lock");
        let first = lock_dir(&dir, std::time::Duration::ZERO).unwrap();
        let err = lock_dir(&dir, std::time::Duration::ZERO).unwrap_err().to_string();
        assert!(err.contains("已經有一顆 daemon"), "{err}");
        assert!(err.contains(&format!("pid {}", std::process::id())), "{err}");
        let lock_file = first.path().to_path_buf();
        // 前一顆放掉鎖之後才輪得到下一顆（daemon 收攤 / 被 kill 就是這個形狀）。
        drop(first);
        lock_dir(&dir, std::time::Duration::ZERO).unwrap();
        assert!(lock_file.exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
