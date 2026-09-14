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
        .map(|d| normalize(&d))
}

/// 設定檔位置：`--config` 給了就用它，否則是資料目錄底下那一份。
pub fn config_path(config_arg: Option<PathBuf>, env_dir: Option<PathBuf>) -> PathBuf {
    match config_arg {
        Some(p) => normalize(&p),
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
    let dir = normalize(&dir);
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

/// 只讀出 `[server] data_dir`：**不** mkdir、**不**寫預設檔。設定檔的建立要等拿到鎖之後
/// （`ConfigStore::load` 會 mkdir 兼寫一份預設 config，第二顆程序不該先動到共用設定）。
/// 解析不出來就回 `None`：真正的解析錯誤留給拿鎖之後的 `ConfigStore::load` 報。
pub fn peek_data_dir(cfg_path: &Path) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Peek {
        server: Option<PeekServer>,
    }
    #[derive(serde::Deserialize)]
    struct PeekServer {
        data_dir: Option<String>,
    }
    let text = std::fs::read_to_string(cfg_path).ok()?;
    let peek: Peek = toml::from_str(&text).ok()?;
    peek.server?.data_dir
}

/// 啟動前的準備：唯讀算出資料目錄 → 建立**那個目錄**（鎖檔要有地方放）→ 拿鎖。
/// 回來之後才輪到會寫檔的事（`ConfigStore::load`、`ui-token`、DB）。
#[derive(Debug)]
pub struct Prepared {
    pub cfg_path: PathBuf,
    pub dir: PathBuf,
    /// 拿在手上，`serve` 期間不能放掉。
    pub lock: DirLock,
}

pub fn prepare(config_arg: Option<PathBuf>, wait: std::time::Duration) -> Result<Prepared> {
    let env_dir = env_dir();
    let config_given = config_arg.is_some();
    let cfg_path = config_path(config_arg, env_dir.clone());
    let dir = data_dir(&cfg_path, config_given, peek_data_dir(&cfg_path).as_deref(), env_dir)?;
    std::fs::create_dir_all(&dir).with_context(|| format!("create data dir {}", dir.display()))?;
    let lock = lock_dir(&dir, wait)?;
    Ok(Prepared { cfg_path, dir, lock })
}

/// `serve` 的前半段，原封不動：prepare（唯讀解析 → 建資料目錄 → 拿鎖）→ 載入設定 → 確認 →
/// 開 DB。測試走同一條，才驗得到「拿不到鎖時一個檔都沒生出來」這種順序性質。
pub struct Instance {
    pub dir: PathBuf,
    pub cfg_path: PathBuf,
    pub store: crate::config::ConfigStore,
    pub pool: sqlx::SqlitePool,
    pub lock: DirLock,
    /// `None` = 正式實例（預設資料目錄）。
    pub slug: Option<String>,
}

pub async fn open_instance(config_arg: Option<PathBuf>, wait: std::time::Duration) -> Result<Instance> {
    let config_given = config_arg.is_some();
    let prepared = prepare(config_arg, wait)?;
    let store = crate::config::ConfigStore::load(prepared.cfg_path.clone()).await?;
    let data_dir_in_cfg = store.get().await.server.data_dir;
    confirm_data_dir(&prepared, data_dir_in_cfg.as_deref(), config_given)?;
    // 只算不設：行程層級的 `set_instance` 由 `serve` 自己叫（測試不該汙染整個行程）。
    let slug = instance_slug(&prepared.dir);
    let pool = crate::db::open(&prepared.dir.join("agents-manager.sqlite3")).await?;
    let Prepared { cfg_path, dir, lock } = prepared;
    Ok(Instance { dir, cfg_path, store, pool, lock, slug })
}

/// 拿鎖之後載入的設定，`data_dir` 必須跟當初唯讀看到的同一個——不同就是有人在這中間改了檔案，
/// 這時繼續跑等於拿著 A 的鎖寫 B 的 DB。
pub fn confirm_data_dir(prepared: &Prepared, loaded: Option<&str>, config_given: bool) -> Result<()> {
    let again = data_dir(&prepared.cfg_path, config_given, loaded, env_dir())?;
    if !same_dir(&again, &prepared.dir) {
        bail!(
            "設定檔 {} 在拿鎖之後把資料目錄從 {} 改成 {}：拒絕啟動，請重跑。",
            prepared.cfg_path.display(),
            prepared.dir.display(),
            again.display()
        );
    }
    Ok(())
}

/// 遠端主機上的預設根（正式實例）。
pub const REMOTE_ROOT: &str = ".config/agents-manager";

/// 這顆 daemon 的「實例名」：預設資料目錄＝`None`（正式），其他＝資料目錄的短雜湊。
/// 遠端主機上只看得到 `$HOME`，兩顆 daemon 管同一台遠端、bot id 又相同時會共用 spool 檔，
/// 所以遠端的 bot 目錄要照這個名字分開（SPEC §11.4）。
pub fn instance_slug(data_dir: &Path) -> Option<String> {
    if same_dir(data_dir, &default_dir()) {
        return None;
    }
    let bytes = normalize(data_dir).to_string_lossy().into_owned();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    Some(format!("{hash:016x}"))
}

/// 遠端主機上這顆實例的根目錄（`$HOME` 之下的相對路徑）。
pub fn remote_root_for(slug: Option<&str>) -> String {
    match slug {
        Some(s) => format!("{REMOTE_ROOT}/instances/{s}"),
        None => REMOTE_ROOT.to_string(),
    }
}

static INSTANCE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// `serve` 開完 instance 後叫一次，之後 hook 安裝與遠端 drain 都讀同一個值
/// （同一個行程只有一顆 daemon；測試不叫它，所以一律當正式實例）。
pub fn set_instance(slug: Option<String>) {
    let _ = INSTANCE.set(slug);
}

pub fn remote_root() -> String {
    remote_root_for(INSTANCE.get().and_then(|s| s.as_deref()))
}

/// 這顆行程是不是隔離實例。`serve` 之外（測試、hook 子命令）沒設過，一律當正式實例。
pub fn is_isolated() -> bool {
    INSTANCE.get().map(|s| s.is_some()).unwrap_or(false)
}

/// 資料目錄的獨佔鎖，活到行程結束為止（`flock` 綁在 fd 上，關檔／行程死掉就自動放開）。
#[derive(Debug)]
pub struct DirLock {
    file: std::fs::File,
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
    Ok(DirLock { file })
}

impl Drop for DirLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn expand_tilde(s: &str) -> PathBuf {
    let home = || dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    if s == "~" {
        return home();
    }
    match s.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(s),
    }
}

/// 逐段解析：每走一段就 canonicalize（把途中的 symlink 展開），遇到 `..` 才在**已展開**的路徑上回退。
///
/// 不能先在字面上消掉 `..`：`/a/link/../state`（link → `/b/c`）真正指的是 `/b/state`，字面摺疊會算成
/// `/a/state`，兩顆 daemon 就可能一個鎖 A、一個開 B。尾段還不存在時 canonicalize 會失敗，那段原樣接著。
fn normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(p)
    };
    let mut out = PathBuf::from("/");
    for comp in abs.components() {
        match comp {
            Component::Prefix(prefix) => out = PathBuf::from(prefix.as_os_str()),
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => {
                out.push(name);
                if let Ok(real) = std::fs::canonicalize(&out) {
                    out = real;
                }
            }
        }
    }
    out
}

/// symlink 別名（`/tmp` vs `/private/tmp`）、`..`、還不存在的目錄都要算同一個。
fn same_dir(a: &Path, b: &Path) -> bool {
    normalize(a) == normalize(b)
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

    /// 目錄還不存在、`/tmp` 是 symlink、路徑裡有 `..`、寫成裸 `~`——都不能被當成「不一致」而拒絕啟動。
    #[test]
    fn aliases_and_missing_dirs_still_count_as_the_same_dir() {
        let dir = tmp("alias");
        let cfg = dir.join("config.toml");
        let missing = dir.join("state");
        assert!(same_dir(&missing, &dir.join("sub/../state")), "`..` 要先摺疊");
        data_dir(&cfg, true, Some("state"), Some(dir.join("state"))).unwrap();
        data_dir(&cfg, true, Some("state"), Some(dir.join("sub/../state"))).unwrap();
        // /tmp 與 /private/tmp 是同一個目錄（macOS）。
        let slash_tmp = PathBuf::from("/tmp").join(dir.file_name().unwrap());
        if slash_tmp.exists() {
            assert!(same_dir(&slash_tmp, &dir));
        }
        assert_eq!(expand_tilde("~"), dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")));
        assert_eq!(data_dir(&cfg, true, Some("~"), None).unwrap(), normalize(&expand_tilde("~")));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// serve 的前半段：拿不到鎖時**一個檔都不能建**——`ConfigStore::load` 會寫一份預設 config，
    /// 那是共用設定，第二顆程序不該碰得到。
    #[test]
    fn prepare_touches_nothing_until_it_holds_the_lock() {
        let dir = tmp("prepare");
        let cfg = dir.join("config.toml");
        let held = lock_dir(&dir, std::time::Duration::ZERO).unwrap();

        let err = prepare(Some(cfg.clone()), std::time::Duration::ZERO).unwrap_err().to_string();
        assert!(err.contains("已經有一顆 daemon"), "{err}");
        assert!(!cfg.exists(), "拒絕啟動時不該寫出預設 config");
        let left: Vec<String> =
            std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into()).collect();
        assert_eq!(left, vec!["daemon.lock".to_string()], "除了鎖檔什麼都不該產生");

        drop(held);
        let ready = prepare(Some(cfg.clone()), std::time::Duration::ZERO).unwrap();
        assert!(same_dir(&ready.dir, &dir), "{} != {}", ready.dir.display(), dir.display());
        assert!(!cfg.exists(), "prepare 自己也不寫 config，那是拿鎖之後的事");
        // 設定檔在拿鎖之後把 data_dir 改掉 → 拒絕（拿著 A 的鎖寫 B 的 DB）。
        let err = confirm_data_dir(&ready, Some("elsewhere"), true).unwrap_err().to_string();
        assert!(err.contains("拿鎖之後"), "{err}");
        confirm_data_dir(&ready, None, true).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 先在字面上消 `..` 會錯解 symlink：`/a/link/../state`（link → `/b/c`）真正是 `/b/state`。
    #[test]
    fn parent_dir_after_a_symlink_resolves_to_the_link_target() {
        let root = tmp("symlink");
        let a = root.join("a");
        let bc = root.join("b/c");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&bc).unwrap();
        std::os::unix::fs::symlink(&bc, a.join("link")).unwrap();

        let got = normalize(&a.join("link/../state"));
        assert!(same_dir(&got, &root.join("b/state")), "{} 應該落在 b/ 底下", got.display());
        assert!(!same_dir(&got, &a.join("state")), "字面摺疊會算成 a/state，那會鎖錯 DB");
        // 拒絕啟動的判斷也跟著對：AM_DATA_DIR 寫成別名不算不一致。
        data_dir(&a.join("link/config.toml"), true, None, Some(bc.clone())).unwrap();
        std::fs::remove_dir_all(&root).ok();
    }

    /// 真的走一次 serve 的前半段（prepare → 載入設定 → 確認 → 開 DB），順序性質才驗得到。
    #[tokio::test]
    async fn open_instance_writes_nothing_until_it_owns_the_dir() {
        let dir = tmp("open");
        let cfg = dir.join("config.toml");
        let held = lock_dir(&dir, std::time::Duration::ZERO).unwrap();

        let err = match open_instance(Some(cfg.clone()), std::time::Duration::ZERO).await {
            Err(e) => e.to_string(),
            Ok(_) => panic!("鎖被別人拿著時不該開得起來"),
        };
        assert!(err.contains("已經有一顆 daemon"), "{err}");
        let left: Vec<String> =
            std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into()).collect();
        assert_eq!(left, vec!["daemon.lock".to_string()], "config、sqlite、ui-token 都不該生出來");

        drop(held);
        let inst = open_instance(Some(cfg.clone()), std::time::Duration::ZERO).await.unwrap();
        assert!(same_dir(&inst.dir, &dir));
        assert!(dir.join("agents-manager.sqlite3").exists(), "DB 開在設定檔旁邊");
        assert!(!default_dir().join("agents-manager.sqlite3").starts_with(&dir));
        assert!(cfg.exists(), "拿到鎖之後才寫出預設 config");
        // 隔離實例有自己的遠端 namespace，正式實例沒有。
        let slug = inst.slug.clone().expect("非預設資料目錄＝隔離實例");
        assert_eq!(remote_root_for(Some(&slug)), format!("{REMOTE_ROOT}/instances/{slug}"));
        assert_eq!(instance_slug(&default_dir()), None);
        assert_eq!(instance_slug(&dir), Some(slug), "同一個目錄每次都算出同一個名字");

        inst.pool.close().await;
        drop(inst.lock);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn peek_reads_data_dir_without_creating_anything() {
        let dir = tmp("peek");
        let cfg = dir.join("config.toml");
        assert_eq!(peek_data_dir(&cfg), None, "檔案不存在就是 None，不是建一份");
        assert!(!cfg.exists());
        std::fs::write(&cfg, "[server]\nlisten = '127.0.0.1:7799'\ndata_dir = '/tmp/am-iso'\n").unwrap();
        assert_eq!(peek_data_dir(&cfg).as_deref(), Some("/tmp/am-iso"));
        // 壞檔留給拿鎖之後的 ConfigStore::load 報，不在這裡炸。
        std::fs::write(&cfg, "這不是 toml =\n").unwrap();
        assert_eq!(peek_data_dir(&cfg), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn no_config_flag_keeps_the_default_dir() {
        let cfg = config_path(None, None);
        assert_eq!(cfg, default_dir().join("config.toml"));
        assert!(same_dir(&data_dir(&cfg, false, None, None).unwrap(), &default_dir()));
    }

    #[test]
    fn explicit_data_dir_in_the_config_wins() {
        let dir = tmp("explicit");
        let data = dir.join("state");
        let cfg = dir.join("config.toml");
        let got = data_dir(&cfg, true, Some(data.to_str().unwrap()), None).unwrap();
        assert!(same_dir(&got, &data), "{} != {}", got.display(), data.display());
        // 相對路徑以設定檔所在目錄為準。
        assert!(same_dir(&data_dir(&cfg, true, Some("state"), None).unwrap(), &data));
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
        let lock_file = dir.join("daemon.lock");
        // 前一顆放掉鎖之後才輪得到下一顆（daemon 收攤 / 被 kill 就是這個形狀）。
        drop(first);
        lock_dir(&dir, std::time::Duration::ZERO).unwrap();
        assert!(lock_file.exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
