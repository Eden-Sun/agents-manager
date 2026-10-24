//! Developer-only whole-job Cargo offload (issue #104).
//!
//! V1 deliberately offloads verification commands (check/test/clippy) to one external SSH host.
//! It does not try to make a Linux/x86_64 binary runnable on macOS/aarch64, and therefore keeps
//! build/run/release work local.

use crate::config::{BuildRemoteCfg, ConfigFile};
use crate::lifecycle::LcError;
use crate::state::App;
use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "remote_cargo_hosts.rs"]
mod hosts;
use hosts::{acquire, local_shell, only_host, usable_targets, Busy};
pub use hosts::list_hosts_cli;

const PASSWORD_FILE: &str = "remote-cargo-password";

/// 整體上限最多設到一天（0＝不設上限）。
const MAX_TIMEOUT_SECS: u64 = 24 * 3600;

/// `[build.remote] max_concurrent` 最多設到這麼多。
const MAX_CONCURRENT: usize = 64;

/// 超過整體上限時 helper 的結束碼（跟 GNU `timeout` 一樣是 124）。shim 只把 125 當成「退回本機」，所以不會在本機重跑一次。
pub const EXIT_TIMEOUT: i32 = 124;

/// 排遠端名額排超過 [`QUEUE_WAIT_SECS`] 時守門的交握結束碼。
/// 這時遠端什麼都還沒跑；helper 轉成 125 讓 shim 排本機名額（#1853）。
pub const EXIT_QUEUE_FULL: i32 = 75;

/// 「這次不轉外部編譯，改在本機跑」的結束碼：shim 只認這一個，看到就往下去拿本機名額（`cargo_shim.rs`）。
pub const EXIT_RUN_LOCALLY: i32 = 125;

/// ssh 自己沒進去時的結束碼（連不上、認證被拒、host key 不合）。遠端的指令真的跑起來之後，回的是那個指令的結束碼，
/// 所以在交握階段看到 255 就是「我們根本沒進去」（issue #428）。
const SSH_FAILED: i32 = 255;

// 測試用：把 `ssh`／`rsync` 換成假腳本（本機當遠端）。只有測試 build 有這個入口，而且是**執行緒本地**的——並行的其他測試不受影響。
#[cfg(test)]
thread_local! {
    static TEST_PROGRAMS: std::cell::RefCell<Option<(String, String)>> = const { std::cell::RefCell::new(None) };
    // 測試用：覆寫正式路徑（[`Admission::from_cfg`]）的排隊等待秒數；一樣是執行緒本地，要在呼叫 `run_offload` 的那條執行緒上設。
    static TEST_QUEUE_WAIT_SECS: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
    // 測試用：覆寫正式路徑（[`Admission::from_cfg`]）的 shared 等待秒數（[`shared_wait_secs`]）；同上，要在呼叫 `run_offload` 的那條執行緒上設。
    static TEST_SHARED_WAIT_SECS: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
    // 測試用：守門腳本讀的負載檔（[`loadavg_path`]）；沒設就是 `/proc/loadavg`。
    static TEST_LOADAVG: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
    // 測試用：本機執行器（`host = "local"`）的 `$HOME`——假 cargo 放在它的 `.cargo/bin`。
    static TEST_LOCAL_HOME: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

fn ssh_program() -> String {
    #[cfg(test)]
    {
        if let Some((ssh, _)) = TEST_PROGRAMS.with(|p| p.borrow().clone()) {
            return ssh;
        }
    }
    "ssh".to_string()
}

fn rsync_program() -> String {
    #[cfg(test)]
    {
        if let Some((_, rsync)) = TEST_PROGRAMS.with(|p| p.borrow().clone()) {
            return rsync;
        }
    }
    "rsync".to_string()
}

#[derive(Debug, Deserialize)]
pub struct RemoteBuildInput {
    pub enabled: bool,
    pub host: String,
    pub user: String,
    #[serde(default = "default_port")]
    pub ssh_port: u16,
    #[serde(default)]
    pub remote_root: String,
    #[serde(default)]
    pub cargo_jobs: usize,
    /// 遠端測試執行緒上限（issue #202）。None＝維持現在的值；0＝不設。
    #[serde(default)]
    pub test_threads: Option<usize>,
    /// 一次遠端編譯的整體時間上限（秒，issue #194）。None＝維持現在的值；0＝不設上限。
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// 遠端 `shared/` 閒置幾小時回收（issue #196）。None＝維持現在的值。
    #[serde(default)]
    pub shared_idle_hours: Option<u64>,
    /// 遠端 `shared/` 最多留幾份（issue #196）。None＝維持現在的值；0＝不限。
    #[serde(default)]
    pub max_shared_dirs: Option<usize>,
    /// 遠端同時最多幾個編譯（issue #104）。None＝維持現在的值；0＝依遠端核數與 RAM 自動算。
    #[serde(default)]
    pub max_concurrent: Option<usize>,
    /// None = preserve the currently stored password; Some("") = delete it (key/agent auth).
    #[serde(default)]
    pub password: Option<String>,
}

fn default_port() -> u16 {
    22
}

/// 這份設定的密碼檔在哪；`None`＝沒有密碼（key/agent）。
///
/// 設定沒有 `password_id`＝舊版，讀固定檔名 `remote-cargo-password`；有的話讀 `remote-cargo-password.<id>`（issue #104）。
/// id 只收 16 位小寫 hex（本機產生），手改壞的設定不會變成路徑穿越。
fn password_path(data_dir: &Path, remote: &BuildRemoteCfg) -> anyhow::Result<Option<PathBuf>> {
    match remote.password_id.as_deref() {
        None => Ok(Some(data_dir.join(PASSWORD_FILE))),
        Some("") => Ok(None),
        Some(id) if id.len() == 16 && id.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')) => {
            Ok(Some(data_dir.join(format!("{PASSWORD_FILE}.{id}"))))
        }
        Some(id) => anyhow::bail!("[build.remote] password_id 格式不對（{id:?}）；到設定頁重新輸入密碼"),
    }
}

fn password_is_set(data_dir: &Path, remote: &BuildRemoteCfg) -> bool {
    matches!(password_path(data_dir, remote), Ok(Some(p)) if std::fs::metadata(&p).map(|m| m.is_file() && m.len() > 0).unwrap_or(false))
}

/// 測試用的故障注入（issue #104）：`Fail`＝那一步回錯誤、照正常路徑收拾；`Crash`＝行程死在那一步之後，什麼都不收拾。
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq)]
enum Fault {
    Fail,
    Crash,
}

#[cfg(test)]
thread_local! {
    static SECRET_FAULT: std::cell::Cell<Option<(&'static str, Fault)>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
#[derive(Debug)]
struct SimulatedCrash(&'static str);

#[cfg(test)]
impl std::fmt::Display for SimulatedCrash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "simulated crash after {}", self.0)
    }
}

#[cfg(test)]
impl std::error::Error for SimulatedCrash {}

fn fault_point(step: &'static str) -> anyhow::Result<()> {
    #[cfg(test)]
    if let Some((at, how)) = SECRET_FAULT.with(|f| f.get()) {
        if at == step {
            return Err(match how {
                Fault::Fail => anyhow::anyhow!("injected failure at {step}"),
                Fault::Crash => anyhow::Error::new(SimulatedCrash(step)),
            });
        }
    }
    let _ = step;
    Ok(())
}

/// 模擬的行程死亡：死掉的行程不會跑任何收拾。
fn crashed(e: &anyhow::Error) -> bool {
    #[cfg(test)]
    let hit = e.downcast_ref::<SimulatedCrash>().is_some();
    #[cfg(not(test))]
    let hit = {
        let _ = e;
        false
    };
    hit
}

/// 把新密碼寫成一份**還沒有任何設定指到**的 `remote-cargo-password.<id>`，回傳 id（issue #104）。
///
/// 0600 是 open(2) 建檔時就給的，不是事後 chmod——以前 `fs::write` 完才 chmod，中間檔案是 umask 決定的 0644，
/// 這時 crash 就永遠留著一份別人讀得到的密碼。寫完 fsync 才 rename 成正式檔名，看得到的正式檔一定是完整的；
/// 中途失敗就刪掉暫存檔。現在在用的密碼檔從頭到尾沒被碰。
fn stage_password(data_dir: &Path, password: &str) -> anyhow::Result<String> {
    let id = format!("{:016x}", rand::random::<u64>());
    let path = data_dir.join(format!("{PASSWORD_FILE}.{id}"));
    let tmp = data_dir.join(format!("{PASSWORD_FILE}.{id}.tmp"));
    let res = (|| -> anyhow::Result<()> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        fault_point("created")?;
        f.write_all(password.as_bytes())?;
        fault_point("written")?;
        f.sync_all()?;
        fault_point("synced")?;
        drop(f);
        std::fs::rename(&tmp, &path)?;
        fault_point("renamed")?;
        // 目錄也 fsync，斷電後新檔名才一定在（有些檔案系統不支援對目錄 fsync：盡力而為）。
        if let Ok(d) = std::fs::File::open(data_dir) {
            let _ = d.sync_all();
        }
        Ok(())
    })();
    match res {
        Ok(()) => Ok(id),
        Err(e) if crashed(&e) => Err(e),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            let _ = std::fs::remove_file(&path);
            Err(e)
        }
    }
}

/// 刪掉 data-dir 裡所有不是 `remote` 指到的密碼檔（舊的 id、舊版固定檔名、沒 rename 完的暫存檔）。
fn remove_unused_passwords(data_dir: &Path, remote: &BuildRemoteCfg) -> anyhow::Result<()> {
    let keep = password_path(data_dir, remote)?;
    fault_point("cleanup")?;
    let prefix = format!("{PASSWORD_FILE}.");
    for e in std::fs::read_dir(data_dir)?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if (name == PASSWORD_FILE || name.starts_with(&prefix)) && keep.as_deref() != Some(e.path().as_path()) {
            std::fs::remove_file(e.path())?;
        }
    }
    Ok(())
}

fn commit_lock() -> &'static tokio::sync::Mutex<()> {
    static L: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    L.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// 換設定與密碼（issue #104）。唯一的提交點是 config.toml 的 rename（[`crate::config::write_atomic`]）：
/// 1. 新密碼先寫成一份還沒人指到的 `remote-cargo-password.<新 id>`（[`stage_password`]）；
/// 2. config.toml 一次換成「新設定＋`password_id` 指向新檔」；
/// 3. 才刪掉沒有設定指到的舊密碼檔。
///
/// 任何一步失敗或行程死掉，重讀磁碟只會是「舊設定＋舊密碼」或「新設定＋新密碼」。以前先改 config 再寫密碼：
/// 寫密碼失敗就停在新主機配舊密碼，下一次呼叫會把舊主機的密碼送去新主機。
///
/// `password`：`None`＝沿用現在的密碼（`password_id` 不變）；`Some("")`＝清掉改用 key/agent；其他＝新密碼。
async fn commit_remote(
    store: &crate::config::ConfigStore,
    data_dir: &Path,
    password: Option<&str>,
    apply: impl FnOnce(&mut BuildRemoteCfg),
) -> anyhow::Result<BuildRemoteCfg> {
    // 兩次儲存交錯的話，先提交的那次清舊檔時，會把另一次剛寫好、還沒提交的新密碼檔當成「沒人指到」刪掉。
    let _one_at_a_time = commit_lock().lock().await;
    let staged = match password {
        None => None,
        Some("") => Some(String::new()),
        Some(pw) => Some(stage_password(data_dir, pw)?),
    };
    let committed = store
        .update(|cfg| {
            apply(&mut cfg.build.remote);
            if let Some(id) = &staged {
                cfg.build.remote.password_id = Some(id.clone());
            }
            fault_point("config")?;
            Ok(cfg.build.remote.clone())
        })
        .await;
    let remote = match committed {
        Ok(r) => r,
        Err(e) => {
            if let Some(id) = staged.as_deref().filter(|id| !id.is_empty() && !crashed(&e)) {
                let _ = std::fs::remove_file(data_dir.join(format!("{PASSWORD_FILE}.{id}")));
            }
            return Err(e);
        }
    };
    // 已經提交了：清舊檔失敗不算這次失敗（剩下的是沒人指到的 0600 舊檔，下次儲存再清）。
    if let Err(e) = remove_unused_passwords(data_dir, &remote) {
        if crashed(&e) {
            return Err(e);
        }
        tracing::warn!("remote Cargo: 清掉沒在用的舊密碼檔失敗：{e:#}");
    }
    Ok(remote)
}

fn sanitized(cfg: &BuildRemoteCfg, data_dir: &Path) -> Value {
    json!({
        "enabled": cfg.enabled,
        "host": cfg.host,
        "user": cfg.user,
        "ssh_port": cfg.ssh_port,
        "remote_root": cfg.remote_root,
        "cargo_jobs": cfg.cargo_jobs,
        "test_threads": cfg.test_threads,
        "timeout_secs": cfg.timeout_secs,
        "shared_idle_hours": cfg.shared_idle_hours,
        "max_shared_dirs": cfg.max_shared_dirs,
        "max_concurrent": cfg.max_concurrent,
        "password_set": password_is_set(data_dir, cfg),
        // 之後的建置主機（#1956）：唯讀，只在 config.toml 的 `[[build.remote.hosts]]` 改。
        "hosts": cfg.targets().iter().skip(usize::from(!cfg.host.trim().is_empty())).map(|h| json!({
            "target": h.target_label(),
            "host": h.host,
            "user": h.user,
            "ssh_port": h.ssh_port,
            "cargo_jobs": h.cargo_jobs,
            "max_concurrent": h.max_concurrent,
        })).collect::<Vec<_>>(),
    })
}

/// host／user／remote_root 的字元限制：這三個會進 ssh 的引數與遠端 shell（rsync 的遠端路徑、守門腳本）。
/// API 寫入時擋（[`put_settings`]），讀 config.toml 要轉遠端時也擋（[`decide_offload`]）——設定檔手改得到，
/// 不能讓 `host = "-oProxyCommand=…"` 變成 ssh 的選項、`remote_root` 的 `$(…)` 被遠端 shell 展開。`remote_root` 空＝用預設。
fn validate_target(host: &str, user: &str, remote_root: &str) -> Result<(), &'static str> {
    let safe_host = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':');
    let safe_user = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_');
    let safe_root = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '/');
    if !host.chars().all(safe_host) || host.starts_with('-') {
        return Err("remote Cargo host 含不支援的字元");
    }
    if !user.chars().all(safe_user) || user.starts_with('-') {
        return Err("remote Cargo user 含不支援的字元");
    }
    if !remote_root.is_empty() && (!remote_root.chars().all(safe_root) || remote_root.contains("..") || remote_root.starts_with('-')) {
        return Err("remote_root 只能使用英數、._-/，不可含 ..，也不可以 - 開頭");
    }
    Ok(())
}

pub async fn get_settings(State(app): State<Arc<App>>) -> Json<Value> {
    let cfg = app.cfg.get().await;
    Json(sanitized(&cfg.build.remote, &app.data_dir))
}

pub async fn put_settings(
    State(app): State<Arc<App>>,
    Json(input): Json<RemoteBuildInput>,
) -> Result<Json<Value>, LcError> {
    let host = input.host.trim().to_string();
    let user = input.user.trim().to_string();
    let remote_root = input.remote_root.trim().to_string();
    // `host = "local"`＝本機執行器（#1956），不用 user。
    if input.enabled && (host.is_empty() || (user.is_empty() && host != crate::config::LOCAL_BUILD_HOST)) {
        return Err(LcError::Bad("remote Cargo 啟用時 host 與 user 都必填（本機執行器 host = \"local\" 不用 user）".into()));
    }
    validate_target(&host, &user, &remote_root).map_err(|e| LcError::Bad(e.into()))?;
    if input.ssh_port == 0 {
        return Err(LcError::Bad("ssh_port must be 1..65535".into()));
    }
    let cargo_jobs = if input.cargo_jobs == 0 { 4 } else { input.cargo_jobs.min(64) };
    if input.shared_idle_hours == Some(0) {
        return Err(LcError::Bad("shared_idle_hours 至少 1（要清就把 max_shared_dirs 設小）".into()));
    }
    if input.max_concurrent.is_some_and(|n| n > MAX_CONCURRENT) {
        return Err(LcError::Bad(format!("max_concurrent 最多 {MAX_CONCURRENT}（0＝依遠端核數與 RAM 自動算）")));
    }
    if input.timeout_secs.is_some_and(|t| t > MAX_TIMEOUT_SECS) {
        return Err(LcError::Bad(format!("timeout_secs 最多 {MAX_TIMEOUT_SECS} 秒（0＝不設上限）")));
    }
    // 密碼不進 config.toml；設定與密碼一起提交（見 `commit_remote`），失敗時兩邊都還是舊的。
    let remote = commit_remote(&app.cfg, &app.data_dir, input.password.as_deref(), |r| {
        r.enabled = input.enabled;
        r.host = host.clone();
        r.user = user.clone();
        r.ssh_port = input.ssh_port;
        r.remote_root = if remote_root.is_empty() { crate::config::default_remote_build_root() } else { remote_root.clone() };
        r.cargo_jobs = cargo_jobs;
        r.test_threads = input.test_threads.unwrap_or(r.test_threads).min(256);
        r.timeout_secs = input.timeout_secs.unwrap_or(r.timeout_secs);
        r.shared_idle_hours = input.shared_idle_hours.unwrap_or(r.shared_idle_hours);
        r.max_shared_dirs = input.max_shared_dirs.unwrap_or(r.max_shared_dirs);
        r.max_concurrent = input.max_concurrent.unwrap_or(r.max_concurrent);
    })
    .await
    .map_err(|e| {
        // 寫之前磁碟被手改：這次沒寫、可重送（409），其餘照舊當上游錯誤（#1647）。
        if e.downcast_ref::<crate::config::ConfigChangedOnDisk>().is_some() {
            crate::api::cfg_err(e)
        } else {
            LcError::Upstream(format!("store remote Cargo settings: {e:#}"))
        }
    })?;
    Ok(Json(sanitized(&remote, &app.data_dir)))
}

pub async fn test_settings(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let cfg = app.cfg.get().await;
    let remote = cfg.build.remote.clone();
    if remote.host.trim().is_empty() || (remote.user.trim().is_empty() && !remote.is_local()) {
        return Err(LcError::Bad("remote Cargo host 尚未設定".into()));
    }
    let data_dir = app.data_dir.clone();
    let result = tokio::task::spawn_blocking(move || probe(&remote, &data_dir))
        .await
        .map_err(|e| LcError::Upstream(e.to_string()))?
        .map_err(|e| LcError::Upstream(e.to_string()))?;
    Ok(Json(result))
}

fn has_program(name: &str) -> bool {
    let script = format!("command -v {} >/dev/null 2>&1", sh_quote(name));
    Command::new("sh").arg("-c").arg(script).status().map(|s| s.success()).unwrap_or(false)
}

/// ssh 問密碼時由這支回答。密碼**只走環境變數**（`AM_SSH_PASSWORD`），不進 argv——`ps` 看得到
/// 別人的 argv，看不到別人的環境。
///
/// 2026-09-18：這台機器沒有 `sshpass`（macOS 的 homebrew core 也沒有這個 formula），設了密碼的
/// 遠端 Cargo 一按「測試」就整個擋下來。OpenSSH 8.4 起有 `SSH_ASKPASS_REQUIRE=force`，不需要
/// 終端機也不需要 `DISPLAY`，這條路不必再多裝一個東西。
const ASKPASS_FILE: &str = "remote-cargo-askpass.sh";
const ASKPASS_SH: &str = "#!/bin/sh\n# agents-manager: ssh/rsync 問密碼時回答它；密碼只從環境變數來。\nprintf '%s\\n' \"$AM_SSH_PASSWORD\"\n";

fn askpass_helper(data_dir: &Path) -> anyhow::Result<PathBuf> {
    let path = data_dir.join(ASKPASS_FILE);
    if std::fs::read_to_string(&path).ok().as_deref() != Some(ASKPASS_SH) {
        std::fs::write(&path, ASKPASS_SH)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(path)
}

/// 這台機器的 ssh 有沒有 `SSH_ASKPASS_REQUIRE`（OpenSSH 8.4+）。`ssh -V` 印在 stderr。
fn ssh_supports_askpass_require() -> bool {
    let Ok(out) = Command::new("ssh").arg("-V").output() else { return false };
    let v = String::from_utf8_lossy(&out.stderr);
    let Some(rest) = v.split("OpenSSH_").nth(1) else { return false };
    openssh_at_least_8_4(rest)
}

/// `9.8p1 …` / `8.4p1` / `10.3p1` → 夠不夠新。字串比大小會說 10 < 8，所以拆成數字比。
pub fn openssh_at_least_8_4(rest: &str) -> bool {
    let head: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let mut it = head.split('.');
    let (Some(Ok(major)), minor) = (it.next().map(str::parse::<u32>), it.next().and_then(|m| m.parse::<u32>().ok())) else {
        return false;
    };
    major > 8 || (major == 8 && minor.unwrap_or(0) >= 4)
}

/// 密碼要怎麼餵給 ssh／rsync：有 `sshpass` 就照舊，沒有就走 askpass。兩條都不行才報錯。
enum PwMode {
    Sshpass,
    Askpass(PathBuf),
}

fn password_mode(data_dir: &Path) -> anyhow::Result<PwMode> {
    if has_program("sshpass") {
        return Ok(PwMode::Sshpass);
    }
    if ssh_supports_askpass_require() {
        return Ok(PwMode::Askpass(askpass_helper(data_dir)?));
    }
    anyhow::bail!(
        "這台機器沒有 `sshpass`，ssh 也太舊（OpenSSH 8.4 以下沒有 SSH_ASKPASS_REQUIRE）。裝 sshpass、升級 ssh，或把密碼清掉改用 SSH 金鑰／agent"
    )
}

/// 這份設定用的密碼。設定明確指到一份密碼檔（`password_id`）而它不見了是錯誤，不悄悄改成 key/agent。
fn secret(data_dir: &Path, remote: &BuildRemoteCfg) -> anyhow::Result<Option<String>> {
    let Some(path) = password_path(data_dir, remote)? else { return Ok(None) };
    match std::fs::read_to_string(&path) {
        Ok(s) => {
            let s = s.trim_end_matches(|c| c == '\r' || c == '\n').to_string();
            Ok((!s.is_empty()).then_some(s))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && remote.password_id.is_none() => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            anyhow::bail!("設定指到的密碼檔 {} 不見了；到設定頁重新輸入密碼（或清掉改用 SSH 金鑰）", path.display())
        }
        Err(e) => Err(e.into()),
    }
}

/// 連線存活選項（#174），ssh 與 rsync 的 `-e` 共用：對端連上之後靜默消失（Wi-Fi 換 AP、Mac 睡著醒來 IP 變了、遠端掉電）時，
/// 沒有這些 ssh 只能等 TCP keepalive（預設 2 小時），helper 與 agent 的 cargo 一路卡著，遠端的鎖與目錄也一直被佔。
/// `ConnectTimeout` 連握手一起算（連上就不說話的對端）；`ServerAlive*` 管連上之後——15 秒問一次、連 3 次沒回（約 45 秒）就斷。
/// 跟 `hosts.rs::ssh_args` 的節奏一致。
const SSH_LIVENESS_OPTS: [&str; 6] = ["-o", "ConnectTimeout=15", "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=3"];

/// 設了 `identity_file` 卻讀不到＝明確報錯（ssh 對不存在的 `-i` 只警告，會悄悄改試別把 key）。
fn check_identity(remote: &BuildRemoteCfg) -> anyhow::Result<()> {
    if !remote.identity_file.is_empty() && !Path::new(&remote.identity_file).is_file() {
        anyhow::bail!("[build.remote] identity_file 找不到：{}", remote.identity_file);
    }
    Ok(())
}

fn ssh_base(remote: &BuildRemoteCfg, password: Option<&str>, data_dir: &Path) -> anyhow::Result<Command> {
    check_identity(remote)?;
    let mode = match password {
        Some(_) => Some(password_mode(data_dir)?),
        None => None,
    };
    Ok(ssh_cmd(remote, password, mode.as_ref()))
}

/// [`ssh_base`] 的純組裝部分（`mode` 已經決定好），好讓「密碼怎麼餵進去」測得到。
fn ssh_cmd(remote: &BuildRemoteCfg, password: Option<&str>, mode: Option<&PwMode>) -> Command {
    let mut cmd = match &mode {
        Some(PwMode::Sshpass) => {
            let mut c = Command::new("sshpass");
            c.arg("-e").arg("ssh");
            c
        }
        _ => Command::new(ssh_program()),
    };
    if let (Some(pw), Some(m)) = (password, mode) {
        match m {
            PwMode::Sshpass => {
                cmd.env("SSHPASS", pw);
            }
            PwMode::Askpass(helper) => {
                cmd.env("AM_SSH_PASSWORD", pw)
                    .env("SSH_ASKPASS", helper)
                    .env("SSH_ASKPASS_REQUIRE", "force")
                    // 只問一次：密碼錯的話要當場失敗，不要卡在互動提示上。
                    .arg("-o")
                    .arg("NumberOfPasswordPrompts=1");
            }
        }
    }
    cmd.arg("-p")
        .arg(remote.ssh_port.to_string())
        .args(SSH_LIVENESS_OPTS)
        .arg("-o")
        .arg("StrictHostKeyChecking=accept-new")
        .arg("-o")
        .arg(if password.is_some() { "BatchMode=no" } else { "BatchMode=yes" });
    if !remote.identity_file.is_empty() {
        cmd.arg("-i").arg(&remote.identity_file).arg("-o").arg("IdentitiesOnly=yes");
    }
    cmd.arg(format!("{}@{}", remote.user, remote.host));
    cmd
}

/// 在建置主機上跑一段 POSIX sh（#1956）：ssh 主機照舊是 ssh＋登入 shell 包一層 `sh -c`；
/// 本機執行器（`host = "local"`）直接 `sh -c`，其餘（守門腳本、目錄配置、manifest、失效 touch）全部同一套。
fn host_shell(remote: &BuildRemoteCfg, data_dir: &Path, script: &str) -> anyhow::Result<Command> {
    if remote.is_local() {
        return local_shell(script);
    }
    let pw = secret(data_dir, remote)?;
    let mut cmd = ssh_base(remote, pw.as_deref(), data_dir)?;
    cmd.arg(login_shell_arg(script));
    Ok(cmd)
}

/// probe 在遠端跑的那一行。`~/.cargo/bin` 一併看：rustup 裝好之後**只**改 shell profile，
/// 而 ssh 的非互動 shell 不讀 profile，`command -v cargo` 會說沒有（2026-09-18）。
/// clippy 也看：它是轉過去的三個指令之一，rustup 的 minimal profile 不含它（issue #104）。
const PROBE_SH: &str = "printf 'OS='; uname -s; printf 'ARCH='; uname -m; \
PATH=\"$HOME/.cargo/bin:$PATH\"; echo \"CARGO=$(command -v cargo)\"; cargo --version 2>/dev/null || true; \
echo \"CLIPPY=$(cargo clippy --version 2>/dev/null)\"";

/// probe 的輸出拆成前端看得懂的欄位。**沒有 cargo 不是錯誤**——連得上只是還沒裝工具鏈，UI 要
/// 能直接提示「幫你裝」，而不是丟一段原始輸出讓人猜（使用者 2026-09-18）。
pub fn parse_probe(stdout: &str) -> (Option<String>, Option<String>, Option<String>, Option<String>) {
    let (mut os, mut arch, mut cargo, mut version) = (None, None, None, None);
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("OS=") {
            os = (!v.is_empty()).then(|| v.to_string());
        } else if let Some(v) = line.strip_prefix("ARCH=") {
            arch = (!v.is_empty()).then(|| v.to_string());
        } else if let Some(v) = line.strip_prefix("CARGO=") {
            cargo = (!v.is_empty()).then(|| v.to_string());
        } else if line.starts_with("cargo ") {
            version = Some(line.to_string());
        }
    }
    (os, arch, cargo, version)
}

/// probe 輸出裡的 `CLIPPY=<版本>`；空的＝沒有 clippy。
pub fn probe_clippy(stdout: &str) -> Option<String> {
    stdout.lines().find_map(|l| l.trim().strip_prefix("CLIPPY=")).filter(|v| !v.is_empty()).map(str::to_string)
}

/// probe／安裝工具鏈這類「單次 ssh、收 stdout」的呼叫的上限（#329）：遠端卡住（NFS、hung sshd）時不能永遠佔著一條 API 請求執行緒。
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);
const INSTALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20 * 60);

/// 同 `Command::output`，但超過 `limit` 就砍掉子行程並回錯。
fn output_with_timeout(mut cmd: Command, limit: std::time::Duration) -> anyhow::Result<std::process::Output> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = cmd.spawn()?;
    let pid = child.id() as i32;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(limit) {
        Ok(out) => Ok(out?),
        Err(_) => {
            // 執行緒握著 Child；只能用 pid 砍（它還沒被收掉，pid 不會被回收重用）。
            unsafe { libc::kill(pid, libc::SIGKILL) };
            anyhow::bail!("ssh 超過 {} 秒沒有結束，已中止", limit.as_secs())
        }
    }
}

fn output_with_deadline(
    mut cmd: Command,
    what: &str,
    deadline: &Deadline,
) -> anyhow::Result<std::process::Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = cmd.spawn().map_err(|e| anyhow::anyhow!("{what}: {e}"))?;
    let pid = child.id() as i32;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    loop {
        if let Some(why) = deadline.stop() {
            unsafe { libc::kill(pid, libc::SIGKILL) };
            anyhow::bail!("{what}: {why}");
        }
        match rx.recv_timeout(std::time::Duration::from_millis(20)) {
            Ok(out) => return Ok(out?),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("{what}: output reader stopped")
            }
        }
    }
}

fn probe(remote: &BuildRemoteCfg, data_dir: &Path) -> anyhow::Result<Value> {
    let pw = if remote.is_local() { None } else { secret(data_dir, remote)? };
    let cmd = host_shell(remote, data_dir, PROBE_SH)?;
    let out = output_with_timeout(cmd, PROBE_TIMEOUT)?;
    if !out.status.success() {
        anyhow::bail!("ssh probe failed (exit {:?}): {}", out.status.code(), String::from_utf8_lossy(&out.stderr).trim());
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let (os, arch, cargo, version) = parse_probe(&stdout);
    let clippy = probe_clippy(&stdout);
    Ok(json!({
        "ok": true,
        "output": stdout.trim(),
        "password_auth": pw.is_some(),
        "os": os,
        "arch": arch,
        "cargo_path": cargo,
        "cargo_version": version,
        "cargo_missing": cargo.is_none(),
        "clippy_version": clippy,
        // 有 cargo 卻沒有 clippy：`cargo clippy` 轉過去才會失敗，UI 要當場講、給安裝按鈕（安裝會補上）。
        "clippy_missing": cargo.is_some() && clippy.is_none(),
    }))
}

/// 在遠端裝 Rust 工具鏈。已經有了就什麼都不做（回報現有版本）。
///
/// `--profile minimal --no-modify-path`：我們只需要 cargo／rustc 跑 check/test/clippy，而且不要
/// 去動使用者的 shell profile——probe 與 `run_remote` 都自己把 `~/.cargo/bin` 接到 PATH 前面。
/// minimal 不含 clippy，而 `cargo clippy` 是會轉過去的三個指令之一（issue #104）：新裝的帶 `--component clippy`，
/// 本來就有 cargo 的也補一次（rustup 裝的才補得了），補不上就回報 `CLIPPY_MISSING=1`，不讓 UI 說「裝好了」卻第一次 clippy 才失敗。
const INSTALL_SH: &str = "set -e\n\
PATH=\"$HOME/.cargo/bin:$PATH\"\n\
if command -v cargo >/dev/null 2>&1; then\n\
  printf 'ALREADY='; cargo --version\n\
else\n\
  if ! command -v curl >/dev/null 2>&1; then echo 'remote host has no curl; install curl (or rustup) there first' >&2; exit 2; fi\n\
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --component clippy --no-modify-path\n\
  printf 'INSTALLED='; \"$HOME/.cargo/bin/cargo\" --version\n\
fi\n\
if ! cargo clippy --version >/dev/null 2>&1 && command -v rustup >/dev/null 2>&1; then rustup component add clippy >&2 || true; fi\n\
cargo clippy --version >/dev/null 2>&1 || echo 'CLIPPY_MISSING=1'\n\
command -v cc >/dev/null 2>&1 || command -v gcc >/dev/null 2>&1 || echo 'CC_MISSING=1'\n";

fn install_toolchain(remote: &BuildRemoteCfg, data_dir: &Path) -> anyhow::Result<Value> {
    let cmd = host_shell(remote, data_dir, INSTALL_SH)?;
    let out = output_with_timeout(cmd, INSTALL_TIMEOUT)?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !out.status.success() {
        anyhow::bail!("rustup install failed (exit {:?}): {}", out.status.code(), if stderr.is_empty() { stdout } else { stderr });
    }
    let already = stdout.contains("ALREADY=");
    // rustup 只裝 rust，不裝 linker。沒有 `cc` 的話 cargo test 會在連結那一步才爆，訊息很難懂
    // （2026-09-18 實測這台就是這樣），所以當場講出來。
    let cc_missing = stdout.lines().any(|l| l.trim() == "CC_MISSING=1");
    let clippy_missing = stdout.lines().any(|l| l.trim() == "CLIPPY_MISSING=1");
    let version = stdout
        .lines()
        .find_map(|l| l.strip_prefix("ALREADY=").or_else(|| l.strip_prefix("INSTALLED=")))
        .map(|v| v.trim().to_string());
    Ok(json!({
        "ok": true,
        "already_installed": already,
        "cargo_version": version,
        "cc_missing": cc_missing,
        "clippy_missing": clippy_missing,
        "output": stdout,
    }))
}

/// `POST /api/build/remote/install-toolchain`
pub async fn install_settings(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let cfg = app.cfg.get().await;
    let remote = cfg.build.remote.clone();
    if remote.host.trim().is_empty() || (remote.user.trim().is_empty() && !remote.is_local()) {
        return Err(LcError::Bad("remote Cargo host 尚未設定".into()));
    }
    let data_dir = app.data_dir.clone();
    tokio::task::spawn_blocking(move || install_toolchain(&remote, &data_dir))
        .await
        .map_err(|e| LcError::Upstream(e.to_string()))?
        .map(Json)
        .map_err(|e| LcError::Upstream(e.to_string()))
}

/// Only these commands are safe/useful to offload cross-platform in V1.
/// `build` stays local because Linux/x86_64 artifacts are not macOS/aarch64 artifacts.
///
/// 子指令前面可以有「原樣搬到遠端也是同一個意思」的全域旗標（`-v`／`-q`／`--locked`／`--color`，issue #195：`cargo --locked test` 也是驗證）。
/// `+toolchain`（遠端不一定裝了）、`-C`／`--config`（本機路徑）、`--offline`／`--frozen`（取決於本機的快取）、`-Z` 不搬——回 false 讓它留在本機、照本機名額排。
pub fn eligible(args: &[String]) -> bool {
    // Cache invalidation must inspect the exact target/build directory used by Cargo.
    // Command-line overrides may redirect the managed cache location; --config may set either.
    if args.iter().take_while(|a| a.as_str() != "--").any(|a| {
        a == "--target-dir" || a.starts_with("--target-dir=")
            || a == "--config" || a.starts_with("--config=")
    }) {
        return false;
    }
    let mut it = args.iter().map(String::as_str);
    while let Some(a) = it.next() {
        match a {
            "-v" | "-vv" | "--verbose" | "-q" | "--quiet" | "--locked" => continue,
            "--color" => {
                it.next();
            }
            _ if a.starts_with("--color=") => continue,
            "c" | "check" | "t" | "test" => return true,
            // `clippy --fix` 會改原始碼：改到的是 rsync 過去的遠端那份、不會同步回來，呼叫端以為修好了、本機一個字都沒變（issue #104）。
            // 只轉唯讀的驗證；`--` 後面是給 clippy-driver 的 lint 參數，不算。
            "clippy" => return !it.take_while(|a| *a != "--").any(|a| a == "--fix"),
            _ => return false,
        }
    }
    false
}

/// 會改變 cargo 編譯／檢查結果、但遠端的 ssh session 看不到的本機環境變數（#325）。
/// 例：`RUSTFLAGS="-D warnings" cargo clippy` 搬到遠端就沒有 `-D warnings`，遠端綠、本機會紅的假綠。
/// 有設的話這次不轉遠端（退回本機，並講出來），不能悄悄用不同的設定驗。
pub fn env_blocking_offload<I: IntoIterator<Item = (String, String)>>(vars: I) -> Option<String> {
    const EXACT: [&str; 11] = [
        "RUSTFLAGS",
        "RUSTDOCFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_ENCODED_RUSTDOCFLAGS",
        "CARGO_TARGET_DIR",
        "CARGO_BUILD_TARGET_DIR",
        "CARGO_BUILD_BUILD_DIR",
        "CARGO_BUILD_TARGET",
        "CARGO_BUILD_RUSTFLAGS",
        "CARGO_BUILD_RUSTDOCFLAGS",
        "CARGO_INCREMENTAL",
    ];
    vars.into_iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, _)| k)
        .find(|k| EXACT.contains(&k.as_str()) || k.starts_with("CARGO_PROFILE_") || k.starts_with("CARGO_FEATURE_") || (k.starts_with("CARGO_TARGET_") && k.ends_with("_RUSTFLAGS")))
}

fn fnv1a64(s: &str) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 同一個 repository 的 linked worktree 會回到同一個 git common dir；非 Git 目錄仍以自身隔離。
/// canonicalize 讓從 symlink 進入的同一份 repo 也使用相同身份。
fn cache_identity_path(cwd: &Path) -> PathBuf {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let common = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(&cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| PathBuf::from(s.trim()))
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| if p.is_absolute() { p } else { cwd.join(p) })
        .and_then(|p| p.canonicalize().ok());
    common.unwrap_or(cwd)
}

fn cache_hash(cwd: &Path) -> String {
    format!("{:016x}", fnv1a64(&cache_identity_path(cwd).to_string_lossy()))
}

/// ssh 把這條字串交給遠端的登入 shell（fish／tcsh 不吃 `VAR=value` 與 `$(…)`）。
/// 跟守門腳本一樣先包一層 `sh -c`，登入 shell 只負責把引數交給 `/bin/sh`（#670）。
pub fn login_shell_arg(script: &str) -> String {
    format!("sh -c {}", sh_quote(script))
}

pub fn sh_quote(s: &str) -> String {
    // 空字串要寫成 `''`：回空字串的話 `cargo test -- ''` 送到遠端會少一個引數。
    if !s.is_empty() && s.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_./:=@+".contains(&c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

// ── 遠端工作目錄（issue #141）──────────────────────────────────────────────────────────────
//
// 版面：`<remote_root>/<repo common dir 路徑的 fnv1a64>/`
//   - `shared/`＋`shared.lock`：同一個 repo 的 worktree 共用原始碼＋`target/`，跨呼叫保留（增量編譯）。
//   - `shared-<N>/`＋`.lock`（#1956）：同一個池的其他份，池的大小＝這台的名額數。守門拿第一份沒人持鎖的；
//     第一次用到某一份時從池裡另一份複製出來（守門的 `seed`），之後各自增量。好幾個 worktree 同時編，各拿一份熱的，不再串成一列。
//   - `job-<pid>-<ms>/`＋`.lock`：池裡每一份都被佔著、等了 [`SHARED_WAIT_SECS`] 還沒有任何一份空出來時的退路，冷編譯，結束就刪。
//   - 純數字的 `<pid>/`：#141 之前的版本留下的，沒有鎖，只能靠「沒行程在用＋放很久」回收。
// 另有 `<remote_root>/.slots/<n>`：遠端名額（issue #104）。守門拿到目錄之後再搶一個 `flock`，拿著直到結束——同一個 remote_root
// 不分 worktree 同時最多 `max_concurrent` 個遠端編譯，其餘排隊。鎖跟著守門 shell 的 fd 走，守門怎麼死都會放。
//
// 每次呼叫先開一條「守門」ssh（[`guard_script`]）：它在遠端拿 flock、把目錄交給這次呼叫，然後讀 stdin
// 等到 EOF 才清理。helper 正常結束、失敗、被 Ctrl-C／SIGTERM／kill -9，本機這端的 pipe 都會關掉，遠端就
// 看到 EOF——**清理不靠本機活著做完**（2026-09-19 實測：本機 ssh 被 kill -9，遠端 shell 照樣讀到 EOF 跑完）。
// 沒有 pty 的 ssh 斷線後遠端 cargo 不會收到訊號、會繼續跑（同日實測），所以 run 的那條 ssh 先把自己的
// process group 寫下來（`<dir>.pgid-<token>`），守門清理時整組收掉再刪目錄。
//
// 租約身份（issue #148）是本機每次呼叫新產生的 [`LeaseToken`]（128 位元隨機），不是遠端 shell 的 `$$`：
// PID 會被回收重用，「值一樣」不等於「同一次租約」。守門只把 `$$` 拿來做行程簿記（自己的 process group、
// `/proc/$$/fd`），`.owner`、`.pgid-<token>` 與交給 run 的 token 全都用這個。

/// 沒有鎖、也沒有行程的 cwd 在裡面，放這麼久就當孤兒刪（守門被 kill 在遠端那頭、舊版留下的 `<pid>/`）。
const ORPHAN_IDLE_SECS: u64 = 10 * 60;
/// `shared/` 是快取不是垃圾：閒置這麼久才收（worktree 早就合併刪掉的那些）。
const SHARED_IDLE_SECS: u64 = 3 * 3600;
/// 遠端磁碟剩不到這個比例時，閒置的 `shared/` 也照孤兒的門檻收（#141：一小時塞滿 97G）。
const LOW_DISK_FREE_PCT: u64 = 25;

/// 共用 target 池（`shared`、`shared-<N>`）每一份都被佔著時，最多等這麼久才改用冷編譯的 `job-*`（issue #104、#1937、#1956）。
/// 池的份數＝名額數，所以池全忙＝名額全滿：這時改走 `job-*` 也拿不到名額、只會退回本機冷編，上限縮短只會更慢（#1956 的取捨）；
/// 等待結束條件是「任何一份空出來」，實際等待約是單份時的 1/N。
/// 持有者佔的是整段 cargo（編譯＋測試），不是只有增量的十幾秒；#1856 之後同一個 repo 的所有 worktree 共用這一份 `shared/`，
/// 排在前面的好幾顆 child 都可能要跑好幾分鐘。r7 只有 1 個遠端名額（`max_concurrent = 1`）：持有者還在跑時，改用 `job-*` 也拿不到
/// 遠端名額，所以舊的 5 分鐘一到只能改在本機冷編譯（#1886 的 log：等滿 300 秒 → 改用獨立目錄 → 遠端名額全滿 → 本機排隊，本機跑了 8 分多）。
/// 所以上限改成一次冷編譯加餘裕：持有者本身是冷編譯時也等得到，只有真的卡住才退回。整體上限 `timeout_secs` 照舊是硬界線。
/// 拿到 `shared/` 時守門會印「等了 N 秒拿到共用遠端 target」（N > 0 才印），給 #1937 統計實際等待分布。
const SHARED_WAIT_SECS: u64 = 15 * 60;
/// 遠端是額外名額：短暫等候仍全滿就退回本機名額（issue #1853）。
/// 守門仍回 75 表示尚未開工；helper 將這個交握結果轉成 125。
const QUEUE_WAIT_SECS: u64 = 3;
/// `max_concurrent = 0`（自動）的記憶體帳：留給系統這麼多，每個遠端編譯算 [`AUTO_PER_JOB_KB`]。
/// 2026-09-19 在 32 vCPU／64 GiB 的遠端量：一次冷的 `cargo test -p agents-managerd` 光主 crate 那顆 rustc 就多吃約 4.6 GiB，
/// 兩次同時跑時全機用到 8.2 GiB；再加上測試本身（8 個執行緒、各自起子行程）與 lib／測試 harness 兩顆大 rustc 重疊，一次抓 8 GiB。
const AUTO_RESERVE_KB: u64 = 8 * 1024 * 1024;
/// 2026-10-24 本機實測（#1956）：`agents_managerd` 那顆 rustc 一次約 13.5 GB RSS，抓 14 GiB。62 GB 的本機 → 3 份；15 GB 的 r7 → 1 份。
const AUTO_PER_JOB_KB: u64 = 14 * 1024 * 1024;

/// 守門的排隊參數（issue #104）。正式呼叫由設定與上面的常數組成（[`Admission::from_cfg`]）；測試用短的等待時間。
#[derive(Debug, Clone, Copy)]
struct Admission {
    /// 同時最多幾個遠端編譯，也是每個 repo 共用 target 池的份數（#1956）；`0`＝守門依遠端的核數與 RAM 算：
    /// `min(核數 × 3 ÷ cargo_jobs, (RAM − 8 GiB) ÷ 14 GiB)`，至少 1。CPU 容許 3 倍超賣（一次編譯大多時間只有一顆 rustc 在跑，
    /// `cargo_jobs` 是尖峰；#1956 實測瓶頸是單顆 `agents_managerd` 串行，不是核數），記憶體不超賣——2026-09-19 就是 9 個冷編譯同時跑把遠端壓垮。
    /// 16 核／62 GiB、`cargo_jobs = 12` → 3；32 核／64 GiB → 4；15 GiB 的 r7 → 1。
    max: usize,
    /// 自動算時每個編譯算幾個核（`cargo_jobs`）。
    jobs: usize,
    shared_wait_secs: u64,
    queue_wait_secs: u64,
    /// 共用 target 池的份數（#1956）；`0`＝跟 `max`（算出來的名額數）一樣。正式設定一律 0，測試用 1 走舊的「單份＋`job-*`」路徑。
    pool: usize,
    /// 多台挑主機的第一輪（#1956）：不等任何鎖、不改走冷目錄，拿不到就回 `BUSY`（[`Busy`]）。
    try_only: bool,
    /// 試用時 1 分鐘負載超過這個值就當沒有空槽（只有本機執行器設，見 [`hosts::local_max_load`]）。
    max_load: Option<f64>,
}

/// 排遠端 shared 目錄的等待秒數：正式是 [`SHARED_WAIT_SECS`]；測試覆寫見 [`TEST_SHARED_WAIT_SECS`]。
fn shared_wait_secs() -> u64 {
    #[cfg(test)]
    {
        if let Some(secs) = TEST_SHARED_WAIT_SECS.with(|w| w.get()) {
            return secs;
        }
    }
    SHARED_WAIT_SECS
}

/// 守門讀的負載檔（腳本的 `@LOADAVG@`）：正式一律 `/proc/loadavg`；測試覆寫見 [`TEST_LOADAVG`]。
fn loadavg_path() -> String {
    #[cfg(test)]
    {
        if let Some(p) = TEST_LOADAVG.with(|p| p.borrow().clone()) {
            return p.display().to_string();
        }
    }
    "/proc/loadavg".to_string()
}

impl Admission {
    fn from_cfg(remote: &BuildRemoteCfg) -> Self {
        Admission {
            max: remote.max_concurrent,
            jobs: remote.cargo_jobs.max(1),
            shared_wait_secs: shared_wait_secs(),
            queue_wait_secs: Self::queue_wait_secs(),
            pool: 0,
            try_only: false,
            max_load: None,
        }
    }

    fn queue_wait_secs() -> u64 {
        #[cfg(test)]
        {
            if let Some(secs) = TEST_QUEUE_WAIT_SECS.with(|w| w.get()) {
                return secs;
            }
        }
        QUEUE_WAIT_SECS
    }
}

/// 一次租約的身份（fencing token）：本機產生的 128 位元隨機數，固定 32 個小寫 hex。
///
/// 只能由 [`LeaseToken::new`]（隨機）得到（測試另有驗過格式的 `parse`），所以直接嵌進遠端 shell 與
/// 檔名（`<dir>.owner` 的內容、`<dir>.pgid-<token>`）都不會變成 command injection 或路徑穿越。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseToken(String);

impl LeaseToken {
    pub fn new() -> Self {
        Self(format!("{:032x}", rand::random::<u128>()))
    }

    /// 正式碼只用 [`LeaseToken::new`]；`parse` 是測試拿固定值、以及釘住「什麼樣的字串算合法」用的。
    #[cfg(test)]
    pub fn parse(s: &str) -> Option<Self> {
        (s.len() == 32 && s.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))).then(|| Self(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 守門連線在遠端跑的腳本（POSIX sh；遠端要是有 flock 與 /proc 的 Linux）。
///
/// stdout 協定：排隊時每秒一行 `Q`（心跳，見 `alive`）；拿到目錄與名額後 `ROOT\t<絕對路徑>`、`DIR\t<這次的工作目錄>\t<token，原樣回>`、
/// `SLOT\t<第幾個名額>\t<名額總數>`、每個候選目錄一行 `D\t<路徑>\t<鎖被持有>\t<有行程 cwd 在裡面>\t<閒置秒數>`、`F\t<剩餘 KB>\t<總 KB>`，最後 `END`。
/// 之後 stdin 每行 `rm <路徑>` 是要它回收的孤兒（它自己拿鎖、重看一次有沒有人在用才刪），EOF＝這次結束。
/// 排遠端名額排超過 `queue_wait_secs` 就不交目錄、exit 75（[`EXIT_QUEUE_FULL`]）。
fn guard_script(root: &str, hash: &str, job: &str, token: &LeaseToken, adm: Admission) -> String {
    GUARD_SH
        .replace("@ROOT@", &sh_quote(root))
        .replace("@HASH@", &sh_quote(hash))
        .replace("@JOB@", &sh_quote(job))
        .replace("@TOKEN@", &sh_quote(token.as_str()))
        .replace("@MAX@", &adm.max.to_string())
        .replace("@JOBS@", &adm.jobs.max(1).to_string())
        .replace("@SWAIT@", &adm.shared_wait_secs.to_string())
        .replace("@QWAIT@", &adm.queue_wait_secs.to_string())
        .replace("@POOL@", &adm.pool.to_string())
        .replace("@TRY@", if adm.try_only { "1" } else { "0" })
        .replace("@MAXLOAD@", &adm.max_load.filter(|m| *m > 0.0).map(|m| format!("{m:.2}")).unwrap_or_else(|| "0".into()))
        .replace("@LOADAVG@", &sh_quote(&loadavg_path()))
        .replace("@RESERVE_KB@", &AUTO_RESERVE_KB.to_string())
        .replace("@PER_JOB_KB@", &AUTO_PER_JOB_KB.to_string())
        .replace("@HEX16@", &"[0-9a-f]".repeat(16))
}

const GUARD_SH: &str = r#"set -u
trap '' HUP PIPE
root=@ROOT@; hash=@HASH@; job=@JOB@; token=@TOKEN@
max=@MAX@; jobs=@JOBS@; swait=@SWAIT@; qwait=@QWAIT@; pool=@POOL@; try=@TRY@; maxload=@MAXLOAD@
if ! command -v flock >/dev/null 2>&1 || [ ! -r /proc/self/stat ]; then
  echo 'agents-manager: remote Cargo 的遠端要有 flock（util-linux）與 /proc（Linux）' >&2; exit 2
fi
mkdir -p "$root" && cd "$root" && root=$(pwd -P) || exit 2
# 名額數 0＝依這台的核數與 RAM 算（算式與理由見 helper 的 `Admission::max`）。
if [ "$max" -eq 0 ]; then
  cpu=$(getconf _NPROCESSORS_ONLN 2>/dev/null); case "$cpu" in ''|*[!0-9]*) cpu=1 ;; esac
  mem=$(awk '/^MemTotal:/ { print $2 }' /proc/meminfo 2>/dev/null); case "$mem" in ''|*[!0-9]*) mem=0 ;; esac
  max=$((cpu * 3 / jobs)); b=$(((mem - @RESERVE_KB@) / @PER_JOB_KB@))
  [ "$b" -lt "$max" ] && max=$b
  [ "$max" -ge 1 ] || max=1
fi
# 共用 target 池的份數（#1956）：0＝跟名額數一樣。
[ "$pool" -ge 1 ] || pool=$max
# `$$` 只做行程簿記（自己的 process group 不能被收、`/proc/$$/fd`）；租約身份是本機給的 $token，PID 會被回收重用。
me=$(ps -o pgid= -p $$ | tr -d ' ')
# 只碰自己命名規則的目錄：remote_root 設錯（例如設成家目錄）也不會去刪別人的東西。
ours() {
  r=${1#"$root"/}
  [ "$r" != "$1" ] || return 1
  case "$r" in */*/*) return 1 ;; */*) ;; *) return 1 ;; esac
  case "${r%/*}" in @HEX16@) ;; *) return 1 ;; esac
  x=${r#*/}
  case "$x" in
    shared) return 0 ;;
    shared-*) x=${x#shared-}; case "$x" in ''|*[!0-9]*) return 1 ;; esac; return 0 ;;
    job-*) x=${x#job-}; case "$x" in ''|*[!0-9-]*) return 1 ;; esac; return 0 ;;
  esac
  case "$x" in ''|*[!0-9]*) return 1 ;; esac
  return 0
}
cwds() { for p in /proc/[0-9]*; do readlink "$p/cwd"; done 2>/dev/null | sed 's|$|/|'; }
# 鎖檔在 open 與 flock 之間被回收刪掉的話，鎖到的是一個沒有名字的 inode：要確認路徑還指著自己鎖的那個。
same() { a=$(stat -L -c %d:%i "$1" 2>/dev/null) && [ -n "$a" ] && [ "$a" = "$(stat -L -c %d:%i "/proc/$$/fd/$2" 2>/dev/null)" ]; }
inventory() {
  C=$(cwds); now=$(date +%s)
  for d in "$root"/*/*; do
    [ -d "$d" ] && [ ! -L "$d" ] && [ "$d" != "$1" ] && ours "$d" || continue
    t=$(stat -c %Z "$d" 2>/dev/null) || continue
    k=0; b=0
    if [ -e "$d.lock" ]; then
      m=$(stat -c %Y "$d.lock" 2>/dev/null) && [ "$m" -gt "$t" ] && t=$m
      flock -n "$d.lock" true || k=1
    fi
    printf '%s\n' "$C" | grep -qF "$d/" && b=1
    printf 'D\t%s\t%s\t%s\t%s\n' "$d" "$k" "$b" "$((now - t))"
  done
  df -Pk "$root" | awk 'NR == 2 { printf "F\t%s\t%s\n", $4, $2 }'
}
gc_rm() {
  [ "$1" != "$2" ] && ours "$1" && [ -d "$1" ] && [ ! -L "$1" ] || return 0
  { flock -n 9 && same "$1.lock" 9 && ! cwds | grep -qF "$1/" && rm -rf "$1" && rm -f "$1.owner" "$1".pgid-* "$1.lock" "$1.source-manifest" "$1".source-manifest.tmp-*; } 9>>"$1.lock"
  rmdir "${1%/*}" 2>/dev/null
  return 0
}
reap() {
  f="$1.pgid-$token"
  [ -s "$f" ] || return 0
  g=$(cat "$f"); rm -f "$f"
  case "$g" in ''|*[!0-9]*) return 0 ;; esac
  [ "$g" -gt 1 ] && [ "$g" != "$me" ] || return 0
  kill -TERM -"$g" 2>/dev/null || return 0
  i=0
  while kill -0 -"$g" 2>/dev/null; do
    i=$((i + 1)); [ $i -ge 50 ] && { kill -KILL -"$g" 2>/dev/null; break; }; sleep 0.1
  done
  i=0
  while kill -0 -"$g" 2>/dev/null && [ $i -lt 50 ]; do i=$((i + 1)); sleep 0.1; done
}
# 離開了 process group 的殘留（自己 `setsid` 的測試輔助行程之類，`reap` 追不到）：還把 cwd 放在這個目錄裡的一併收掉（issue #201）。
# 這個目錄是這次租約獨佔的（flock），裡面的行程不是這次的就是上一代留下的孤兒；自己（cwd 在 $root）不會被打到。
sweep() {
  for p in /proc/[0-9]*; do
    c=$(readlink "$p/cwd" 2>/dev/null) || continue
    case "$c/" in "$1"/*) kill -KILL "${p#/proc/}" 2>/dev/null ;; esac
  done
}
# 排隊中每秒問一次本機那端還在不在（順便當作這一秒的等待）；不在了就不再佔著 shared 的鎖空等。
# helper 被砍、Ctrl-C、網路斷時，遠端 sshd 會關掉守門的 stdin，stdout 卻照樣寫得進去（2026-09-19 實測）——所以看 stdin：
# 讀到 EOF＝斷了。helper 交握之前不寫 stdin，這裡讀不到東西、也不會吃掉資料：讀了一秒還在等＝還活著。
# 另外對 stdout 打一行心跳（`Q`），寫不出去也算斷了。沒有 `timeout` 的機器只剩心跳這條，等待改用 sleep。
has_timeout=$(command -v timeout)
alive() {
  printf 'Q\n' 2>/dev/null || return 1
  if [ -n "$has_timeout" ]; then
    timeout 1 dd bs=1 count=1 >/dev/null 2>&1
    [ $? -eq 124 ]
  else
    sleep 1
  fi
}
# 目錄的鎖（fd 8）：拿不到就每秒再試，最多等 $1 秒（0＝不等），還是拿不到回 75。
lock_dir() {
  flock -n 8 && return 0
  [ "$1" -gt 0 ] || return 75
  echo "agents-manager: 這棵 worktree 的共用遠端 target 正被另一次呼叫使用，等它用完（最多 $1 秒；增量編譯比冷編譯划算）…" >&2
  h0=$(date +%s)
  while :; do
    alive || return 73
    if flock -n 8; then
      hw=$(($(date +%s) - h0)); [ "$hw" -eq 0 ] || echo "agents-manager: 等了 $hw 秒拿到共用遠端 target" >&2
      return 0
    fi
    [ $(($(date +%s) - h0)) -lt "$1" ] || return 75
  done
}
# 遠端名額（issue #104）：`$root/.slots/<n>` 的 flock，fd 7，拿著直到守門結束（怎麼死都會放）。
# 同一個 remote_root 不分 worktree 最多 $max 個；全滿就每秒再試，排超過 $qwait 秒回 74。
slot=0
take_slot() {
  # 試用輪已經先拿到名額（才播種）：hold 不再拿第二次。
  [ "$slot" -eq 0 ] || return 0
  mkdir -p "$root/.slots" || return 2
  q0=$(date +%s); said=
  while :; do
    i=1
    while [ "$i" -le "$max" ]; do
      exec 7>>"$root/.slots/$i"
      if flock -n 7; then
        slot=$i
        qw=${q0:+$(($(date +%s) - q0))}
        qw=${qw:-0}
        [ -z "$said" ] || echo "agents-manager: 排到遠端名額 $i/$max（等了 $(($(date +%s) - q0)) 秒）" >&2
        return 0
      fi
      exec 7>&-
      i=$((i + 1))
    done
    # `-le` 不是 `-lt`（issue #486）：`date +%s` truncate 到整秒，`now - q0` 會比真實經過的時間**多算最多 1 秒**
    # （`q0` 落在某一秒的最後 10ms 時，真的過了 1.01 秒就已經算成 2）。用 `-lt` 的話 `qwait=2` 實際只等了約 1 秒。
    # 改成等到「算出來的秒數**超過** qwait」才放棄：truncate 的誤差只會讓它多等，不會少等（最多多等 1 秒）。
    [ "$try" = 0 ] || return 74
    [ $(($(date +%s) - q0)) -le "$qwait" ] || return 74
    [ -n "$said" ] || { echo "agents-manager: 遠端同時編譯已滿（$max 個，[build.remote] max_concurrent），排隊等名額（最多 $qwait 秒）…" >&2; said=1; }
    alive || return 73
  done
}
# 目錄先、名額後：等 shared 的人不佔名額；拿著名額的人不會再等任何鎖（順序一致，不會互等）。
hold() {
  lock_dir "$2" || return $?
  same "$1.lock" 8 || return 76
  take_slot || return $?
  # 不再排隊了：撤掉排隊號碼牌（`.queue/<token>`，挑主機時數人頭用）。
  exec 6>&-; rm -f "$qf"
  mkdir -p "$1" && touch "$1.lock" && printf '%s\n' "$token" > "$1.owner" || return 2
  printf 'ROOT\t%s\nDIR\t%s\t%s\nSLOT\t%s\t%s\n' "$root" "$1" "$token" "$slot" "$max"
  inventory "$1"
  case "${1##*/}" in shared|shared-*) out=shared ;; *) out=isolated ;; esac
  printf 'OUTCOME\t%s\t%s\t%s\n' "$out" "${hw:-}" "${qw:-}"
  echo END
  # 本機隨時可能已經斷線：之後再寫 stdout/stderr 會 EPIPE，清理不能因此半途而廢。
  exec >/dev/null 2>&1
  while read -r op p; do [ "$op" = rm ] && gc_rm "$p" "$1"; done
  # 先撤 owner 再收 pgid：晚到的 run 若通過了 owner 檢查，它的 pgid 一定已經寫好了。
  rm -f "$1.owner"
  reap "$1"
  sweep "$1"
  rm -f "$1".pgid-*
  case "${1##*/}" in shared|shared-*) keep=1 ;; *) keep= ;; esac
  if [ -n "$keep" ]; then touch "$1.lock"; else rm -rf "$1"; rm -f "$1.lock" "$1.source-manifest" "$1".source-manifest.tmp-*; fi
  return 0
}
queue_full() {
  # 印**量到的**秒數，不是設定值（issue #486）：log 自報「等了 2 秒」而外面量到 1.04 秒，
  # 比慢一秒難查得多。`q0` 還沒設過（沒排隊就走到這裡）時退回設定值。
  waited=${q0:+$(($(date +%s) - q0))}
  printf 'OUTCOME\tqueue_full\t%s\t%s\n' "${hw:-}" "${waited:-}"
  echo "agents-manager: 等遠端名額等了 ${waited:-$qwait} 秒還是全滿（$max 個），這次改在本機排隊" >&2
  exit 75
}
# 共用 target 池（#1956）：`shared`、`shared-2` … `shared-$max`，一份一把鎖、各自增量；池的大小＝名額數，
# 所以「池全忙」就是「名額全滿」，在那之前不會有人等。
member() { if [ "$1" -le 1 ]; then printf '%s/%s/shared' "$root" "$hash"; else printf '%s/%s/shared-%s' "$root" "$hash" "$1"; fi; }
try_pool() {
  i=1
  while [ "$i" -le "$pool" ]; do
    d=$(member "$i")
    exec 8>>"$d.lock"
    flock -n 8 && return 0
    exec 8>&-
    i=$((i + 1))
  done
  return 1
}
# 拿池裡一份的鎖（fd 8，拿到的那份在 $d）：都忙就每秒輪詢**每一份**，任何一份空出來就拿；最多等 $1 秒，還是沒有回 75。
lock_pool() {
  if try_pool; then hw=${hw:-0}; return 0; fi
  [ "$1" -gt 0 ] || return 75
  echo "agents-manager: 這個 repo 的共用遠端 target（$pool 份）都在用，等任何一份空出來（最多 $1 秒；增量編譯比冷編譯划算）…" >&2
  h0=$(date +%s)
  while :; do
    alive || return 73
    if try_pool; then
      this_hw=$(($(date +%s) - h0))
      hw=$((${hw:-0} + this_hw))
      [ "$this_hw" -eq 0 ] || echo "agents-manager: 等了 $this_hw 秒拿到共用遠端 target（${d##*/}）" >&2
      return 0
    fi
    if [ $(($(date +%s) - h0)) -ge "$1" ]; then
      this_hw=$(($(date +%s) - h0))
      hw=$((${hw:-0} + this_hw))
      return 75
    fi
  done
}
# 第一次用到池裡的某一份（目錄還不存在）：從另一份複製，不冷編。先挑沒人持鎖的（一致的快照，拷的時候拿著它的鎖），
# 都在用才從最近用過的那份拷。各 profile 的 `.fingerprint/` 先拷、其餘後拷且不覆蓋：拷到的指紋不會比產物新
# （舊指紋＝多重編一次，不會把舊產物誤判成新鮮）。拷進 `<份>.seed` 再 rename，半途死掉不會留下一份殘缺的。
seed() {
  [ -e "$1" ] && return 0
  src=; busy_src=; best=-1; i=1
  while [ "$i" -le "$pool" ]; do
    c=$(member "$i"); i=$((i + 1))
    [ "$c" != "$1" ] && [ -d "$c/target" ] && [ ! -L "$c" ] || continue
    exec 5>>"$c.lock"
    if flock -n 5; then src=$c; break; fi
    exec 5>&-
    m=$(stat -c %Y "$c.lock" 2>/dev/null) || m=0
    [ "$m" -gt "$best" ] && { best=$m; busy_src=$c; }
  done
  [ -n "$src" ] || src=$busy_src
  [ -n "$src" ] || return 0
  echo "agents-manager: 第一次用 ${1##*/}：從 ${src##*/} 複製共用 target 再增量編（不冷編）…" >&2
  t0=$(date +%s); tmp="$1.seed"
  rm -rf "$tmp"
  ok=1
  mkdir -p "$tmp/target" || ok=
  for fp in "$src"/target/*/.fingerprint "$src"/target/*/*/.fingerprint; do
    [ -n "$ok" ] && [ -d "$fp" ] || continue
    rel=${fp#"$src"/}; mkdir -p "$tmp/${rel%/*}" && cp -a --reflink=auto "$fp" "$tmp/${rel%/*}/" || ok=
  done
  # 不覆蓋已經拷好的指紋；舊版 coreutils（< 9.3）沒有 `--update=none` 就改用 rsync 補完。
  if [ -n "$ok" ] && { cp -a --reflink=auto --update=none "$src/." "$tmp/" 2>/dev/null || rsync -a --ignore-existing "$src/" "$tmp/"; } && mv "$tmp" "$1"; then
    [ -f "$src.source-manifest" ] && cp -p "$src.source-manifest" "$1.source-manifest"
    echo "agents-manager: 複製完成（$(($(date +%s) - t0)) 秒）" >&2
  else
    rm -rf "$tmp"
    echo "agents-manager: 從 ${src##*/} 複製失敗，${1##*/} 這次冷編" >&2
  fi
  exec 5>&-
  return 0
}
# 排隊號碼牌（#1956）：等鎖期間 flock 住 `.queue/<token>`（fd 6），拿到名額就撤；守門怎麼死鎖都會放，
# 數人頭時鎖沒被持有的殘檔順手刪。只是挑主機用的估計，不影響誰拿到鎖。
mkdir -p "$root/.queue" || exit 2
qf="$root/.queue/$token"
trap 'rm -f "$qf"' EXIT
# 試用（多台挑主機的第一輪）：池裡沒有空的份、或名額全滿就不等、不改走冷目錄；拿不到就回 `BUSY\t<排隊人數>`、exit 77。
busy() {
  q=0
  for f in "$root"/.queue/*; do
    [ -f "$f" ] || continue
    if flock -n "$f" true 2>/dev/null; then rm -f "$f"; else q=$((q + 1)); fi
  done
  printf 'BUSY\t%s\n' "$q"
  exit 77
}
if [ "$try" = 1 ]; then
  if [ "$maxload" != 0 ] && awk -v m="$maxload" '{ exit !($1 > m) }' @LOADAVG@ 2>/dev/null; then
    echo "agents-manager: 這台 1 分鐘負載超過 $maxload，當作沒有空槽" >&2; busy
  fi
  mkdir -p "$root/$hash" || exit 2
  lock_pool 0 || busy
  # 先確定有名額再播種：沒名額就回忙，不要先拷完一整份 target 才發現（拿著的名額在 hold 裡直接用）。
  take_slot; rc=$?
  case $rc in 0) ;; 74) busy ;; *) exit $rc ;; esac
  seed "$d"
  hold "$d" 0; rc=$?
  # 76＝鎖檔在 open 與 flock 之間被回收：正常守門會重試，試用就當這台忙、讓 helper 換下一台。
  case $rc in
    0|73) exit 0 ;;
    74|75|76) busy ;;
  esac
  exit $rc
fi
exec 6>>"$qf" && flock -n 6
# 73＝排隊中發現本機已經斷線：沒人要結果了，直接結束（鎖跟著放）。
n=0; s0=$(date +%s)
while :; do
  mkdir -p "$root/$hash" || exit 2
  w=$((swait - ($(date +%s) - s0))); [ "$w" -gt 0 ] || w=0
  lock_pool "$w"; rc=$?
  case $rc in
    0) ;;
    73) exit 0 ;;
    75) break ;;
    *) exit $rc ;;
  esac
  seed "$d"
  hold "$d" 0; rc=$?
  exec 8>&-
  case $rc in
    0|73) exit 0 ;;
    74) queue_full ;;
  esac
  n=$((n + 1)); [ $n -lt 5 ] || exit $rc
done
[ "$swait" -eq 0 ] || echo "agents-manager: 等了 $swait 秒共用遠端 target 還沒空出來（$pool 份都在用）" >&2
echo 'agents-manager: 這棵 worktree 的共用遠端 target 正被另一次呼叫使用，這次改用獨立目錄（冷編譯，結束就刪）' >&2
hold "$root/$hash/$job" 0 8>>"$root/$hash/$job.lock"; rc=$?
# 重導向先把 $job.lock 建出來，只有 hold 成功的尾端會刪它；73／74／75 路徑不刪就永遠留著、沒人 GC（#326）。
rm -f "$root/$hash/$job.lock"
case $rc in
  73) exit 0 ;;
  74) queue_full ;;
  75) exit 2 ;;
esac
exit $rc
"#;

#[derive(Debug, Clone, PartialEq)]
pub struct RemoteDir {
    pub path: String,
    pub locked: bool,
    pub busy: bool,
    pub idle_secs: u64,
}

/// 守門連線交回來的：這次用哪個目錄、原樣回的 token，以及順手看到的回收候選。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Handshake {
    pub root: String,
    pub dir: String,
    pub token: String,
    pub dirs: Vec<RemoteDir>,
    /// (剩餘 KB, 總 KB)
    pub disk: Option<(u64, u64)>,
    /// (這次拿到第幾個遠端名額, 名額總數)
    pub slot: Option<(usize, usize)>,
}

/// 守門過程收集到的終態等待觀測（issue #1960）。
#[derive(Debug, Clone, Default, PartialEq)]
struct WaitObservation {
    pub outcome: Option<String>,
    pub shared_wait_secs: Option<u64>,
    pub slot_wait_secs: Option<u64>,
}

/// 內部觀察版：在解析 Handshake 時額外記錄 OUTCOME 結構化行，即使後續 EOF 失敗也能保留觀測數據。
fn read_handshake_observed(r: &mut impl BufRead) -> (anyhow::Result<Handshake>, WaitObservation) {
    let mut hs = Handshake::default();
    let mut obs = WaitObservation::default();
    let mut line = String::new();
    loop {
        line.clear();
        match r.read_line(&mut line) {
            Ok(0) => {
                return (
                    Err(anyhow::anyhow!("remote workdir guard exited before handing out a directory")),
                    obs,
                );
            }
            Err(e) => return (Err(e.into()), obs),
            Ok(_) => {}
        }
        let f: Vec<&str> = line.trim_end_matches(['\r', '\n']).split('\t').collect();
        match f.as_slice() {
            ["END"] if !hs.dir.is_empty() => return (Ok(hs), obs),
            ["ROOT", root] => hs.root = root.to_string(),
            ["DIR", dir, token] => {
                hs.dir = dir.to_string();
                hs.token = token.to_string();
            }
            ["D", path, k, b, idle] => hs.dirs.push(RemoteDir {
                path: path.to_string(),
                locked: *k == "1",
                busy: *b == "1",
                idle_secs: idle.parse::<i64>().unwrap_or(0).max(0) as u64,
            }),
            ["F", free, total] => hs.disk = free.parse().ok().zip(total.parse().ok()),
            ["SLOT", i, max] => hs.slot = i.parse().ok().zip(max.parse().ok()),
            ["OUTCOME", out, sw, qw] => {
                obs.outcome = Some((*out).to_string());
                obs.shared_wait_secs = sw.parse().ok();
                obs.slot_wait_secs = qw.parse().ok();
            }
            ["BUSY", n] => return (Err(anyhow::Error::new(Busy { waiters: n.parse().unwrap_or(0) })), obs),
            _ => {}
        }
    }
}

/// 讀到 `END` 為止。登入 shell 的雜訊行略過；還沒拿到 `DIR` 就 EOF＝守門失敗（原因在它的 stderr）。
pub fn read_handshake(r: &mut impl BufRead) -> anyhow::Result<Handshake> {
    read_handshake_observed(r).0
}

/// 共用 target 池的一份（#1956）：`shared` 或 `shared-<N>`（同守門的 `ours`）。
fn is_shared_leaf(leaf: &str) -> bool {
    leaf == "shared" || leaf.strip_prefix("shared-").is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum LeafKind {
    Shared,
    Job,
}

/// `<root>/<16 hex>/<shared｜job-<pid>-<ms>｜<pid>>` 才是自己的；其他一律不碰（同守門的 `ours`）。
fn leaf_kind(root: &str, path: &str) -> Option<LeafKind> {
    let rel = path.strip_prefix(root)?.strip_prefix('/')?;
    let (hash, leaf) = rel.split_once('/')?;
    if hash.len() != 16 || !hash.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    if is_shared_leaf(leaf) {
        return Some(LeafKind::Shared);
    }
    let digits = leaf.strip_prefix("job-").unwrap_or(leaf);
    let ok = !digits.is_empty()
        && digits.bytes().all(|c| c.is_ascii_digit() || (c == b'-' && leaf.starts_with("job-")));
    ok.then_some(LeafKind::Job)
}

/// 回收政策（issue #196）。`shared/` 是快取不是垃圾，但不能只靠時間：一天開十幾顆子 agent、每張票數個變異副本，每個路徑一個新的 hash、各 2～3G，
/// 一天內就塞滿（實測 36 個 hash、29G）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GcPolicy {
    /// 閒置這麼久的 `shared/` 就收（磁碟緊的時候降到 [`ORPHAN_IDLE_SECS`]）。
    pub shared_idle_secs: u64,
    /// `shared/` 最多留幾份（連同正在用的、這次自己的）；超過就從最久沒用的開始收（至少閒置 [`ORPHAN_IDLE_SECS`]，剛用完的不動）。`0`＝不限。
    pub max_shared: usize,
}

impl Default for GcPolicy {
    fn default() -> Self {
        GcPolicy { shared_idle_secs: SHARED_IDLE_SECS, max_shared: crate::config::BuildRemoteCfg::default().max_shared_dirs }
    }
}

impl GcPolicy {
    pub fn from_cfg(remote: &BuildRemoteCfg) -> Self {
        GcPolicy { shared_idle_secs: remote.shared_idle_hours.max(1) * 3600, max_shared: remote.max_shared_dirs }
    }
}

/// 這次要請守門回收哪些目錄。有鎖被持有、有行程 cwd 在裡面、或就是自己這次的目錄，一律不動；
/// 其餘照種類看閒置多久，`shared/` 另外受 [`GcPolicy::max_shared`] 的數量上限管（LRU：最久沒用的先收）。
/// 守門刪之前會自己再拿鎖、再看一次行程，這裡是第一道篩選。
pub fn select_gc(hs: &Handshake, policy: &GcPolicy) -> Vec<String> {
    let low_disk = matches!(hs.disk, Some((free, total)) if total > 0 && free * 100 < total * LOW_DISK_FREE_PCT);
    let is_shared = |d: &&RemoteDir| leaf_kind(&hs.root, &d.path) == Some(LeafKind::Shared);
    let mut picked: Vec<String> = hs
        .dirs
        .iter()
        .filter(|d| !d.locked && !d.busy && d.path != hs.dir)
        .filter(|d| match leaf_kind(&hs.root, &d.path) {
            Some(LeafKind::Shared) if !low_disk => d.idle_secs >= policy.shared_idle_secs,
            Some(_) => d.idle_secs >= ORPHAN_IDLE_SECS,
            None => false,
        })
        .map(|d| d.path.clone())
        .collect();
    if policy.max_shared > 0 {
        // 盤點不含這次自己的目錄，所以自己的那份另外算。
        let own = hs.dir.rsplit_once('/').is_some_and(|(_, leaf)| is_shared_leaf(leaf));
        let total = hs.dirs.iter().filter(is_shared).count() + usize::from(own);
        let gone = hs.dirs.iter().filter(is_shared).filter(|d| picked.contains(&d.path)).count();
        let kept = total.saturating_sub(gone);
        if kept > policy.max_shared {
            let mut extra: Vec<&RemoteDir> = hs
                .dirs
                .iter()
                .filter(is_shared)
                .filter(|d| !d.locked && !d.busy && d.path != hs.dir && d.idle_secs >= ORPHAN_IDLE_SECS && !picked.contains(&d.path))
                .collect();
            extra.sort_by(|a, b| b.idle_secs.cmp(&a.idle_secs));
            picked.extend(extra.into_iter().take(kept - policy.max_shared).map(|d| d.path.clone()));
        }
    }
    picked
}

/// 守門連線。活著＝遠端目錄是這次呼叫的；關掉 stdin（[`Lease::finish`]、drop、或這個行程死掉）＝還回去。
struct Lease {
    child: Child,
    stdin: Option<ChildStdin>,
    /// 留著不關：守門寫 stdout 時讀端要還在，免得它被 SIGPIPE 打斷。
    stdout: Option<BufReader<ChildStdout>>,
    hs: Handshake,
    /// 這次租約的身份（本機產生、守門原樣回；[`run_script`] 用它核對 `.owner`）。
    token: LeaseToken,
    obs: WaitObservation,
}

/// 排遠端名額排超過上限，守門沒交目錄就結束了（exit 75）。攜帶結構化觀測數據與主機標籤（issue #1960）。
#[derive(Debug, Clone, PartialEq)]
struct QueueFull {
    pub host: String,
    pub shared_wait_secs: Option<u64>,
    pub slot_wait_secs: Option<u64>,
}

impl std::fmt::Display for QueueFull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("remote Cargo queue wait exceeded its limit")
    }
}

impl std::error::Error for QueueFull {}

/// ssh 根本沒進去：連不上、認證被拒、host key 不合、或本機沒有 `ssh`（issue #428）。
/// 遠端這時**什麼都還沒跑**，所以跟 `125` 那條一樣退回本機，而不是讓呼叫端的編譯整個失敗。
#[derive(Debug)]
struct Unreachable {
    /// 一句話說明「ssh 那邊發生什麼事」，會出現在 stderr 與 `remote_reachable` 的 `reason`。
    why: String,
}

impl std::fmt::Display for Unreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.why)
    }
}

impl std::error::Error for Unreachable {}

impl Lease {
    /// `cmd` 跑的守門腳本要是用 `token` 產生的；它交回來的 token 對不上就當守門出事、不當成拿到目錄。
    ///
    /// 名額全滿或 shared 被佔著時，守門要排隊，可能很久才交出目錄（issue #104）：交握放到另一條執行緒讀，
    /// 這裡每 20ms 看一次 `ctl`，收到終止訊號就砍掉本機這條 ssh（排隊中的守門靠心跳發現斷線，自己結束、放鎖）。
    fn start(mut cmd: Command, token: LeaseToken, ctl: &Deadline) -> anyhow::Result<Self> {
        // stderr 原樣轉出去（守門與 ssh 自己講的話照舊看得到），順手留最後一行：連不進去時，
        // 那一行就是 ssh 的原話（`Permission denied …`／`Connection refused`），要放進我們的說明裡（issue #428）。
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        // 叫不起 ssh（這台沒裝、PATH 裡沒有）也是「遠端什麼都還沒跑」：當連不上處理，退回本機（issue #428）。
        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::Error::new(Unreachable { why: format!("叫不起 {}：{e}", ssh_program()) }))?;
        let said = Arc::new(Mutex::new(String::new()));
        // 這條在 stderr 讀完（管線 EOF）時才會斷；失敗路徑靠它等那一行真的進到 `said` 再讀（issue #467）。
        // 不送內容、只當完成訊號：成功路徑沒有人收，送內容會讓長時間的編譯把 channel 撐大。
        let (relay_done, relay_rx) = std::sync::mpsc::channel::<()>();
        match child.stderr.take() {
            Some(err) => {
                let said = said.clone();
                std::thread::spawn(move || {
                    for line in BufReader::new(err).lines().map_while(Result::ok) {
                        eprintln!("{line}");
                        if !line.trim().is_empty() {
                            *said.lock().unwrap_or_else(|e| e.into_inner()) = line;
                        }
                    }
                    drop(relay_done);
                });
            }
            // 沒有 stderr 可讀就當場斷訊號，否則失敗路徑會白等滿上限。
            None => drop(relay_done),
        }
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or_else(|| anyhow::anyhow!("remote workdir guard: no stdout"))?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let (hs, obs) = read_handshake_observed(&mut reader);
            let _ = tx.send((reader, hs, obs));
        });
        let (reader, hs, obs) = loop {
            match rx.recv_timeout(std::time::Duration::from_millis(20)) {
                Ok(got) => break got,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(why) = ctl.stop() {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(anyhow::Error::new(why));
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => anyhow::bail!("remote workdir guard: handshake reader died"),
            }
        };
        let mut lease = Lease { child, stdin, stdout: Some(reader), hs: Handshake::default(), token, obs: obs.clone() };
        lease.hs = match hs {
            Ok(hs) => hs,
            Err(e) if e.downcast_ref::<Busy>().is_some() => {
                lease.finish();
                return Err(e);
            }
            Err(e) => {
                // 沒交目錄就結束了：75＝排隊超過上限（原因守門已經印在 stderr），
                // 255＝ssh 自己沒進去（連不上／認證被拒／host key 不合，issue #428），其他是守門出事。
                match lease.finish().and_then(|st| st.code()) {
                    Some(EXIT_QUEUE_FULL) => return Err(anyhow::Error::new(QueueFull {
                        host: String::new(), // 由 open_lease 或 caller 補充
                        shared_wait_secs: obs.shared_wait_secs,
                        slot_wait_secs: obs.slot_wait_secs,
                    })),
                    Some(SSH_FAILED) => {
                        // 等轉發那條讀完再看（issue #467）：`finish()` 只保證 ssh 這個行程收掉了，**不保證**
                        // 轉發執行緒已經把最後一行寫進 `said`。macOS 上剛好都來得及，Linux 上量到的是空字串，
                        // 於是 `reason` 變成「什麼都沒說」而畫面上明明印著 `Permission denied`（#467 的紅）。
                        // 行程已經收掉＝管線一定會 EOF，所以這裡等的是「馬上就會發生的事」；仍然給上限，
                        // 不讓它變成 helper 卡住的新路（等不到就用手上有的，頂多少一句原話）。
                        // 只送完成訊號（`Err(Disconnected)`＝讀完了），所以這裡等的是「斷線」而不是「收到值」。
                        let _ = relay_rx.recv_timeout(std::time::Duration::from_secs(5));
                        let said = said.lock().unwrap_or_else(|e| e.into_inner()).clone();
                        let said = said.trim();
                        return Err(anyhow::Error::new(Unreachable {
                            why: if said.is_empty() {
                                format!("ssh 回 {SSH_FAILED}，什麼都沒說")
                            } else {
                                format!("ssh 回 {SSH_FAILED}：{said}")
                            },
                        }));
                    }
                    _ => return Err(e),
                }
            }
        };
        if lease.hs.token != lease.token.as_str() {
            anyhow::bail!("remote workdir guard answered with a lease token that is not the one it was given");
        }
        Ok(lease)
    }

    /// 請守門回收這些孤兒；寫不進去（守門已經死了）就算了，下次呼叫還會再看到。
    fn collect(&mut self, paths: &[String]) {
        if let Some(w) = self.stdin.as_mut() {
            for p in paths {
                if writeln!(w, "rm {p}").is_err() {
                    break;
                }
            }
            let _ = w.flush();
        }
    }

    /// 還回目錄並等遠端清完（`job-*` 刪掉、`shared` 留著、還在跑的遠端 cargo 整組收掉）。
    fn finish(&mut self) -> Option<ExitStatus> {
        drop(self.stdin.take());
        let status = self.child.wait().ok();
        self.stdout.take();
        status
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if self.stdin.is_some() {
            self.finish();
        }
    }
}

/// 遠端跑 cargo 的那一行。先記下自己的 process group、再確認目錄還是這次租的（owner＝token），順序不能反：
/// 守門清理是「先撤 owner、再讀 pgid」，所以通過檢查的 run 一定會被收到。
///
/// `test_threads > 0` 就帶 `RUST_TEST_THREADS`（issue #202）：libtest 的命令列 `--test-threads` 優先於環境變數，所以呼叫端自己帶了旗標照樣算數。
///
/// `sub` 是本機 cwd 相對於同步過去的根的路徑（[`source_root`]；`""`＝就在根）：遠端也要在同一個子目錄下跑，
/// 相對路徑的參數（`--manifest-path ../x`、`-p` 的路徑）才跟本機是同一個意思。
fn run_script(dir: &str, sub: &str, token: &LeaseToken, jobs: usize, test_threads: usize, args: &[String]) -> String {
    let argv = args.iter().map(|a| sh_quote(a)).collect::<Vec<_>>().join(" ");
    let tok = token.as_str();
    let cache_path = format!("{dir}/target");
    // `~/.cargo/bin` 要自己接：rustup 只改 shell profile，ssh 的非互動 shell 不讀（同 `PROBE_SH`）。
    // Keep Cargo's cache under the inspected target without exporting cache overrides into
    // tests/build scripts that spawn another Cargo for an unrelated temporary project.
    format!(
        "d={dir}; g=$(ps -o pgid= -p $$ | tr -d ' ') && printf '%s\\n' \"$g\" > \"$d.pgid-{tok}\" \
         && [ \"$(cat \"$d.owner\" 2>/dev/null)\" = {tok} ] \
         || {{ echo 'agents-manager: 遠端工作目錄已經還回去了（helper 中途被砍？），不跑 cargo' >&2; exit 126; }}; \
         cd \"$d\"/{sub} && PATH=\"$HOME/.cargo/bin:$PATH\" CARGO_BUILD_JOBS={jobs}{threads} env -u CARGO_TARGET_DIR -u CARGO_BUILD_TARGET_DIR -u CARGO_BUILD_BUILD_DIR cargo --config {target_config} --config {build_config} {argv}",
        dir = sh_quote(dir),
        target_config = sh_quote(&format!("build.target-dir={cache_path:?}")),
        build_config = sh_quote(&format!("build.build-dir={cache_path:?}")),
        sub = sh_quote(sub),
        jobs = jobs.max(1),
        threads = if test_threads > 0 { format!(" RUST_TEST_THREADS={test_threads}") } else { String::new() },
    )
}

/// 收到的第一個終止訊號（0＝沒有）；[`install_signal_handlers`] 的 handler 寫它，[`Deadline`] 讀它。
static SIGNALLED: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

extern "C" fn on_signal(sig: libc::c_int) {
    // 第二次訊號＝不想再等了：直接結束。遠端那頭照樣清得掉——守門靠連線中斷（stdin EOF）偵測，不靠我們活著做完。
    if SIGNALLED.swap(sig, std::sync::atomic::Ordering::SeqCst) != 0 {
        unsafe { libc::_exit(128 + sig) };
    }
}

/// helper 正式入口才裝（[`run_cli`]）：SIGINT（Ctrl-C）、SIGTERM、SIGHUP 不再直接把 helper 打死、留下還連著的 ssh 與遠端的 cargo，
/// 而是記下來，[`run_offload`] 把本機的 rsync／ssh 收掉、等守門把遠端那組行程收乾淨才結束（issue #201）。
/// 只裝 handler、只做 atomic 操作（async-signal-safe）。
fn install_signal_handlers() {
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        unsafe { libc::signal(sig, on_signal as extern "C" fn(libc::c_int) as usize as libc::sighandler_t) };
    }
}

/// 這次遠端編譯該不該停下來：整體時間上限（issue #194），或本機收到終止訊號（issue #201）。
/// 連線正常、遠端的 cargo 或測試卡住（死結、等鎖、測試掛住）時，ssh 的 ConnectTimeout／ServerAlive（#174）管不到，沒有上限 helper 與
/// 呼叫端的 agent 會一直等。
struct Deadline {
    limit: Option<std::time::Duration>,
    /// [`Deadline::arm`] 之後才有：排隊等遠端名額的時間不算（issue #104）。
    at: std::cell::Cell<Option<std::time::Instant>>,
    /// 測試用：模擬「收到終止訊號」。
    abort: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// 只有正式的 helper 看真的訊號；測試在同一個行程裡並行跑，不能共用那個全域。
    watch_signals: bool,
    /// 啟動時的父行程（cargo shim）與讀「現在的父行程」的函式（測試可換）：shim 被單獨殺掉（`timeout 60 cargo test` 只殺 shim、
    /// SIGKILL）時 helper 變孤兒，沒有人會再送訊號給它，遠端編譯會一路跑到 timeout_secs（#323）。父行程變了就當成被終止。
    parent: Option<(i32, fn() -> i32)>,
}

/// 為什麼停：當成 `anyhow` 錯誤一路往上傳，[`run_offload`] 認得它。
#[derive(Debug)]
enum Stopped {
    /// 超過整體上限：回 [`EXIT_TIMEOUT`]。
    TimedOut,
    /// 收到訊號（值）：回 `128 + 訊號`，跟被那個訊號打死的行程一樣。
    Interrupted(i32),
}

impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Stopped::TimedOut => f.write_str("remote Cargo exceeded its overall time limit"),
            Stopped::Interrupted(sig) => write!(f, "remote Cargo interrupted by signal {sig}"),
        }
    }
}

impl std::error::Error for Stopped {}

impl Deadline {
    /// `limit_secs == 0`＝不設上限。時間從 [`Deadline::arm`] 起算，在那之前只看訊號。
    fn new(limit_secs: u64) -> Self {
        Deadline {
            // 手改 config 的 u64::MAX 不能讓 `arm()` 的 Instant 加法溢位 panic（#329）；API 端本來就擋在 MAX_TIMEOUT_SECS。
            limit: (limit_secs > 0).then(|| std::time::Duration::from_secs(limit_secs.min(MAX_TIMEOUT_SECS))),
            at: std::cell::Cell::new(None),
            abort: Default::default(),
            watch_signals: false,
            parent: None,
        }
    }

    /// 從現在開始算整體上限：拿到遠端名額與目錄的那一刻。滿載時排了幾分鐘隊的編譯，不能一開始跑就被砍。
    fn arm(&self) {
        self.at.set(self.limit.map(|l| std::time::Instant::now() + l));
    }

    /// 正式 helper：也看真的終止訊號。
    fn watching_signals(mut self) -> Self {
        self.watch_signals = true;
        self.parent = Some((unsafe { libc::getppid() }, || unsafe { libc::getppid() }));
        self
    }

    fn stop(&self) -> Option<Stopped> {
        if self.abort.load(std::sync::atomic::Ordering::SeqCst) {
            return Some(Stopped::Interrupted(libc::SIGTERM));
        }
        if let Some((orig, now)) = self.parent {
            if now() != orig {
                return Some(Stopped::Interrupted(libc::SIGTERM));
            }
        }
        if self.watch_signals {
            let sig = SIGNALLED.load(std::sync::atomic::Ordering::SeqCst);
            if sig != 0 {
                return Some(Stopped::Interrupted(sig));
            }
        }
        self.at.get().filter(|at| std::time::Instant::now() >= *at).map(|_| Stopped::TimedOut)
    }
}

/// 跑一個子行程到結束；該停了（超過上限、收到訊號）就砍掉它並回 [`Stopped`]。輪詢（20ms）而不是另開看門狗執行緒：`Child::kill` 只會砍還沒被
/// 收掉的行程，不會碰到 pid 被回收重用的別人。被砍的只是**本機**這條 ssh／rsync；遠端那組行程由守門在 [`Lease`] 放掉時收（stdin EOF）。
fn run_status(mut cmd: Command, what: &str, deadline: &Deadline) -> anyhow::Result<ExitStatus> {
    cmd.stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
    let child = cmd.spawn().map_err(|e| anyhow::anyhow!("{what}: {e}"))?;
    wait_child(child, what, deadline)
}

/// 同 [`run_status`]，但 stderr 一邊照印一邊留一份回傳（rsync 要看它說了什麼才知道能不能重試）。
fn run_status_tee_stderr(mut cmd: Command, what: &str, deadline: &Deadline) -> anyhow::Result<(ExitStatus, String)> {
    cmd.stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| anyhow::anyhow!("{what}: {e}"))?;
    let err = child.stderr.take().expect("stderr piped");
    // 讀 stderr 的執行緒把已讀到的收在共用字串裡：子行程結束後孫行程（ssh 背景 master、ProxyCommand）可能還握著 pipe，
    // 不能無限等 EOF，等一小段就拿手上已有的。
    let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let sink = seen.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(err).lines().map_while(Result::ok) {
            eprintln!("{line}");
            let mut g = sink.lock().unwrap();
            g.push_str(&line);
            g.push('\n');
        }
        let _ = done_tx.send(());
    });
    let status = wait_child(child, what, deadline);
    // 被砍時 rsync 的 ssh 孫行程可能還握著 pipe；不等它，免得卡住收尾。
    let seen = if status.is_ok() {
        let _ = done_rx.recv_timeout(std::time::Duration::from_secs(2));
        seen.lock().unwrap().clone()
    } else {
        String::new()
    };
    Ok((status?, seen))
}

fn wait_child(mut child: Child, what: &str, deadline: &Deadline) -> anyhow::Result<ExitStatus> {
    loop {
        if let Some(status) = child.try_wait().map_err(|e| anyhow::anyhow!("{what}: {e}"))? {
            return Ok(status);
        }
        if let Some(why) = deadline.stop() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow::Error::new(why));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn limit_text(secs: u64) -> String {
    if secs % 60 == 0 {
        format!("{} 分鐘", secs / 60)
    } else {
        format!("{secs} 秒")
    }
}

/// 遠端該同步的根，以及 cwd 相對於它的路徑（`""`＝cwd 就是根）。
///
/// 跟 cargo 一樣往上找：最近一層宣告 `[workspace]` 的 `Cargo.toml` 就是工作區根；一路上都沒有就用最近的那個套件根；
/// 完全不在 cargo 專案裡就是 cwd 本身。只同步 cwd（#177）的話，`cd daemon && cargo check` 到了遠端沒有根的
/// `Cargo.lock`、`[profile.*]`、`.cargo/config.toml`——本機用鎖定的依賴，遠端重新解析、驗的不是同一份東西。
fn source_root(cwd: &Path) -> (PathBuf, String) {
    // A workspace member can be reached through a symlink. Walk the real path so we still find
    // the workspace manifest above it; syncing the symlink itself would make --delete treat one
    // member as the whole shared source tree.
    let canonical_cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut package: Option<&Path> = None;
    let mut root: Option<&Path> = None;
    for dir in canonical_cwd.ancestors() {
        let Ok(manifest) = std::fs::read_to_string(dir.join("Cargo.toml")) else { continue };
        package.get_or_insert(dir);
        if manifest.parse::<toml::Table>().is_ok_and(|t| t.contains_key("workspace")) {
            root = Some(dir);
            break;
        }
    }
    let root = root.or(package).unwrap_or(&canonical_cwd);
    let sub = canonical_cwd.strip_prefix(root).map(|r| r.to_string_lossy().into_owned()).unwrap_or_default();
    (root.to_path_buf(), sub)
}

fn open_lease(remote: &BuildRemoteCfg, data_dir: &Path, cwd: &Path, adm: Admission, ctl: &Deadline) -> anyhow::Result<Lease> {
    let root = match remote.remote_root.trim().trim_end_matches('/') {
        "" => crate::config::default_remote_build_root(),
        r => r.to_string(),
    };
    let hash = cache_hash(cwd);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let job = format!("job-{}-{millis}", std::process::id());
    let token = LeaseToken::new();
    let cmd = host_shell(remote, data_dir, &guard_script(&root, &hash, &job, &token, adm))?;
    match Lease::start(cmd, token, ctl) {
        Ok(lease) => Ok(lease),
        Err(mut e) => {
            if let Some(qf) = e.downcast_mut::<QueueFull>() {
                if qf.host.is_empty() {
                    qf.host = remote.host.clone();
                }
            }
            Err(e)
        }
    }
}

/// rsync 的 `-e`：跟 [`ssh_cmd`] 同一組連線選項（`ssh` 那條與這條的行為不能各走各的）。
fn rsync_rsh(remote: &BuildRemoteCfg, has_password: bool, askpass: bool) -> String {
    let mut ssh = format!(
        "ssh -p {} {} -o StrictHostKeyChecking=accept-new -o {}",
        remote.ssh_port,
        SSH_LIVENESS_OPTS.join(" "),
        if has_password { "BatchMode=no" } else { "BatchMode=yes" }
    );
    if askpass {
        ssh.push_str(" -o NumberOfPasswordPrompts=1");
    }
    if !remote.identity_file.is_empty() {
        ssh.push_str(&format!(" -i {} -o IdentitiesOnly=yes", sh_quote(&remote.identity_file)));
    }
    ssh
}

/// `target` 要錨定在根，否則會把 `src/target` 這類來源目錄也濾掉。其他專案排除項只能由其
/// `[workspace.metadata.agents-manager.remote-cargo].exclude` 明確宣告。
const BASE_RSYNC_EXCLUDES: [&str; 2] = [".git", "/target"];

/// 工具狀態固定排除、只錨定根；前端產物仍是來源，因 release profile 與專案程式都可能讀取它。
/// 只影響來源掃描、rsync 與失效 touch；
/// 不進遠端清理（[`prune_remote_exclusions`]）：沒有任何專案設定排除時，同步不該多一趟 ssh，也不該因為遠端目錄還不存在而失敗。
const IMPLICIT_RSYNC_EXCLUDES: [&str; 1] = ["/.claude"];

/// checksum 讓相同大小與 mtime、但內容不同的來源也會傳輸；停用 mtime 保留，避免舊分支切回來時來源比 target 產物舊。
const RSYNC_CACHE_FLAGS: [&str; 2] = ["--checksum", "--no-times"];
const SOURCE_MANIFEST_MAGIC: &[u8] = b"AMRCM1\0";
const MAX_SOURCE_MANIFEST_BYTES: usize = 256 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceEntry {
    kind: u8,
    mode: u32,
    value: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceManifest {
    entries: BTreeMap<Vec<u8>, SourceEntry>,
}

impl SourceManifest {
    fn encode(&self) -> Vec<u8> {
        let mut out = SOURCE_MANIFEST_MAGIC.to_vec();
        out.extend_from_slice(&(self.entries.len() as u64).to_be_bytes());
        for (path, entry) in &self.entries {
            out.extend_from_slice(&(path.len() as u64).to_be_bytes());
            out.extend_from_slice(path);
            out.push(entry.kind);
            out.extend_from_slice(&entry.mode.to_be_bytes());
            out.extend_from_slice(&(entry.value.len() as u64).to_be_bytes());
            out.extend_from_slice(&entry.value);
        }
        out
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > MAX_SOURCE_MANIFEST_BYTES || !bytes.starts_with(SOURCE_MANIFEST_MAGIC) {
            return None;
        }
        let mut at = SOURCE_MANIFEST_MAGIC.len();
        let count = take_u64(bytes, &mut at)?;
        if count > bytes.len() as u64 {
            return None;
        }
        let mut entries = BTreeMap::new();
        for _ in 0..count {
            let path_len = usize::try_from(take_u64(bytes, &mut at)?).ok()?;
            let path = take_bytes(bytes, &mut at, path_len)?.to_vec();
            let kind = *bytes.get(at)?;
            at += 1;
            let mode = u32::from_be_bytes(take_bytes(bytes, &mut at, 4)?.try_into().ok()?);
            let value_len = usize::try_from(take_u64(bytes, &mut at)?).ok()?;
            let value = take_bytes(bytes, &mut at, value_len)?.to_vec();
            let path_buf = path_from_bytes(&path)?;
            if path.is_empty()
                || path_buf.is_absolute()
                || path_buf.components().any(|c| {
                    matches!(
                        c,
                        std::path::Component::ParentDir
                            | std::path::Component::RootDir
                            | std::path::Component::Prefix(_)
                    )
                })
                || entries
                    .insert(path, SourceEntry { kind, mode, value })
                    .is_some()
            {
                return None;
            }
        }
        (at == bytes.len()).then_some(Self { entries })
    }
}

fn take_bytes<'a>(bytes: &'a [u8], at: &mut usize, len: usize) -> Option<&'a [u8]> {
    let end = at.checked_add(len)?;
    let out = bytes.get(*at..end)?;
    *at = end;
    Some(out)
}

fn take_u64(bytes: &[u8], at: &mut usize) -> Option<u64> {
    Some(u64::from_be_bytes(
        take_bytes(bytes, at, 8)?.try_into().ok()?,
    ))
}

fn path_from_bytes(bytes: &[u8]) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt as _;
    Some(PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec())))
}

fn rsync_exclusions(root: &Path) -> anyhow::Result<Vec<String>> {
    let manifest = std::fs::read_to_string(root.join("Cargo.toml"))?;
    let value: toml::Value = toml::from_str(&manifest)?;
    let configured = value
        .get("workspace")
        .and_then(|w| w.get("metadata"))
        .and_then(|m| m.get("agents-manager"))
        .and_then(|a| a.get("remote-cargo"))
        .and_then(|r| r.get("exclude"));
    let mut exclusions = BASE_RSYNC_EXCLUDES
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    if let Some(configured) = configured {
        let values = configured.as_array().ok_or_else(|| {
            anyhow::anyhow!(
                "workspace.metadata.agents-manager.remote-cargo.exclude must be an array of paths"
            )
        })?;
        for value in values {
            let path = value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("remote-cargo exclude entries must be strings"))?;
            let Some(relative) = path.strip_prefix('/') else {
                anyhow::bail!("remote-cargo exclude paths must start with `/`: {path:?}")
            };
            if relative.is_empty()
                || relative
                    .split('/')
                    .any(|part| part.is_empty() || part == "." || part == "..")
                || !path
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'-' | b'.'))
                || exclusions.contains(&path.to_string())
            {
                anyhow::bail!("invalid remote-cargo exclude path: {path:?}");
            }
            exclusions.push(path.to_string());
        }
    }
    // 根 `.claude/` 的 worktree 與其 target 合計十幾 GB，不能整個掃描、送上遠端；巢狀 `.claude` 是原始碼，照送。
    // 前端產物可能是 release build 或專案自行 include 的輸入，照常同步。
    exclusions.extend(IMPLICIT_RSYNC_EXCLUDES.iter().map(|s| s.to_string()));
    Ok(exclusions)
}

fn is_excluded_source_path(relative: &Path, exclusions: &[String]) -> bool {
    use std::os::unix::ffi::OsStrExt as _;
    if relative
        .components()
        .any(|c| c.as_os_str().as_bytes() == b".git")
    {
        return true;
    }
    let bytes = relative.as_os_str().as_bytes();
    if bytes == b"target" || bytes.starts_with(b"target/") {
        return true;
    }
    exclusions
        .iter()
        .skip(BASE_RSYNC_EXCLUDES.len())
        .any(|entry| {
            let prefix = entry.trim_start_matches('/').as_bytes();
            bytes == prefix || bytes.starts_with(&[prefix, b"/"].concat())
        })
}

fn is_implicit_source_exclusion(relative: &Path) -> bool {
    IMPLICIT_RSYNC_EXCLUDES.iter().any(|entry| {
        let excluded = Path::new(entry.trim_start_matches('/'));
        relative == excluded || relative.starts_with(excluded)
    })
}

fn scan_source_manifest(root: &Path, exclusions: &[String]) -> anyhow::Result<SourceManifest> {
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::PermissionsExt as _;

    fn visit(
        root: &Path,
        dir: &Path,
        exclusions: &[String],
        entries: &mut BTreeMap<Vec<u8>, SourceEntry>,
    ) -> anyhow::Result<()> {
        for child in std::fs::read_dir(dir)? {
            let child = child?;
            let path = child.path();
            let relative = path.strip_prefix(root)?;
            if is_excluded_source_path(relative, exclusions) {
                continue;
            }
            let metadata = std::fs::symlink_metadata(&path)?;
            let kind = metadata.file_type();
            let (tag, value) = if kind.is_symlink() {
                (
                    b'L',
                    std::fs::read_link(&path)?.as_os_str().as_bytes().to_vec(),
                )
            } else if kind.is_dir() {
                (b'D', Vec::new())
            } else if kind.is_file() {
                let mut file = std::fs::File::open(&path)?;
                let mut hash = Sha256::new();
                std::io::copy(&mut file, &mut DigestWriter(&mut hash))?;
                (b'F', hash.finalize().to_vec())
            } else {
                (b'O', Vec::new())
            };
            entries.insert(
                relative.as_os_str().as_bytes().to_vec(),
                SourceEntry {
                    kind: tag,
                    mode: metadata.permissions().mode() & 0o7777,
                    value,
                },
            );
            if kind.is_dir() {
                visit(root, &path, exclusions, entries)?;
            }
        }
        Ok(())
    }

    struct DigestWriter<'a>(&'a mut Sha256);
    impl std::io::Write for DigestWriter<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut entries = BTreeMap::new();
    // build scripts may inspect permissions of the workspace root as well as its children.
    entries.insert(b".".to_vec(), SourceEntry {
        kind: b'D',
        mode: std::fs::metadata(root)?.permissions().mode() & 0o7777,
        value: Vec::new(),
    });
    visit(root, root, exclusions, &mut entries)?;
    Ok(SourceManifest { entries })
}

fn touch_remote_workspace(
    remote: &BuildRemoteCfg,
    data_dir: &Path,
    dir: &str,
    deadline: &Deadline,
) -> anyhow::Result<()> {
    let mut script = format!(
        "set -eu\nlease_dir={}\n[ -d \"$lease_dir\" ] && [ ! -L \"$lease_dir\" ] || exit 2\nroot=$(cd -P -- \"$lease_dir\" && pwd) || exit 2\n[ \"$root\" = \"$lease_dir\" ] || exit 2\ncd -- \"$root\"\n",
        sh_quote(dir)
    );
    script.push_str(REMOTE_TOUCH_MTIME_SETUP);
    script.push_str(
        "if prepare_touch_stamp && find -P . \\( -path './target' -o -path './.git' -o -path './.claude' \\) -prune -o \\( -type f -o -type l \\) -exec touch -h -m -d \"@$touch_stamp\" -- '{}' + && verify_touch_mtimes && wait_for_touch_stamp; then\n  :\nelse\n  invalidate_target_fingerprints\n  find -P . \\( -path './target' -o -path './.git' -o -path './.claude' \\) -prune -o -type f -exec touch -m -- '{}' +\n  find -P . \\( -path './target' -o -path './.git' -o -path './.claude' \\) -prune -o -type l -exec touch -h -m -- '{}' +\nfi\n",
    );
    let cmd = host_shell(remote, data_dir, &script)?;
    let status = run_status(cmd, "invalidate remote Cargo workspace sources", deadline)?;
    if !status.success() {
        anyhow::bail!("remote workspace source invalidation failed with exit {:?}", status.code());
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum SourceInvalidation {
    Unchanged,
    Files(Vec<PathBuf>),
    Workspace,
}

#[derive(Debug, Default)]
struct ManifestDiffCause {
    count: usize,
    paths: Vec<String>,
}

#[derive(Debug)]
struct SourceInvalidationReport {
    action: SourceInvalidation,
    safe_files: Vec<PathBuf>,
    touch_paths: BTreeSet<PathBuf>,
    fallback: BTreeMap<&'static str, ManifestDiffCause>,
}

impl SourceInvalidationReport {
    fn record_fallback(&mut self, cause: &'static str, path: Option<&[u8]>) {
        let detail = self.fallback.entry(cause).or_default();
        detail.count += 1;
        if detail.paths.len() < 8 {
            if let Some(path) = path {
                detail.paths.push(String::from_utf8_lossy(path).into_owned());
            }
        }
    }

    fn log(&self) {
        match &self.action {
            SourceInvalidation::Unchanged => return,
            SourceInvalidation::Files(paths) => {
                let sample = paths
                    .iter()
                    .take(8)
                    .map(|path| format!("{path:?}"))
                    .collect::<Vec<_>>();
                eprintln!(
                    "agents-manager: source manifest: targeted touch ({} paths): {}",
                    paths.len(),
                    sample.join(", ")
                );
            }
            SourceInvalidation::Workspace => {
                let reasons = self
                    .fallback
                    .iter()
                    .map(|(cause, detail)| {
                        let paths = if detail.paths.is_empty() {
                            String::new()
                        } else {
                            format!(" [{}]", detail.paths.join(", "))
                        };
                        format!("{cause} ({}{paths})", detail.count)
                    })
                    .collect::<Vec<_>>();
                let safe = self
                    .safe_files
                    .iter()
                    .take(8)
                    .map(|path| format!("{path:?}"))
                    .collect::<Vec<_>>();
                let safe_note = if safe.is_empty() {
                    String::new()
                } else {
                    format!(
                        "; otherwise-safe files ({} total): {}",
                        self.safe_files.len(),
                        safe.join(", ")
                    )
                };
                eprintln!(
                    "agents-manager: source manifest: full-workspace touch fallback: {}{safe_note}",
                    reasons.join("; ")
                );
            }
        }
    }
}

/// Build a timestamp strictly later than all Cargo fingerprints, dep-info and build-script output in the shared
/// target. If the remote cannot read or set that timestamp, callers invalidate those records
/// before falling back to the remote's ordinary `touch` behavior.
const REMOTE_TOUCH_MTIME_SETUP: &str = r#"
validate_target_cache_paths() {
  [ ! -L target ] || { echo 'agents-manager: refusing a Cargo target symlink' >&2; return 1; }
  [ ! -d target ] || {
    cache_symlink=$(find -P target -type l -print -quit) || return 1
    [ -z "$cache_symlink" ] || { echo 'agents-manager: refusing symlinks inside the Cargo target cache' >&2; return 1; }
  }
}
invalidate_target_fingerprints() {
  [ ! -L target ] || { echo 'agents-manager: refusing to invalidate through a target symlink' >&2; return 1; }
  [ ! -d target ] || {
    cache_symlink=$(find -P target -type l -print -quit) || return 1
    if [ -n "$cache_symlink" ]; then
      echo 'agents-manager: discarding Cargo target containing symlinks' >&2
      rm -rf -- target
      return
    fi
    find -P target -type d -name .fingerprint -prune -exec rm -rf -- '{}' +
    find -P target -type f -name '*.d' -exec rm -f -- '{}' +
  }
}
prepare_touch_stamp() {
  validate_target_cache_paths || return 1
  touch_now=$(LC_ALL=C date +%s.%N) || return 1
  touch_artifact_mtimes=
  if [ -d target ]; then
    touch_artifact_mtimes=$(LC_ALL=C find -P target -type f \( -path '*/.fingerprint/*' -o -name '*.d' -o -path '*/build/*/output' \) -printf '%T@\n') || return 1
  fi
  touch_artifact_max=$(printf '%s\n%s\n' "$touch_now" "$touch_artifact_mtimes" | LC_ALL=C awk '
    BEGIN { maximum = -1; valid = 1 }
    NF == 0 { next }
    NF != 1 || $1 !~ /^[0-9]+([.][0-9]+)?$/ { valid = 0; next }
    { if ($1 > maximum) maximum = $1 }
    END { if (!valid || maximum < 0) exit 1; printf "%.9f\n", maximum }
  ') || return 1
  LC_ALL=C awk -v now="$touch_now" -v artifact_max="$touch_artifact_max" 'BEGIN { exit !(artifact_max <= now) }' || return 1
  touch_stamp=$(LC_ALL=C awk -v now="$touch_now" -v artifact_max="$touch_artifact_max" 'BEGIN { maximum = now; if (artifact_max > maximum) maximum = artifact_max; printf "%.0f\n", int(maximum) + 1 }') || return 1
  [ -n "$touch_stamp" ]
}
touch_one_at_safe_mtime() {
  touch_path=./$1
  touch -m -d "@$touch_stamp" -- "$touch_path" || return 1
  touch_actual=$(LC_ALL=C find -P "$touch_path" -maxdepth 0 -printf '%T@') || return 1
  LC_ALL=C awk -v actual="$touch_actual" -v artifact_max="$touch_artifact_max" 'BEGIN { exit !(actual > artifact_max) }'
}
verify_touch_mtimes() {
  touch_source_mtimes=$(LC_ALL=C find -P . \( -path './target' -o -path './.git' -o -path './.claude' \) -prune -o \( -type f -o -type l \) -printf '%T@\n') || return 1
  printf '%s\n' "$touch_source_mtimes" | LC_ALL=C awk -v artifact_max="$touch_artifact_max" '
    NF == 0 { next }
    NF != 1 || $1 !~ /^[0-9]+([.][0-9]+)?$/ || $1 <= artifact_max { invalid = 1 }
    END { exit invalid }
  '
}
wait_for_touch_stamp() {
  touch_after=$(LC_ALL=C date +%s) || return 1
  if [ "$touch_after" -ge "$touch_stamp" ]; then return 0; fi
  sleep 1 || return 1
  touch_after=$(LC_ALL=C date +%s) || return 1
  [ "$touch_after" -ge "$touch_stamp" ]
}
"#;

/// Decide whether the last successful source manifest allows precise invalidation.
/// Existing regular-file changes can be invalidated narrowly; removing one touches its surviving
/// parent directory. Any added file can change module resolution or a proc macro directory listing
/// while absent from old dep-info, so additions force workspace invalidation.
/// Cargo's dep-info detects missing file inputs,
/// while parent-directory mtimes cover build-script watches for directory contents. Structural
/// changes, Cargo controls, and symlinks stay global.
#[cfg(test)]
fn source_invalidation(
    root: &Path,
    previous: Option<&SourceManifest>,
    current: &SourceManifest,
) -> SourceInvalidation {
    source_invalidation_report(root, previous, current).action
}

fn source_invalidation_report(
    root: &Path,
    previous: Option<&SourceManifest>,
    current: &SourceManifest,
) -> SourceInvalidationReport {
    let mut report = SourceInvalidationReport {
        action: SourceInvalidation::Unchanged,
        safe_files: Vec::new(),
        touch_paths: BTreeSet::new(),
        fallback: BTreeMap::new(),
    };
    let Some(previous) = previous else {
        report.record_fallback("missing successful baseline", None);
        report.action = SourceInvalidation::Workspace;
        return report;
    };
    if previous == current {
        return report;
    }

    // A directory symlink can feed include_* inputs in another package. A link outside the tree
    // is also unknowable from this manifest. If either exists, any source change gets full touch.
    for (bytes, entry) in &current.entries {
        if entry.kind != b'L' {
            continue;
        }
        let Some(path) = path_from_bytes(bytes) else {
            report.record_fallback("unrepresentable symlink path", Some(bytes));
            continue;
        };
        let Ok(target) = root.join(path).canonicalize() else {
            report.record_fallback("unresolved symlink", Some(bytes));
            continue;
        };
        if !target.starts_with(root) {
            report.record_fallback("symlink target outside workspace", Some(bytes));
        } else if target.is_dir() {
            report.record_fallback("directory symlink", Some(bytes));
        }
    }

    for (bytes, now) in &current.entries {
        let Some(before) = previous.entries.get(bytes) else {
            if is_cargo_control_path(bytes) {
                report.record_fallback("Cargo control input added", Some(bytes));
                continue;
            }
            // Compiler dep-info may list only existing files discovered by a proc macro.
            // New inputs (with any extension, including assets) can change its expansion
            // without a tracked directory, so no file addition is safe to narrow.
            let cause = match now.kind {
                b'D' => "added directory",
                b'L' => "symlink added",
                b'F' => "file added",
                _ => "added non-file path",
            };
            report.record_fallback(cause, Some(bytes));
            continue;
        };
        if before == now {
            continue;
        }
        if before.kind != now.kind {
            report.record_fallback("file type changed", Some(bytes));
            continue;
        }
        if before.kind != b'F' || now.kind != b'F' {
            report.record_fallback("non-file entry changed", Some(bytes));
            continue;
        }
        if is_cargo_control_path(bytes) {
            report.record_fallback("Cargo control input changed", Some(bytes));
            continue;
        }
        let Some(path) = safe_source_path(bytes) else {
            report.record_fallback("unsafe file path", Some(bytes));
            continue;
        };
        report.safe_files.push(path.clone());
        report.touch_paths.insert(path);
    }
    for bytes in previous.entries.keys() {
        if !current.entries.contains_key(bytes) {
            let before = &previous.entries[bytes];
            if is_cargo_control_path(bytes) {
                report.record_fallback("Cargo control input removed", Some(bytes));
                continue;
            }
            if before.kind == b'F' && !is_cargo_control_path(bytes) {
                if let Some(path) = safe_source_path(bytes) {
                    let parent = source_parent(&path);
                    if parent_dir_exists_in_manifest(current, &parent) {
                        report.safe_files.push(path);
                        report.touch_paths.insert(parent);
                        continue;
                    }
                }
            }
            let cause = match before.kind {
                b'D' => "removed directory",
                b'L' => "symlink removed",
                b'F' => "removed path",
                _ => "removed non-file path",
            };
            report.record_fallback(cause, Some(bytes));
        }
    }

    if report.fallback.is_empty() && report.touch_paths.is_empty() {
        report.record_fallback("unclassified manifest difference", None);
    }
    report.action = if report.fallback.is_empty() {
        if report.touch_paths.is_empty() {
            SourceInvalidation::Unchanged
        } else {
            SourceInvalidation::Files(report.touch_paths.iter().cloned().collect())
        }
    } else {
        SourceInvalidation::Workspace
    };
    report
}

fn safe_source_path(bytes: &[u8]) -> Option<PathBuf> {
    let path = path_from_bytes(bytes)?;
    if bytes.contains(&0)
        || path.is_absolute()
        || path.components().any(|part| {
            matches!(
                part,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
        || path.to_str().is_none()
    {
        None
    } else {
        Some(path)
    }
}

fn source_parent(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

fn parent_dir_exists_in_manifest(manifest: &SourceManifest, parent: &Path) -> bool {
    if parent == Path::new(".") {
        return true;
    }
    use std::os::unix::ffi::OsStrExt as _;
    manifest
        .entries
        .get(parent.as_os_str().as_bytes())
        .is_some_and(|entry| entry.kind == b'D')
}

fn is_cargo_control_path(path: &[u8]) -> bool {
    path.split(|byte| *byte == b'/').any(|component| {
        component == b"Cargo.toml"
            || component == b"Cargo.lock"
            || component == b"build.rs"
            || component == b".cargo"
            || component == b"rust-toolchain"
            || component == b"rust-toolchain.toml"
    })
}

fn touch_remote_files(
    remote: &BuildRemoteCfg,
    data_dir: &Path,
    dir: &str,
    paths: &[PathBuf],
    deadline: &Deadline,
) -> anyhow::Result<()> {
    // The root .claude tree is absent from rsync and manifests; do not touch stale leftovers even
    // if a path is passed directly.
    let paths: Vec<_> = paths
        .iter()
        .filter(|path| !is_implicit_source_exclusion(path))
        .collect();
    if paths.is_empty() {
        return Ok(());
    }
    let mut script = format!(
        "set -eu\nlease_dir={}\n[ -d \"$lease_dir\" ] && [ ! -L \"$lease_dir\" ] || exit 2\nroot=$(cd -P -- \"$lease_dir\" && pwd) || exit 2\n[ \"$root\" = \"$lease_dir\" ] || exit 2\ncd -- \"$root\"\n",
        sh_quote(dir)
    );
    script.push_str(REMOTE_TOUCH_MTIME_SETUP);
    script.push_str("touch_fallback=0\nif ! prepare_touch_stamp; then\n  invalidate_target_fingerprints\n  touch_fallback=1\nfi\n");
    for path in &paths {
        let relative = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path cannot be individually invalidated"))?;
        script.push_str(&format!(
            "if [ \"$touch_fallback\" = 1 ]; then\n  touch -m -- {}\nelse\n  if ! touch_one_at_safe_mtime {}; then\n    invalidate_target_fingerprints\n    touch_fallback=1\n    touch -m -- {}\n  fi\nfi\n",
            sh_quote(relative),
            sh_quote(relative),
            sh_quote(relative)
        ));
    }
    script.push_str(
        "if [ \"$touch_fallback\" = 0 ] && ! wait_for_touch_stamp; then\n  invalidate_target_fingerprints\n  touch_fallback=1\nfi\nif [ \"$touch_fallback\" = 1 ]; then\n",
    );
    for path in &paths {
        let relative = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path cannot be individually invalidated"))?;
        script.push_str(&format!("  touch -m -- {}\n", sh_quote(relative)));
    }
    script.push_str("fi\n");
    let cmd = host_shell(remote, data_dir, &script)?;
    let status = run_status(
        cmd,
        "invalidate changed remote Cargo source files",
        deadline,
    )?;
    if !status.success() {
        anyhow::bail!(
            "changed remote Cargo source invalidation failed with exit {:?}",
            status.code()
        );
    }
    Ok(())
}

fn remote_manifest_path(dir: &str) -> String {
    format!("{}.source-manifest", dir.trim_end_matches('/'))
}

fn read_remote_manifest(
    remote: &BuildRemoteCfg,
    data_dir: &Path,
    dir: &str,
    deadline: &Deadline,
) -> anyhow::Result<Option<SourceManifest>> {
    let path = remote_manifest_path(dir);
    // Validate before rsync, including unchanged hot runs: neither the lease nor Cargo's
    // cache may redirect writes through symlinks. Reuse this mandatory SSH round trip.
    let mut script = format!(
        "set -eu\nlease_dir={}\n[ ! -L \"$lease_dir\" ] || exit 2\n[ -e \"$lease_dir\" ] || exit 44\n[ -d \"$lease_dir\" ] || exit 2\nroot=$(cd -P -- \"$lease_dir\" && pwd) || exit 2\n[ \"$root\" = \"$lease_dir\" ] || exit 2\ncd -- \"$root\"\n",
        sh_quote(dir),
    );
    script.push_str(REMOTE_TOUCH_MTIME_SETUP);
    script.push_str("if ! validate_target_cache_paths; then invalidate_target_fingerprints; fi\n");
    script.push_str(&format!("if [ -f {0} ]; then cat {0}; else exit 44; fi", sh_quote(&path)));
    let cmd = host_shell(remote, data_dir, &script)?;
    let out = output_with_deadline(cmd, "read remote Cargo source manifest", deadline)?;
    if out.status.code() == Some(44) {
        return Ok(None);
    }
    if !out.status.success() {
        anyhow::bail!(
            "read remote Cargo source manifest failed with exit {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(SourceManifest::decode(&out.stdout))
}

fn write_remote_manifest(
    remote: &BuildRemoteCfg,
    data_dir: &Path,
    dir: &str,
    manifest: &[u8],
    deadline: &Deadline,
) -> anyhow::Result<()> {
    let path = remote_manifest_path(dir);
    let tmp = format!("{path}.tmp-{}", std::process::id());
    let script = format!(
        "umask 077; tmp={}; trap 'rm -f \"$tmp\"' EXIT; cat > \"$tmp\" && mv -f \"$tmp\" {}",
        sh_quote(&tmp),
        sh_quote(&path)
    );
    let mut cmd = host_shell(remote, data_dir, &script)?;
    cmd.stdin(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("write remote Cargo source manifest: {e}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("write remote Cargo source manifest: no stdin"))?
        .write_all(manifest)?;
    let status = wait_child(child, "write remote Cargo source manifest", deadline)?;
    if !status.success() {
        anyhow::bail!(
            "write remote Cargo source manifest failed with exit {:?}",
            status.code()
        );
    }
    Ok(())
}

fn prune_remote_exclusions(
    remote: &BuildRemoteCfg,
    data_dir: &Path,
    dir: &str,
    exclusions: &[String],
    deadline: &Deadline,
) -> anyhow::Result<()> {
    let paths: Vec<_> = exclusions
        .iter()
        .skip(BASE_RSYNC_EXCLUDES.len())
        .filter(|path| !IMPLICIT_RSYNC_EXCLUDES.contains(&path.as_str()))
        .map(|path| path.trim_start_matches('/'))
        .collect();
    if paths.is_empty() {
        return Ok(());
    }
    let mut script = format!(
        "set -eu\nlease_dir={}\n[ -d \"$lease_dir\" ] && [ ! -L \"$lease_dir\" ] || exit 2\nroot=$(cd -P -- \"$lease_dir\" && pwd) || exit 2\n[ \"$root\" = \"$lease_dir\" ] || exit 2\ncd -- \"$root\"\nsafe_remove() {{\n  rel=$1; depth=$2\n  case \"$rel\" in ''|/*|../*|*/../*|*/..|.|./*) return 2 ;; esac\n  parent=${{rel%/*}}; if [ \"$parent\" = \"$rel\" ]; then parent=; fi\n  if [ -n \"$parent\" ]; then\n    oldifs=$IFS; IFS=/; set -- $parent; IFS=$oldifs\n    cursor=$root\n    for component do\n      cursor=$cursor/$component\n      if [ -L \"$cursor\" ]; then echo 'agents-manager: refusing remote exclusion under a symlink ancestor' >&2; return 1; fi\n      [ -d \"$cursor\" ] || return 0\n    done\n  fi\n  target=$root/$rel\n  case \"$target\" in \"$root\"/*) ;; *) return 2 ;; esac\n  find -P . -mindepth \"$depth\" -maxdepth \"$depth\" -path \"./$rel\" -prune -execdir rm -rf -- '{{}}' +\n}}\n",
        sh_quote(dir)
    );
    for path in &paths {
        let depth = path.split('/').count();
        script.push_str(&format!("safe_remove {} {depth}\n", sh_quote(path)));
    }
    let cmd = host_shell(remote, data_dir, &script)?;
    let status = run_status(
        cmd,
        "remove explicitly excluded remote Cargo inputs",
        deadline,
    )?;
    if !status.success() {
        anyhow::bail!(
            "remove explicitly excluded remote Cargo inputs failed with exit {:?}",
            status.code()
        );
    }
    Ok(())
}

/// rsync 失敗是不是「來源樹在同步途中被改動」：掃完檔案清單之後檔案被刪／換名（`bun run build` 換掉 `web/dist` 的 hash 檔名）。
/// 24＝rsync 自己說的 vanished；23（部分傳輸失敗）只有在它抱怨的是「找不到檔案」時才算，其他 23（權限等）照舊直接失敗。
pub fn source_changed_during_sync(code: Option<i32>, stderr: &str) -> bool {
    match code {
        Some(24) => true,
        Some(23) => stderr.contains("No such file or directory") || stderr.contains("vanished"),
        _ => false,
    }
}

/// 來源樹在同步途中被改動時最多同步幾次（含第一次）。重試之間稍等，讓別人的 build 寫完。
const SYNC_ATTEMPTS: u32 = 3;
const SYNC_RETRY_PAUSE: std::time::Duration = std::time::Duration::from_millis(1500);

fn rsync_destination(remote: &BuildRemoteCfg, dir: &str) -> String {
    // 本機執行器：直接寫本機路徑（守門交回來的是絕對路徑）。
    if remote.is_local() {
        return format!("{}/", dir.trim_end_matches('/'));
    }
    let host = if remote.host.contains(':') { format!("[{}]", remote.host) } else { remote.host.clone() };
    format!("{}@{}:{}/", remote.user, host, dir.trim_end_matches('/'))
}

fn sync_source(remote: &BuildRemoteCfg, data_dir: &Path, cwd: &Path, dir: &str, deadline: &Deadline) -> anyhow::Result<()> {
    if !has_program(&rsync_program()) {
        anyhow::bail!("remote Cargo requires `rsync` on the daemon host");
    }
    let local = remote.is_local();
    if !local {
        check_identity(remote)?;
    }
    let root = std::fs::canonicalize(cwd)?;
    let pw = if local { None } else { secret(data_dir, remote)? };
    let mode = match pw {
        Some(_) => Some(password_mode(data_dir)?),
        None => None,
    };
    let ssh = rsync_rsh(
        remote,
        pw.is_some(),
        matches!(mode, Some(PwMode::Askpass(_))),
    );
    let source = format!("{}/", root.to_string_lossy().trim_end_matches('/'));
    let dest = rsync_destination(remote, dir);
    // The successful manifest is the baseline across every retry. A partial rsync may already
    // have applied chmod or type changes before returning 23/24; those differences must survive.
    let previous = read_remote_manifest(remote, data_dir, dir, deadline)?;
    let command = |exclusions: &[String]| {
        let mut cmd = match &mode {
            Some(PwMode::Sshpass) => {
                let mut c = Command::new("sshpass");
                c.arg("-e").arg("rsync");
                c
            }
            _ => Command::new(rsync_program()),
        };
        if let (Some(pw), Some(m)) = (pw.as_deref(), &mode) {
            match m {
                PwMode::Sshpass => {
                    cmd.env("SSHPASS", pw);
                }
                PwMode::Askpass(helper) => {
                    cmd.env("AM_SSH_PASSWORD", pw).env("SSH_ASKPASS", helper).env("SSH_ASKPASS_REQUIRE", "force");
                }
            }
        }
        // 本機複製不壓縮（`-z` 只會白燒 CPU）。
        cmd.args([if local { "-a" } else { "-az" }, "--delete"])
            .args(RSYNC_CACHE_FLAGS)
            .arg("--quiet");
        for exclude in exclusions {
            cmd.arg("--exclude").arg(exclude);
        }
        if !local {
            cmd.args(["-e", &ssh]);
        }
        cmd.arg(&source).arg(&dest);
        cmd
    };
    // 共用主樹的 `web/dist` 會被別的 bot 的 `bun run build` 整批換掉；rsync 列完清單後舊檔消失就回 23/24。
    // 重送一次就是新的一致狀態（`--delete` 也會清掉遠端的舊 hash 檔），所以短暫重試，不把呼叫端的 cargo 判死。
    for attempt in 1..=SYNC_ATTEMPTS {
        let exclusions = rsync_exclusions(&root)?;
        let before = scan_source_manifest(&root, &exclusions)?;
        prune_remote_exclusions(remote, data_dir, dir, &exclusions, deadline)?;
        let (status, stderr) =
            run_status_tee_stderr(command(&exclusions), "rsync source", deadline)?;
        if !status.success() {
            if !source_changed_during_sync(status.code(), &stderr) {
                anyhow::bail!("rsync failed with exit {:?}", status.code());
            }
            if attempt == SYNC_ATTEMPTS {
                anyhow::bail!(
                    "rsync failed with exit {:?}: 來源樹在同步時一直被改動（多半是別的 bot 正在 build web，換掉了 web/dist），重試 {SYNC_ATTEMPTS} 次仍失敗；等它 build 完再跑",
                    status.code()
                );
            }
            wait_sync_retry(attempt, deadline)?;
            continue;
        }

        let current_exclusions = rsync_exclusions(&root)?;
        let after = scan_source_manifest(&root, &current_exclusions)?;
        if before != after {
            if attempt == SYNC_ATTEMPTS {
                anyhow::bail!(
                    "來源樹在同步期間持續變動，重試 {SYNC_ATTEMPTS} 次仍無法得到一致快照"
                );
            }
            wait_sync_retry(attempt, deadline)?;
            continue;
        }

        let invalidation = source_invalidation_report(&root, previous.as_ref(), &after);
        invalidation.log();
        match invalidation.action {
            SourceInvalidation::Unchanged => {}
            SourceInvalidation::Files(paths) => {
                touch_remote_files(remote, data_dir, dir, &paths, deadline)?;
            }
            SourceInvalidation::Workspace => {
                // Full touch is the conservative path for structural changes and inputs whose
                // dependency scope cannot be proven from the manifest.
                touch_remote_workspace(remote, data_dir, dir, deadline)?;
            }
        }

        // A source edit after rsync or while mtime updates ran must not be recorded as successfully
        // synchronized. Keep the prior manifest so the next attempt will invalidate all differences.
        let verified_exclusions = rsync_exclusions(&root)?;
        let verified = scan_source_manifest(&root, &verified_exclusions)?;
        if after != verified {
            if attempt == SYNC_ATTEMPTS {
                anyhow::bail!(
                    "來源樹在同步期間持續變動，重試 {SYNC_ATTEMPTS} 次仍無法提交一致快照"
                );
            }
            wait_sync_retry(attempt, deadline)?;
            continue;
        }
        write_remote_manifest(remote, data_dir, dir, &verified.encode(), deadline)?;
        return Ok(());
    }
    unreachable!("the loop always returns or bails on the last attempt")
}

fn wait_sync_retry(attempt: u32, deadline: &Deadline) -> anyhow::Result<()> {
    eprintln!("agents-manager: 來源樹在同步時被改動（多半是別的 bot 正在 build web），{}ms 後重新同步（第 {} 次，共 {SYNC_ATTEMPTS} 次）", SYNC_RETRY_PAUSE.as_millis(), attempt + 1);
    let until = std::time::Instant::now() + SYNC_RETRY_PAUSE;
    while std::time::Instant::now() < until {
        if let Some(why) = deadline.stop() {
            return Err(anyhow::Error::new(why));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Ok(())
}

/// 測試執行緒數：呼叫端自己設了 `RUST_TEST_THREADS`（正整數）就尊重它，沒有才用設定（`0`＝不設）。
fn pick_test_threads(caller: Option<&str>, configured: usize) -> usize {
    caller.and_then(|v| v.trim().parse::<usize>().ok()).filter(|n| *n > 0).unwrap_or(configured)
}

fn run_remote(remote: &BuildRemoteCfg, data_dir: &Path, lease: &Lease, sub: &str, args: &[String], deadline: &Deadline) -> anyhow::Result<i32> {
    let threads = pick_test_threads(std::env::var("RUST_TEST_THREADS").ok().as_deref(), remote.test_threads);
    let mut cmd = host_shell(remote, data_dir, &run_script(&lease.hs.dir, sub, &lease.token, remote.cargo_jobs, threads, args))?;
    let status = if remote.is_local() {
        // 本機執行器的 cargo 自成 process group（不在前景）：接著終端的 stdin，測試一讀就被 SIGTTIN 停住；ssh 時遠端 stdin 本來就是 pipe。
        cmd.stdin(Stdio::null()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
        wait_child(cmd.spawn().map_err(|e| anyhow::anyhow!("remote cargo: {e}"))?, "remote cargo", deadline)?
    } else {
        run_status(cmd, "remote cargo", deadline)?
    };
    Ok(status.code().unwrap_or(1))
}

/// Entry point used by the installed cargo shim. Returns 125 when offload should not happen, so
/// the shim can run the real local Cargo instead. Once a configured remote attempt starts, transport
/// failures are errors (not silent local fallback) because callers must know what was actually verified.
/// `run_cli` 的決定：轉遠端（帶設定），或退回本機（帶要對使用者講的原因；`None`＝本來就沒啟用，不必講）。
pub enum Offload {
    /// 照設定順序的每台建置主機（[`BuildRemoteCfg::targets`]，#1956），至少一台。
    Go(Vec<BuildRemoteCfg>),
    Local(Option<String>),
}

/// 決定這次要不要轉遠端（#324、#325）。設定檔存在卻讀不懂不能靜默退回本機（同 #138 類）：使用者以為在遠端編，實際整批在本機排隊。
pub fn decide_offload(config_path: &Path, args: &[String], env: impl IntoIterator<Item = (String, String)>) -> Offload {
    if !eligible(args) {
        return Offload::Local(None);
    }
    let text = match std::fs::read_to_string(config_path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Offload::Local(None),
        Err(e) => return Offload::Local(Some(format!("讀不到設定檔 {}（{e}）", config_path.display()))),
    };
    let cfg: ConfigFile = match toml::from_str(&text) {
        Ok(c) => c,
        Err(e) => return Offload::Local(Some(format!("設定檔 {} 解析失敗（{e}）", config_path.display()))),
    };
    let remote = cfg.build.remote;
    if !remote.enabled {
        return Offload::Local(None);
    }
    let hosts = remote.targets();
    if hosts.is_empty() {
        return Offload::Local(Some("外部編譯啟用了但 host／user 是空的".into()));
    }
    let (hosts, skipped) = usable_targets(hosts);
    for why in &skipped {
        eprintln!("agents-manager: ⚠️ {why}，這台略過、用其他建置主機");
    }
    if hosts.is_empty() {
        return Offload::Local(skipped.into_iter().next());
    }
    if let Some(var) = env_blocking_offload(env) {
        return Offload::Local(Some(format!("本機設了 {var}，遠端看不到它，結果會跟本機不一致")));
    }
    Offload::Go(hosts)
}

/// `--data-dir` 沒給時的資料目錄（issue #417）：跟 daemon 帶 `--config` 啟動時同一套規則
/// （`[server] data_dir` > 設定檔所在目錄）。`scripts/check.sh` 為了不讓測試讀到正式資料目錄會
/// `env -u AM_DATA_DIR`，shim 因此不再把它當成轉遠端的前提；密碼檔在哪由設定檔說了算。
pub fn resolve_data_dir(config_path: &Path, given: Option<&Path>) -> anyhow::Result<PathBuf> {
    match given.filter(|d| !d.as_os_str().is_empty()) {
        Some(d) => Ok(d.to_path_buf()),
        None => crate::startup::data_dir(config_path, true, crate::startup::peek_data_dir(config_path).as_deref(), None),
    }
}

/// `only`＝`--host <名字>`：只用這一台、不分流（預熱逐台跑）；設定裡沒有這台就退回本機（125）。
pub fn run_cli(config_path: &Path, data_dir: Option<&Path>, cwd: &Path, args: &[String], only: Option<&str>) -> i32 {
    let hosts = match decide_offload(config_path, args, std::env::vars()) {
        Offload::Go(hosts) => match only_host(hosts, only) {
            Some(hosts) => hosts,
            None => {
                eprintln!("agents-manager: 這次 cargo 不轉外部編譯：設定裡沒有建置主機 {}", only.unwrap_or(""));
                return EXIT_RUN_LOCALLY;
            }
        },
        Offload::Local(why) => {
            if let Some(why) = why {
                eprintln!("agents-manager: 這次 cargo 不轉外部編譯，改在本機跑：{why}");
            }
            return EXIT_RUN_LOCALLY;
        }
    };
    let data_dir = match resolve_data_dir(config_path, data_dir) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("agents-manager: 這次 cargo 不轉外部編譯，改在本機跑：從設定檔 {} 推不出資料目錄（{e:#}）", config_path.display());
            return EXIT_RUN_LOCALLY;
        }
    };
    let data_dir = data_dir.as_path();
    eprintln!(
        "agents-manager: remote Cargo → {} ({})",
        hosts.iter().map(BuildRemoteCfg::target_label).collect::<Vec<_>>().join(" / "),
        args.first().map(String::as_str).unwrap_or("")
    );
    install_signal_handlers();
    let call_id = crate::db::ulid();
    // 整體上限是整組共用的（`hosts` 裡的主機不能各自設 timeout_secs）。
    run_offload_on_with_id(&hosts, data_dir, cwd, args, &Deadline::new(hosts[0].timeout_secs).watching_signals(), &call_id)
}

/// 這次 ssh 拿什麼登入（issue #428）。連不上的時候最想知道的就是這個——「沒設金鑰也沒有密碼檔」跟
/// 「金鑰設了但被拒」要修的地方完全不同。密碼檔只看在不在，不讀內容。
fn auth_desc(remote: &BuildRemoteCfg, data_dir: &Path) -> String {
    if remote.is_local() {
        return "本機執行器（不經 ssh）".into();
    }
    match (password_is_set(data_dir, remote), remote.identity_file.as_str()) {
        (true, "") => format!("{} 裡的密碼檔", data_dir.display()),
        (true, key) => format!("{} 裡的密碼檔，加上 identity_file {key}", data_dir.display()),
        (false, "") => {
            format!("ssh 預設的金鑰／agent（`[build.remote]` 沒設 identity_file，{} 也沒有密碼檔）", data_dir.display())
        }
        (false, key) => format!("`[build.remote] identity_file` 指的 {key}"),
    }
}

/// 連不上時印的那一段（issue #428）。**第一行就要講完整件事**：ssh 自己的抱怨已經先印在上面了，
/// 細節接在第二行——不然「這次沒有轉到外部編譯」會被埋在一堆 ssh 訊息裡，整批 child 靜靜擠回本機那兩個名額。
/// 純組字串，好測；真正的決定（回 [`EXIT_RUN_LOCALLY`]）在 [`run_offload`]。
#[cfg(test)]
fn unreachable_note(remote: &BuildRemoteCfg, auth: &str, why: &str) -> String {
    unreachable_note_then(remote, auth, why, "這次 cargo 改在本機跑（要排本機名額）")
}

/// 多台時連不上的那台不是最後一台：講「改試下一台」，不是「改在本機跑」（#1956）。
fn unreachable_note_then(remote: &BuildRemoteCfg, auth: &str, why: &str, then: &str) -> String {
    format!(
        "agents-manager: ⚠️ 連不上外部編譯主機 {}，{then}\n\
         agents-manager:    {why}；這次的登入方式是{auth}",
        remote.target_label()
    )
}

/// 把這一次的連線結論記在 data-dir（issue #428），`/api/build-slots` 的 `remote_reachable` 讀它。
/// 記不下來只講一聲：說明用的東西不能擋住這次編譯。
fn note_health(remote: &BuildRemoteCfg, data_dir: &Path, reachable: bool, reason: Option<String>) {
    let h = crate::remote_health::Health { reachable, checked_at: crate::db::now(), target: remote.target_label(), reason };
    if let Err(e) = crate::remote_health::record(data_dir, &h) {
        eprintln!("agents-manager: 記不下外部編譯主機的連線結論（{e}）");
    }
}

/// 真的把這次呼叫丟到遠端（已經確定要轉、設定也讀好了）。整體時間受 `remote.timeout_secs` 限制（issue #194）；收到終止訊號也會好好收尾（issue #201）。
#[cfg(test)]
fn run_offload(remote: &BuildRemoteCfg, data_dir: &Path, cwd: &Path, args: &[String], ctl: &Deadline) -> i32 {
    run_offload_on(std::slice::from_ref(remote), data_dir, cwd, args, ctl)
}

/// [`run_offload`] 的多台版（#1956）：先挑一台拿到租約（[`acquire`]），之後同步、編譯都在那一台。
fn run_offload_on(hosts: &[BuildRemoteCfg], data_dir: &Path, cwd: &Path, args: &[String], ctl: &Deadline) -> i32 {
    let call_id = crate::db::ulid();
    run_offload_on_with_id(hosts, data_dir, cwd, args, ctl, &call_id)
}

fn run_offload_on_with_id(hosts: &[BuildRemoteCfg], data_dir: &Path, cwd: &Path, args: &[String], ctl: &Deadline, call_id: &str) -> i32 {
    let mut used = 0;
    let result = (|| -> anyhow::Result<i32> {
        // 從這裡起不管怎麼離開（`?`、panic、被殺、超過上限、收到訊號），守門都會收到 EOF 把目錄還回去、遠端還在跑的 cargo 整組收掉。
        let (root, sub) = source_root(cwd);
        let (i, mut lease) = acquire(hosts, data_dir, &root, ctl)?;
        used = i;
        let remote = &hosts[i];
        let outcome = match lease.obs.outcome.as_deref() {
            Some("shared") => "shared",
            Some("isolated") => "isolated",
            _ => {
                let leaf = lease.hs.dir.rsplit_once('/').map(|(_, l)| l).unwrap_or(&lease.hs.dir);
                if is_shared_leaf(leaf) { "shared" } else { "isolated" }
            }
        };
        am_base::remote_event::record(data_dir, &am_base::remote_event::RemoteCargoEvent {
            ts: crate::db::now(),
            call_id: call_id.to_string(),
            bot_id: std::env::var("AM_BOT_ID").ok(),
            host: remote.host.clone(),
            phase: "final",
            outcome,
            shared_wait_secs: lease.obs.shared_wait_secs,
            slot_wait_secs: lease.obs.slot_wait_secs,
        });
        // 整體上限從拿到名額與目錄才開始算（issue #104）。
        ctl.arm();
        lease.collect(&select_gc(&lease.hs, &GcPolicy::from_cfg(remote)));
        sync_source(remote, data_dir, &root, &lease.hs.dir, ctl)?;
        let code = run_remote(remote, data_dir, &lease, &sub, args, ctl)?;
        lease.finish();
        Ok(code)
    })();
    let remote = &hosts[used];
    // `lease` 已經在上面那個閉包結束時放掉：守門的清理（收 process group、還目錄與鎖）做完才走到這裡。
    // 收到終止訊號：Ctrl-C 會同時打到本機的 rsync／ssh，它們的結束碼不代表這次的結果——一律照訊號回 128+sig。
    if let Some(Stopped::Interrupted(sig)) = ctl.stop() {
        eprintln!("agents-manager: 收到訊號 {sig}，已中止遠端編譯（遠端那一整組行程已收掉、目錄與鎖已還回）");
        return 128 + sig;
    }
    match result {
        Ok(code) => {
            note_health(remote, data_dir, true, None);
            code
        }
        // 連不上／認證被拒（issue #428）：遠端什麼都還沒跑，所以跟 125 那條一樣退回本機——但要講清楚原因，
        // 不然整批 child 會靜靜擠回本機那兩個名額，而沒有人知道外部編譯根本沒生效（同 issue #138 的教訓）。
        // 原因與健康檔在 [`acquire`] 裡逐台記過了。
        Err(e) if e.downcast_ref::<Unreachable>().is_some() => EXIT_RUN_LOCALLY,
        // 守門尚未交目錄、遠端還沒開工：改排本機名額（issue #1853）。
        // 守門有回話＝連得上，只是滿了（健康檔在 [`acquire`] 記在排的那台名下）。
        Err(e) => {
            if let Some(qf) = e.downcast_ref::<QueueFull>() {
                let ev = am_base::remote_event::RemoteCargoEvent {
                    ts: crate::db::now(),
                    call_id: call_id.to_string(),
                    bot_id: std::env::var("AM_BOT_ID").ok(),
                    host: qf.host.clone(),
                    phase: "final",
                    outcome: "local_fallback",
                    shared_wait_secs: qf.shared_wait_secs,
                    slot_wait_secs: qf.slot_wait_secs,
                };
                am_base::remote_event::record(data_dir, &ev);
                EXIT_RUN_LOCALLY
            } else if matches!(e.downcast_ref::<Stopped>(), Some(Stopped::TimedOut)) {
                eprintln!(
                    "agents-manager: 遠端編譯超過 {} 上限，已中止（遠端那一整組行程已收掉、目錄與鎖已還回；可調 [build.remote] timeout_secs，0＝不設上限）。這次不會退回本機重跑",
                    limit_text(remote.timeout_secs)
                );
                EXIT_TIMEOUT
            } else {
                eprintln!("agents-manager: remote Cargo failed: {e:#}");
                126
            }
        }
    }
}

#[cfg(test)]
mod tests {
    /// pre-1960 的歷史原樣 read_handshake 邏輯，專用於回歸對照測試。
    fn pre_1960_read_handshake(r: &mut impl BufRead) -> anyhow::Result<Handshake> {
        let mut hs = Handshake::default();
        let mut line = String::new();
        loop {
            line.clear();
            if r.read_line(&mut line)? == 0 {
                anyhow::bail!("remote workdir guard exited before handing out a directory");
            }
            let f: Vec<&str> = line.trim_end_matches(['\r', '\n']).split('\t').collect();
            match f.as_slice() {
                ["END"] if !hs.dir.is_empty() => return Ok(hs),
                ["ROOT", root] => hs.root = root.to_string(),
                ["DIR", dir, token] => {
                    hs.dir = dir.to_string();
                    hs.token = token.to_string();
                }
                ["D", path, k, b, idle] => hs.dirs.push(RemoteDir {
                    path: path.to_string(),
                    locked: *k == "1",
                    busy: *b == "1",
                    idle_secs: idle.parse::<i64>().unwrap_or(0).max(0) as u64,
                }),
                ["F", free, total] => hs.disk = free.parse().ok().zip(total.parse().ok()),
                ["SLOT", i, max] => hs.slot = i.parse().ok().zip(max.parse().ok()),
                ["BUSY", n] => return Err(anyhow::Error::new(Busy { waiters: n.parse().unwrap_or(0) })),
                _ => {}
            }
        }
    }

    #[test]
    fn pre_1960_parser_equivalence_on_new_outcome_fixtures() {
        // 包含新 OUTCOME 結構化行的 fixture 輸入
        let out = "Welcome!\nROOT\t/r\nDIR\t/r/0123456789abcdef/shared\tfeedfacefeedfacefeedfacefeedface\nSLOT\t2\t3\nD\t/r/00000000000000aa/111\t0\t1\t-3\nD\t/r/00000000000000aa/222\t1\t0\t900\nF\t500\t1000\nOUTCOME\tshared\t153\t0\nEND\n";

        let old_res = pre_1960_read_handshake(&mut std::io::Cursor::new(out)).unwrap();
        let new_res = read_handshake(&mut std::io::Cursor::new(out)).unwrap();

        // 逐欄驗證舊 parser 與新 parser 回傳完全一致（新 OUTCOME 控制行被舊 parser 依 _ => {} 乾淨略過）
        assert_eq!(old_res.root, new_res.root);
        assert_eq!(old_res.dir, new_res.dir);
        assert_eq!(old_res.token, new_res.token);
        assert_eq!(old_res.dirs, new_res.dirs);
        assert_eq!(old_res.disk, new_res.disk);
        assert_eq!(old_res.slot, new_res.slot);
    }

    #[test]
    fn read_handshake_backward_compatibility_without_outcome() {
        let out = "Welcome!\nROOT\t/r\nDIR\t/r/0123456789abcdef/shared\tfeedfacefeedfacefeedfacefeedface\nSLOT\t1\t3\nEND\n";
        let (hs, obs) = read_handshake_observed(&mut std::io::Cursor::new(out));
        let h = hs.unwrap();
        assert_eq!(h.dir, "/r/0123456789abcdef/shared");
        assert_eq!(obs.outcome, None);
        assert_eq!(obs.shared_wait_secs, None);
        assert_eq!(obs.slot_wait_secs, None);

        // 舊 wrapper 原樣成功
        let h_old = read_handshake(&mut std::io::Cursor::new(out)).unwrap();
        assert_eq!(h_old.dir, "/r/0123456789abcdef/shared");
    }

    #[test]
    fn read_handshake_with_outcome_control_line() {
        let out = "ROOT\t/r\nDIR\t/r/0123456789abcdef/shared\tfeedfacefeedfacefeedfacefeedface\nSLOT\t2\t3\nOUTCOME\tshared\t153\t0\nEND\n";
        let (hs, obs) = read_handshake_observed(&mut std::io::Cursor::new(out));
        let h = hs.unwrap();
        assert_eq!(h.dir, "/r/0123456789abcdef/shared");
        assert_eq!(obs.outcome.as_deref(), Some("shared"));
        assert_eq!(obs.shared_wait_secs, Some(153));
        assert_eq!(obs.slot_wait_secs, Some(0));
    }

    #[test]
    fn read_handshake_observed_captures_queue_full_outcome_before_eof() {
        let out = "OUTCOME\tqueue_full\t900\t4\n";
        let (hs, obs) = read_handshake_observed(&mut std::io::Cursor::new(out));
        assert!(hs.is_err(), "must fail due to EOF before DIR/END");
        assert_eq!(obs.outcome.as_deref(), Some("queue_full"));
        assert_eq!(obs.shared_wait_secs, Some(900));
        assert_eq!(obs.slot_wait_secs, Some(4));
    }

    #[test]
    fn rsync_destination_brackets_ipv6_hosts() {
        let mut r = remote();
        r.host = "fd7a:115c:a1e0::5".into();
        assert_eq!(rsync_destination(&r, "/home/u/shared/"), "me@[fd7a:115c:a1e0::5]:/home/u/shared/");
        assert_eq!(rsync_destination(&remote(), "/home/u/shared/"), "me@build-host:/home/u/shared/");
    }

    /// #328、#1856、#1861、#1858：固定排除 git、根 target、根 `.claude`；`web/dist` 仍須同步，未明確排除時 node_modules 也是輸入。
    #[test]
    fn the_rsync_excludes_keep_source_dirs_named_target() {
        if !has_program(&rsync_program()) {
            return;
        }
        let base = crate::testing::track(std::env::temp_dir().join(format!("am-rsync-{}", std::process::id())));
        let (src, dst) = (base.join("src"), base.join("dst"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("Cargo.toml"), "[workspace]\n").unwrap();
        for d in [
            "src/target",
            "crates/target",
            "target/debug",
            "web/node_modules",
            ".claude/worktrees/x/target",
            "crates/.claude",
        ] {
            std::fs::create_dir_all(src.join(d)).unwrap();
        }
        std::fs::write(src.join("src/target/mod.rs"), "x").unwrap();
        std::fs::write(src.join("crates/target/Cargo.toml"), "x").unwrap();
        std::fs::write(src.join("target/debug/big"), "x").unwrap();
        std::fs::write(src.join("web/node_modules/package.json"), "{}").unwrap();
        std::fs::create_dir_all(src.join("web/dist")).unwrap();
        std::fs::write(src.join("web/dist/index.html"), "CURRENT FRONTEND").unwrap();
        std::fs::write(src.join(".claude/worktrees/x/target/big"), "x").unwrap();
        std::fs::write(src.join("crates/.claude/keep.rs"), "x").unwrap();
        let excludes = rsync_exclusions(&src).unwrap();
        let manifest = scan_source_manifest(&src, &excludes).unwrap();
        assert!(manifest.entries.contains_key(b"web/dist".as_slice()), "frontend directory is a build input");
        assert!(manifest.entries.contains_key(b"web/dist/index.html".as_slice()), "frontend file is a build input");
        std::fs::create_dir_all(dst.join("web/dist")).unwrap();
        std::fs::write(dst.join("web/dist/index.html"), "STALE FRONTEND").unwrap();
        let mut cmd = Command::new(rsync_program());
        cmd.args(["-a", "--delete"]);
        for exclude in excludes {
            cmd.arg("--exclude").arg(exclude);
        }
        let st = cmd
            .arg(format!("{}/", src.display()))
            .arg(format!("{}/", dst.display()))
            .output()
            .unwrap();
        assert!(st.status.success(), "{}", String::from_utf8_lossy(&st.stderr));
        assert!(dst.join("src/target/mod.rs").exists(), "src/target 是原始碼，要送");
        assert!(dst.join("crates/target/Cargo.toml").exists(), "crates/target 是 workspace 成員，要送");
        assert!(!dst.join("target").exists(), "根目錄的 target 不送");
        assert!(!dst.join(".claude").exists(), "根目錄的 .claude（含 worktree 的 target）不送");
        assert_eq!(std::fs::read_to_string(dst.join("web/dist/index.html")).unwrap(), "CURRENT FRONTEND", "rsync replaces stale frontend content for every profile");
        assert!(dst.join("crates/.claude/keep.rs").is_file(), "巢狀的 .claude 是原始碼，要送");
        assert!(
            dst.join("web/node_modules/package.json").is_file(),
            "未設定排除政策時，保留 node_modules 輸入"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    /// #329：手改 config 的極大 timeout_secs 不能讓 arm() 溢位 panic；單次 ssh 卡住要有上限。
    #[test]
    fn a_huge_timeout_does_not_panic_and_a_hung_ssh_is_bounded() {
        let d = Deadline::new(u64::MAX);
        d.arm();
        assert!(d.stop().is_none());
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let t = std::time::Instant::now();
        let err = output_with_timeout(cmd, std::time::Duration::from_millis(300)).unwrap_err().to_string();
        assert!(err.contains("沒有結束") && t.elapsed() < std::time::Duration::from_secs(25), "{err}");
    }

    /// #417：shim 在 `AM_DATA_DIR` 被清掉時（`scripts/check.sh`）不帶 `--data-dir`，helper 要照 daemon 的規則從設定檔推：
    /// 密碼檔在資料目錄裡，推錯就是認證失敗。給了就照用。
    #[test]
    fn without_a_data_dir_the_helper_resolves_it_from_the_config_like_the_daemon() {
        let base = crate::testing::track(std::env::temp_dir().join(format!("am-r417-{}", crate::db::ulid())));
        let (beside, custom) = (base.join("conf"), base.join("conf/data"));
        std::fs::create_dir_all(&custom).unwrap();
        let canon = |p: &Path| std::fs::canonicalize(p).unwrap();
        let cfg = beside.join("config.toml");
        std::fs::write(&cfg, "[build.remote]\nenabled = true\n").unwrap();
        assert_eq!(canon(&resolve_data_dir(&cfg, None).unwrap()), canon(&beside), "沒寫 [server] data_dir：設定檔所在目錄");
        assert_eq!(canon(&resolve_data_dir(&cfg, Some(Path::new(""))).unwrap()), canon(&beside), "空字串當沒給");
        std::fs::write(&cfg, "[server]\ndata_dir = \"data\"\n").unwrap();
        assert_eq!(canon(&resolve_data_dir(&cfg, None).unwrap()), canon(&custom), "[server] data_dir 優先");
        assert_eq!(resolve_data_dir(&cfg, Some(&base)).unwrap(), base, "給了就照用");
        std::fs::remove_dir_all(&base).ok();
    }

    /// #324／#325：設定檔壞掉、host 空、或本機設了 RUSTFLAGS 這類遠端看不到的變數——要退回本機也要講出原因，不能靜默；
    /// 正常啟用且沒有這些變數才轉遠端；檔案不存在／沒啟用是本來就不轉，不必吵。
    #[test]
    fn the_offload_decision_never_falls_back_silently_when_something_is_wrong() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-decide-{}", std::process::id())));
        std::fs::create_dir_all(&dir).unwrap();
        let args = vec!["test".to_string()];
        let no_env = || Vec::<(String, String)>::new();
        let write = |body: &str| {
            let p = dir.join("c.toml");
            std::fs::write(&p, body).unwrap();
            p
        };
        let good = "[build.remote]\nenabled = true\nhost = \"h\"\nuser = \"u\"\n";
        assert!(matches!(decide_offload(&write(good), &args, no_env()), Offload::Go(_)));
        assert!(matches!(decide_offload(&dir.join("missing.toml"), &args, no_env()), Offload::Local(None)), "沒設定檔＝沒啟用，不吵");
        assert!(matches!(decide_offload(&write("[build.remote]\nenabled = false\n"), &args, no_env()), Offload::Local(None)));
        assert!(matches!(decide_offload(&write("[[["), &args, no_env()), Offload::Local(Some(_))), "壞掉的設定檔要講");
        assert!(matches!(decide_offload(&write("[build.remote]\nenabled = true\n"), &args, no_env()), Offload::Local(Some(_))), "啟用卻沒 host／user 要講");
        let env = vec![("RUSTFLAGS".to_string(), "-D warnings".to_string())];
        match decide_offload(&write(good), &args, env) {
            Offload::Local(Some(why)) => assert!(why.contains("RUSTFLAGS"), "{why}"),
            _ => panic!("RUSTFLAGS 有設就不能轉遠端（遠端看不到，會假綠）"),
        }
        assert!(env_blocking_offload([("RUSTFLAGS".to_string(), String::new())]).is_none(), "空值不算");
        assert!(env_blocking_offload([("PATH".to_string(), "/x".to_string())]).is_none());
        for key in ["CARGO_BUILD_TARGET_DIR", "CARGO_BUILD_BUILD_DIR"] {
            match decide_offload(&write(good), &args, [(key.to_string(), "/outside-cache".to_string())]) {
                Offload::Local(Some(why)) => assert!(why.contains(key), "{why}"),
                _ => panic!("{key} must keep the invocation local"),
            }
            assert!(matches!(decide_offload(&write(good), &args, [(key.to_string(), String::new())]), Offload::Go(_)), "empty {key} must allow offloading");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// config.toml 是人（或有寫檔權限的 agent）手改得到的，不只有 API 寫得進來：API 擋了的 host／user／remote_root 在這裡也要擋，
    /// 不然 `host = "-oProxyCommand=…"`、`user = "-o…"` 會變成 ssh 的選項、`remote_root` 的 `$(…)` 會被 rsync 的遠端 shell 展開。
    /// 擋下來就退回本機並講原因（跟設定檔壞掉同一條路）。
    #[test]
    fn a_hand_edited_config_with_option_or_shell_metacharacters_never_reaches_ssh() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-decide-bad-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let args = vec!["test".to_string()];
        let decide = |host: &str, user: &str, root: &str, port: u32| {
            let p = dir.join("c.toml");
            let body = format!("[build.remote]\nenabled = true\nhost = {host:?}\nuser = {user:?}\nremote_root = {root:?}\nssh_port = {port}\n");
            std::fs::write(&p, body).unwrap();
            decide_offload(&p, &args, Vec::<(String, String)>::new())
        };
        assert!(matches!(decide("h.example", "u", "/srv/cargo", 22), Offload::Go(_)), "正常的照轉");
        assert!(matches!(decide("2001:db8::1", "me.x-y_z", ".cache/a/b", 2222), Offload::Go(_)), "IPv6、相對路徑也正常");
        for (host, user, root, port) in [
            ("-oProxyCommand=touch /tmp/pwn", "u", "/r", 22),
            ("h;id", "u", "/r", 22),
            ("h h", "u", "/r", 22),
            ("h\nid", "u", "/r", 22),
            ("h", "-oProxyCommand=touch /tmp/pwn", "/r", 22),
            ("h", "u;id", "/r", 22),
            ("h", "u", "/a/../b", 22),
            ("h", "u", "/a b", 22),
            ("h", "u", "$(touch /tmp/pwn)", 22),
            ("h", "u", "/a'b", 22),
            ("h", "u", "/a`id`", 22),
            ("h", "u", "-rf", 22),
        ] {
            match decide(host, user, root, port) {
                Offload::Local(Some(why)) => assert!(why.contains("不合法") || why.contains("不支援"), "{host:?} {user:?} {root:?}：{why}"),
                _ => panic!("{host:?} {user:?} {root:?} 不能轉到 ssh"),
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `sh_quote("")` 以前回空字串：`cargo test -- ''`（空的過濾字串）送到遠端少一個引數，跟本機不是同一個意思。
    #[test]
    fn an_empty_argument_survives_quoting() {
        assert_eq!(sh_quote(""), "''");
        let s = run_script("/r/0123456789abcdef/shared", "", &LeaseToken::new(), 1, 0, &["test".into(), "--".into(), String::new(), "a b".into()]);
        assert!(s.ends_with(" test -- '' 'a b'"), "{s}");
    }

    use super::*;

    fn remote() -> BuildRemoteCfg {
        BuildRemoteCfg { host: "build-host".into(), user: "me".into(), ssh_port: 2222, ..Default::default() }
    }

    fn envs(cmd: &Command) -> std::collections::HashMap<String, String> {
        cmd.get_envs()
            .filter_map(|(k, v)| Some((k.to_string_lossy().into_owned(), v?.to_string_lossy().into_owned())))
            .collect()
    }

    /// 沒有 `sshpass` 也要連得上：改用 ssh 自己的 askpass（OpenSSH 8.4+），密碼只走環境變數。
    /// 2026-09-18：這台機器沒有 sshpass（homebrew core 沒有這個 formula），一按「測試」就被擋。
    #[test]
    fn without_sshpass_the_password_goes_through_ssh_askpass() {
        let helper = PathBuf::from("/d/remote-cargo-askpass.sh");
        let cmd = ssh_cmd(&remote(), Some("hunter2"), Some(&PwMode::Askpass(helper.clone())));
        assert_eq!(cmd.get_program(), "ssh");
        let e = envs(&cmd);
        assert_eq!(e.get("SSH_ASKPASS").map(String::as_str), Some("/d/remote-cargo-askpass.sh"));
        assert_eq!(e.get("SSH_ASKPASS_REQUIRE").map(String::as_str), Some("force"));
        assert_eq!(e.get("AM_SSH_PASSWORD").map(String::as_str), Some("hunter2"));
        let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(args.iter().any(|a| a == "NumberOfPasswordPrompts=1"), "密碼錯要當場失敗，不能卡在提示上：{args:?}");
        // 密碼不進 argv——`ps` 看得到別人的 argv。
        assert!(!args.iter().any(|a| a.contains("hunter2")), "{args:?}");

        // 有 sshpass 的機器照舊。
        let cmd = ssh_cmd(&remote(), Some("hunter2"), Some(&PwMode::Sshpass));
        assert_eq!(cmd.get_program(), "sshpass");
        assert_eq!(envs(&cmd).get("SSHPASS").map(String::as_str), Some("hunter2"));

        // 沒有密碼的一律不碰這些環境變數。
        let cmd = ssh_cmd(&remote(), None, None);
        assert_eq!(cmd.get_program(), "ssh");
        assert!(envs(&cmd).is_empty(), "{:?}", envs(&cmd));
    }

    /// #177：`cd daemon && cargo check` 到遠端時，要同步的是**工作區根**（有根的 Cargo.lock／[profile]／.cargo/config.toml），
    /// 再在同一個子目錄下跑。以前一律同步 cwd 本身：遠端沒有鎖檔、重新解析依賴，驗的不是本機那份。
    #[test]
    fn a_subdirectory_call_syncs_the_workspace_root_and_runs_in_the_same_subdirectory() {
        // macOS 的 TMPDIR 在 /var/folders/…（/var → /private/var）。source_root 回的是 canonical 路徑，
        // 所以測試的基底也要先 canonicalize，兩邊才是同一種寫法；否則就只有 macOS 紅。
        let t = crate::testing::track(std::env::temp_dir().canonicalize().unwrap().join(format!("am-r177-{}", crate::db::ulid())));
        let mk = |rel: &str, manifest: Option<&str>| {
            let d = t.join(rel);
            std::fs::create_dir_all(&d).unwrap();
            if let Some(m) = manifest {
                std::fs::write(d.join("Cargo.toml"), m).unwrap();
            }
            d
        };
        let pkg = "[package]\nname = \"p\"\nversion = \"0.1.0\"\n";
        let ws = mk("ws", Some("[workspace]\nmembers = [\"daemon\"]\n"));
        let daemon = mk("ws/daemon", Some(pkg));
        let src = mk("ws/daemon/src", None);
        let solo = mk("solo", Some(pkg));
        let solo_src = mk("solo/src", None);
        let bare = mk("bare/x", None);
        // 自己就是另一個工作區根的目錄（不歸外層管）：它自己當根。
        let inner = mk("ws/tools", Some("[workspace]\n"));
        let inner_src = mk("ws/tools/src", None);
        // manifest 壞掉的目錄不算工作區根，也不能讓 helper 出事。
        let broken = mk("broken", Some("[workspace\n"));

        assert_eq!(source_root(&ws), (ws.clone(), String::new()), "就在根：不變");
        assert_eq!(source_root(&daemon), (ws.clone(), "daemon".into()), "成員目錄：往上找到工作區根");
        assert_eq!(source_root(&src), (ws.clone(), "daemon/src".into()), "更深的子目錄也一樣");
        assert_eq!(source_root(&solo), (solo.clone(), String::new()));
        assert_eq!(source_root(&solo_src), (solo.clone(), "src".into()), "沒有工作區的單一套件：套件根");
        assert_eq!(source_root(&bare), (bare.clone(), String::new()), "不在 cargo 專案裡：cwd 本身，跟以前一樣");
        assert_eq!(source_root(&inner_src), (inner.clone(), "src".into()), "有自己 [workspace] 的算自己的根");
        assert_eq!(source_root(&broken), (broken.clone(), String::new()), "壞 manifest：當成一般套件根");
        #[cfg(unix)]
        {
            let member_link = t.join("member-link");
            std::os::unix::fs::symlink(&daemon, &member_link).unwrap();
            assert_eq!(source_root(&member_link), (ws.clone(), "daemon".into()), "symlink 成員也必須同步 workspace 根");
        }
        let _ = std::fs::remove_dir_all(t);
    }

    /// #174：連線靜默斷掉（Wi-Fi 換 AP、Mac 睡著醒來 IP 變了、遠端掉電）時，ssh 沒有 ServerAlive 就只能等 TCP keepalive
    /// （預設 2 小時）——helper 不返回、agent 的 cargo 卡死，遠端的鎖與目錄也一直被佔。沒有 ConnectTimeout，連上就不說話的對端也
    /// 讓 ssh 永遠停在握手。ssh 那條（守門／run／probe／install）與 rsync 的 `-e` 都要帶，有沒有密碼都一樣。
    #[test]
    fn every_ssh_of_the_offload_gives_up_on_a_silent_peer() {
        fn has(args: &[String], opt: &str) -> bool {
            args.windows(2).any(|w| w[0] == "-o" && w[1].starts_with(opt))
        }
        let ssh_args = |password: Option<&str>, mode: Option<&PwMode>| -> Vec<String> {
            ssh_cmd(&remote(), password, mode).get_args().map(|a| a.to_string_lossy().into_owned()).collect()
        };
        let variants = [
            ssh_args(None, None),
            ssh_args(Some("pw"), Some(&PwMode::Sshpass)),
            ssh_args(Some("pw"), Some(&PwMode::Askpass(PathBuf::from("/d/askpass.sh")))),
        ];
        for args in &variants {
            for opt in ["ConnectTimeout=", "ServerAliveInterval=", "ServerAliveCountMax="] {
                assert!(has(args, opt), "ssh 少了 {opt}：{args:?}");
            }
        }
        for (password, askpass) in [(false, false), (true, false), (true, true)] {
            let rsh = rsync_rsh(&remote(), password, askpass);
            for opt in ["ConnectTimeout=", "ServerAliveInterval=", "ServerAliveCountMax="] {
                assert!(rsh.contains(&format!("-o {opt}")), "rsync 的 -e 少了 {opt}：{rsh}");
            }
            assert!(rsh.starts_with("ssh -p 2222 "), "{rsh}");
        }
    }

    /// issue #104：密碼留空＝key／agent。沒有 sshpass／askpass、BatchMode=yes；有 `identity_file` 時 ssh 與 rsync 都帶同一把 `-i`；找不到檔案要明確報錯。
    #[test]
    fn a_blank_password_uses_key_auth_and_identity_file_reaches_ssh_and_rsync() {
        let mut r = remote();
        r.identity_file = "/keys/my key".into();
        let cmd = ssh_cmd(&r, None, None);
        let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(cmd.get_program(), "ssh");
        assert!(envs(&cmd).is_empty(), "空密碼不該有 SSHPASS／askpass：{:?}", envs(&cmd));
        assert!(args.contains(&"BatchMode=yes".to_string()), "{args:?}");
        let i = args.iter().position(|a| a == "-i").expect("少了 -i");
        assert_eq!(args[i + 1], "/keys/my key");
        assert!(args.contains(&"IdentitiesOnly=yes".to_string()));
        let rsh = rsync_rsh(&r, false, false);
        assert!(rsh.contains("BatchMode=yes") && rsh.contains("-i '/keys/my key' -o IdentitiesOnly=yes"), "{rsh}");
        assert!(!rsync_rsh(&remote(), false, false).contains("-i "), "沒設就不能帶 -i");
        let err = check_identity(&r).unwrap_err().to_string();
        assert!(err.contains("identity_file") && err.contains("/keys/my key"), "{err}");
        assert!(check_identity(&remote()).is_ok());
    }

    /// issue #194：整體上限預設 12 分鐘、可設定；舊的 config.toml 沒寫這個 key 也是 12 分鐘；`0`＝不設上限。
    #[test]
    fn the_overall_limit_defaults_to_twelve_minutes_and_is_configurable() {
        assert_eq!(BuildRemoteCfg::default().timeout_secs, 720);
        let old: BuildRemoteCfg = toml::from_str("enabled = true\nhost = \"h\"\nuser = \"u\"\n").unwrap();
        assert_eq!(old.timeout_secs, 720, "舊設定沒有這個 key：用預設");
        let set: BuildRemoteCfg = toml::from_str("timeout_secs = 90\n").unwrap();
        assert_eq!(set.timeout_secs, 90);
        assert_eq!(limit_text(720), "12 分鐘");
        assert_eq!(limit_text(90), "90 秒");
        let unlimited = Deadline::new(0);
        unlimited.arm();
        assert!(unlimited.stop().is_none(), "0＝不設上限");
        // issue #104：上限從拿到名額與目錄（arm）才開始算，排隊的時間不算。
        let d = Deadline::new(1);
        assert!(d.limit.is_some() && d.at.get().is_none(), "還沒 arm：不計時");
        d.arm();
        assert!(d.at.get().is_some());
    }

    /// 子行程已經成功結束、但它留下的孫行程還握著 stderr pipe（ssh 的背景 master、ProxyCommand）：收尾不能等到孫行程自己退出。
    #[test]
    fn a_grandchild_holding_stderr_does_not_stall_a_finished_child() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut cmd = Command::new("/bin/sh");
            cmd.arg("-c").arg("echo oops >&2; sleep 12 & exit 0");
            let d = Deadline::new(0);
            d.arm();
            let _ = tx.send(run_status_tee_stderr(cmd, "t", &d));
        });
        let (status, seen) = rx
            .recv_timeout(std::time::Duration::from_secs(8))
            .expect("孫行程握著 pipe 時，已結束的子行程不能卡住收尾")
            .unwrap();
        assert!(status.success());
        assert!(seen.contains("oops"), "已經讀到的 stderr 要留下來：{seen:?}");
    }

    /// `ssh -V` 的版本要照數字比：字串比會說 `10.3` 比 `8.4` 小。
    #[test]
    fn the_openssh_version_gate_compares_numbers_not_strings() {
        assert!(openssh_at_least_8_4("10.3p1, LibreSSL 3.3.6"));
        assert!(openssh_at_least_8_4("9.0p1"));
        assert!(openssh_at_least_8_4("8.4p1"));
        assert!(!openssh_at_least_8_4("8.3p1"));
        assert!(!openssh_at_least_8_4("7.9p1"));
        assert!(!openssh_at_least_8_4("not a version"));
    }

    /// askpass 腳本要是 0700、內容正確，而且重複呼叫不會一直重寫。
    #[test]
    fn the_askpass_helper_is_written_once_and_is_not_world_readable() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-askpass-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let path = askpass_helper(&dir).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.starts_with("#!/bin/sh"), "{body}");
        assert!(body.contains("$AM_SSH_PASSWORD"), "{body}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let before = std::fs::metadata(&path).unwrap().modified().unwrap();
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o700);
            assert_eq!(askpass_helper(&dir).unwrap(), path);
            assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), before, "內容沒變就不要重寫");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 連得上但沒有 cargo：這不是錯誤，是「還沒裝工具鏈」，UI 要據此提示幫忙安裝。
    #[test]
    fn a_probe_without_cargo_is_reported_as_a_missing_toolchain() {
        let (os, arch, cargo, ver) = parse_probe("OS=Linux\nARCH=x86_64\nCARGO=\n");
        assert_eq!(os.as_deref(), Some("Linux"));
        assert_eq!(arch.as_deref(), Some("x86_64"));
        assert!(cargo.is_none(), "{cargo:?}");
        assert!(ver.is_none());

        let (_, _, cargo, ver) = parse_probe("OS=Linux\nARCH=x86_64\nCARGO=/home/ubuntu/.cargo/bin/cargo\ncargo 1.90.0 (abc 2026-08-01)\n");
        assert_eq!(cargo.as_deref(), Some("/home/ubuntu/.cargo/bin/cargo"));
        assert_eq!(ver.as_deref(), Some("cargo 1.90.0 (abc 2026-08-01)"));
    }

    /// rustup 只改 shell profile，而 ssh 的非互動 shell 不讀 profile——probe、安裝後的檢查與真正
    /// 的遠端 cargo 都要自己把 `~/.cargo/bin` 接到 PATH 前面，否則裝好了照樣說「沒有 cargo」。
    #[test]
    fn every_remote_command_puts_cargo_bin_on_the_path() {
        assert!(PROBE_SH.contains("$HOME/.cargo/bin:$PATH"), "{PROBE_SH}");
        assert!(INSTALL_SH.contains("$HOME/.cargo/bin:$PATH"), "{INSTALL_SH}");
        // 安裝是冪等的：已經有就只回版本，不會再跑一次 rustup-init。
        assert!(INSTALL_SH.contains("if command -v cargo >/dev/null 2>&1; then\nprintf 'ALREADY='"), "{INSTALL_SH}");
        // minimal（另外帶 clippy，issue #104），而且不去動使用者的 shell profile。
        assert!(INSTALL_SH.contains("--profile minimal --component clippy"), "{INSTALL_SH}");
        assert!(INSTALL_SH.contains("--no-modify-path"), "{INSTALL_SH}");
        // rustup 不裝 linker：沒有 cc 的機器要當場講，不要等到 cargo test 連結失敗才看到天書。
        assert!(INSTALL_SH.contains("CC_MISSING=1"), "{INSTALL_SH}");
    }

    /// #670：登入 shell 是 fish／tcsh 時，裸的 `VAR=value` 腳本直接被拒。外層只接受 `sh -c '…'`。
    #[test]
    fn macos_local_remote_command_is_parsed_by_sh_not_the_login_shell() {
        let fish = r#"cmd=$1
case "$cmd" in
  "sh -c "*) ;;
  *) echo fish-reject >&2; exit 3 ;;
esac
inner=$(python3 -c 'import shlex,sys; print(shlex.split(sys.argv[1])[2])' "$cmd")
exec /bin/sh -c "$inner"
"#;
        let run = |remote_arg: &str| {
            let out = Command::new("/bin/sh").arg("-c").arg(fish).arg("fish").arg(remote_arg).output().unwrap();
            (out.status.code().unwrap_or(99), String::from_utf8_lossy(&out.stderr).into_owned(), String::from_utf8_lossy(&out.stdout).into_owned())
        };
        let raw = run(PROBE_SH);
        assert_eq!(raw.0, 3, "沒包 sh -c 時 fish 要拒：{raw:?}");
        let wrapped = run(&login_shell_arg(PROBE_SH));
        assert_ne!(wrapped.0, 3, "包了 sh -c 不該被 fish 拒：{wrapped:?}");
        assert!(wrapped.2.contains("OS="), "{wrapped:?}");
        for script in [INSTALL_SH, &run_script("/r/job", "", &LeaseToken::new(), 1, 0, &["--version".into()])] {
            let (code, err, _) = run(&login_shell_arg(script));
            assert_ne!(code, 3, "fish 拒了：{err} {script}");
        }
    }

    /// 在 `sh` 裡真的跑遠端的腳本：`HOME` 是空沙盒、PATH 只有 `uname`，`cargo`／`rustup` 用 shell 函式假裝（不寫可執行檔，免得撞 ETXTBSY）。
    fn run_remote_sh(script: &str, fakes: &str) -> String {
        let sandbox = crate::testing::track(std::env::temp_dir().join(format!("am-probe-{}", crate::db::ulid())));
        std::fs::create_dir_all(sandbox.join("bin")).unwrap();
        let uname = String::from_utf8(Command::new("sh").arg("-c").arg("command -v uname").output().unwrap().stdout).unwrap();
        std::os::unix::fs::symlink(uname.trim(), sandbox.join("bin/uname")).unwrap();
        // PATH 換掉之後 `sh` 也照新的 PATH 找：用絕對路徑。
        let out = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("{fakes}\n{script}"))
            .env("HOME", &sandbox)
            .env("PATH", sandbox.join("bin"))
            .env("AM_TEST_SANDBOX", &sandbox)
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&sandbox);
        String::from_utf8(out.stdout).unwrap()
    }

    /// issue #104：probe 要分得出「沒有 cargo」「有 cargo 沒 clippy」「都有」。clippy 是轉過去的三個指令之一，
    /// 有 cargo 沒 clippy 時要報 `clippy_missing`，不能說「可用」然後第一次 `cargo clippy` 才失敗。沒有 cargo 時 `CARGO=` 不能跟下一行黏在一起。
    #[test]
    fn the_probe_tells_a_missing_clippy_apart_from_a_missing_cargo() {
        let none = run_remote_sh(PROBE_SH, "");
        assert!(none.contains("\nCARGO=\n"), "{none}");
        let (_, _, cargo, _) = parse_probe(&none);
        assert_eq!((cargo, probe_clippy(&none)), (None, None), "{none}");

        let no_clippy = run_remote_sh(PROBE_SH, "cargo() { case \"$1\" in --version) echo 'cargo 1.90.0' ;; *) return 1 ;; esac; }");
        let (_, _, cargo, version) = parse_probe(&no_clippy);
        assert_eq!((cargo.as_deref(), version.as_deref(), probe_clippy(&no_clippy)), (Some("cargo"), Some("cargo 1.90.0"), None), "{no_clippy}");

        let both = run_remote_sh(
            PROBE_SH,
            "cargo() { case \"$1\" in --version) echo 'cargo 1.90.0' ;; clippy) echo 'clippy 0.1.90 (abc 2026-08-01)' ;; esac; }",
        );
        assert_eq!(probe_clippy(&both).as_deref(), Some("clippy 0.1.90 (abc 2026-08-01)"), "{both}");
    }

    /// issue #104：本來就有 cargo（只是 minimal、沒有 clippy）的機器按「安裝」要補上 clippy；補不上（不是 rustup 裝的）就回報 `CLIPPY_MISSING=1`。
    #[test]
    fn installing_on_a_host_that_already_has_cargo_adds_clippy() {
        // `rustup component add clippy` 會真的讓 `cargo clippy` 變成可用。
        let cargo = "cargo() { case \"$1\" in --version) echo 'cargo 1.90.0' ;; clippy) [ -e \"$AM_TEST_SANDBOX/clippy\" ] && echo 'clippy 0.1.90' ;; esac; }";
        let rustup = "rustup() { [ \"$*\" = 'component add clippy' ] && : > \"$AM_TEST_SANDBOX/clippy\"; }";
        let out = run_remote_sh(INSTALL_SH, &format!("{cargo}\n{rustup}"));
        assert!(out.contains("ALREADY=cargo 1.90.0"), "{out}");
        assert!(!out.contains("CLIPPY_MISSING"), "rustup 補上了 clippy：{out}");

        let out = run_remote_sh(INSTALL_SH, cargo);
        assert!(out.contains("CLIPPY_MISSING=1"), "沒有 rustup 補不上：要講出來：{out}");
    }

    #[test]
    fn only_cross_platform_verification_commands_are_offloaded() {
        for cmd in ["c", "check", "t", "test", "clippy"] {
            assert!(eligible(&[cmd.into()]), "{cmd}");
        }
        for cmd in ["build", "run", "bench", "doc", "install", "metadata"] {
            assert!(!eligible(&[cmd.into()]), "{cmd}");
        }
        // issue #195：子指令前面的全域旗標。搬得走的（原樣搬過去意思不變）照樣轉；`+toolchain`／本機路徑／取決於本機快取的留在本機。
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for ok in [&["--locked", "test"][..], &["-q", "check"], &["--color", "always", "clippy"], &["--color=never", "t"], &["-v", "--locked", "test", "--", "x"]] {
            assert!(eligible(&v(ok)), "{ok:?}");
        }
        for no in [&["+nightly", "test"][..], &["-C", "x", "test"], &["--offline", "test"], &["--frozen", "check"], &["-Z", "unstable-options", "check"], &["--config", "a=b", "test"], &["--locked", "build"], &["--locked"], &[]] {
            assert!(!eligible(&v(no)), "{no:?}");
        }
        // issue #104：`clippy --fix` 會改原始碼——改到的是遠端那份、不會同步回來，所以留在本機跑。`--` 後面是給 clippy-driver 的，不算。
        for local in [&["clippy", "--fix"][..], &["clippy", "--all-targets", "--fix", "--allow-dirty"], &["-q", "clippy", "--fix", "--", "-D", "warnings"]] {
            assert!(!eligible(&v(local)), "{local:?}");
        }
        for remote in [&["clippy", "--all-targets", "-p", "x"][..], &["clippy", "--", "-D", "warnings"], &["clippy", "--", "--fix"]] {
            assert!(eligible(&v(remote)), "{remote:?}");
        }
        for local in [
            &["check", "--target-dir", "/other/cache"][..],
            &["test", "--target-dir=/other/cache"],
            &["clippy", "--config", "build.target-dir='elsewhere'"],
            &["check", "--config=build.build-dir='elsewhere'"],
        ] {
            assert!(!eligible(&v(local)), "custom cache directories must remain local: {local:?}");
        }
        assert!(eligible(&v(&["test", "--", "--target-dir=filter"])));
        for remote in [
            &["check", "--release"][..],
            &["test", "--profile", "release"],
            &["clippy", "--profile=release"],
        ] {
            assert!(eligible(&v(remote)), "release checks are safe because all source inputs are synced: {remote:?}");
        }
    }

    #[test]
    fn shell_quoting_does_not_allow_argument_injection() {
        assert_eq!(sh_quote("abc/def"), "abc/def");
        assert_eq!(sh_quote("x; touch /tmp/pwn"), "'x; touch /tmp/pwn'");
        assert_eq!(sh_quote("a'b"), "'a'\\''b'");
    }

    const ROOT: &str = "/home/u/.cache/agents-manager/remote-cargo";

    fn rdir(leaf: &str, locked: bool, busy: bool, idle_secs: u64) -> RemoteDir {
        RemoteDir { path: format!("{ROOT}/{leaf}"), locked, busy, idle_secs }
    }

    fn hs(dirs: Vec<RemoteDir>, disk: Option<(u64, u64)>) -> Handshake {
        Handshake {
            root: ROOT.into(),
            dir: format!("{ROOT}/0123456789abcdef/shared"),
            token: "0123456789abcdef0123456789abcdef".into(),
            dirs,
            disk,
            slot: None,
        }
    }

    /// #141：孤兒（沒鎖、沒行程、放超過 10 分鐘）要收；有人在用的——鎖被持有、有行程 cwd 在裡面——
    /// 放多久都不能碰；自己這次的目錄也不能碰。
    #[test]
    fn gc_picks_idle_orphans_and_never_touches_a_directory_in_use() {
        let old = ORPHAN_IDLE_SECS + 1;
        let h = hs(
            vec![
                rdir("00000000000000aa/111", false, false, old),         // 舊版留下的 <pid>，閒置 → 收
                rdir("00000000000000aa/job-9-1", false, false, old),     // 守門死在遠端的 job → 收
                rdir("00000000000000aa/222", false, true, old * 50),     // 有行程在裡面 → 不收
                rdir("00000000000000aa/job-9-2", true, false, old * 50), // 鎖被持有 → 不收
                rdir("00000000000000aa/333", false, false, ORPHAN_IDLE_SECS - 1), // 剛動過 → 不收
                rdir("0123456789abcdef/shared", false, false, old * 50), // 自己這次的 → 不收
            ],
            Some((80, 100)),
        );
        assert_eq!(
            select_gc(&h, &GcPolicy::default()),
            vec![format!("{ROOT}/00000000000000aa/111"), format!("{ROOT}/00000000000000aa/job-9-1")]
        );
    }

    /// `shared/` 是快取：平常閒置 3 小時才收；磁碟剩不到 25% 時照孤兒的 10 分鐘收（#141 塞滿 97G）。
    #[test]
    fn a_shared_target_is_kept_as_cache_unless_the_disk_is_running_out() {
        let dirs = vec![
            rdir("00000000000000aa/shared", false, false, ORPHAN_IDLE_SECS + 1),
            rdir("00000000000000bb/shared", false, false, SHARED_IDLE_SECS + 1),
            rdir("00000000000000cc/shared", true, false, SHARED_IDLE_SECS * 10),
        ];
        let roomy = select_gc(&hs(dirs.clone(), Some((50, 100))), &GcPolicy::default());
        assert_eq!(roomy, vec![format!("{ROOT}/00000000000000bb/shared")]);
        let full = select_gc(&hs(dirs, Some((10, 100))), &GcPolicy::default());
        assert_eq!(
            full,
            vec![format!("{ROOT}/00000000000000aa/shared"), format!("{ROOT}/00000000000000bb/shared")],
            "磁碟緊時，被持有的 shared 還是不能收"
        );
    }

    /// issue #196：只靠「閒置 3 小時」擋不住——一天開十幾顆子 agent、每張票數個變異副本，每個路徑一個新的 hash、各 2～3G（實測 36 個 hash、29G）。
    /// 閒置時間可設定；另外  最多留 N 份，超過就從最久沒用的開始收（LRU）。鎖被持有、有行程在用、這次自己的不算「可收」，
    /// 而且剛用完的（不到 10 分鐘）不動——不然一次跑完的變異副本會被下一個呼叫立刻收走、白白冷編譯。
    #[test]
    fn shared_targets_are_reclaimed_by_a_configurable_age_and_by_a_count_cap_oldest_first() {
        let h = |n: &str| format!("{ROOT}/00000000000000{n}/shared");
        let dirs = vec![
            rdir("00000000000000a1/shared", false, false, 7 * 3600), // 7 小時：超過 6 小時 → 依時間收
            rdir("00000000000000a2/shared", false, false, 5 * 3600),
            rdir("00000000000000a3/shared", false, false, 4 * 3600),
            rdir("00000000000000a4/shared", false, false, 3 * 3600),
            rdir("00000000000000a5/shared", false, false, 2 * 3600),
            rdir("00000000000000aa/shared", false, false, 3600),
            rdir("00000000000000a6/shared", false, false, 30 * 60),
            rdir("00000000000000a7/shared", false, false, 5 * 60), // 剛用完：就算超過數量上限也不動
            rdir("00000000000000a8/shared", true, false, 20 * 3600), // 鎖被持有：不動
            rdir("00000000000000a9/shared", false, true, 20 * 3600), // 有行程在裡面：不動
        ];
        let pick = |policy: &GcPolicy| {
            let mut got = select_gc(&hs(dirs.clone(), Some((80, 100))), policy);
            got.sort();
            got
        };
        let names = |ns: &[&str]| ns.iter().map(|n| h(n)).collect::<Vec<_>>();
        // 共 10 份＋這次自己的一份。數量不限：只依時間收超過 6 小時的。
        assert_eq!(pick(&GcPolicy { shared_idle_secs: 6 * 3600, max_shared: 0 }), names(&["a1"]));
        // 上限 10：依時間收掉 a1 之後剩 10 份，剛好在上限內。
        assert_eq!(pick(&GcPolicy { shared_idle_secs: 6 * 3600, max_shared: 10 }), names(&["a1"]));
        // 上限 6：還要再收 4 份——**最久沒用的先收**（a2、a3、a4、a5），不是最新的。
        assert_eq!(pick(&GcPolicy { shared_idle_secs: 6 * 3600, max_shared: 6 }), names(&["a1", "a2", "a3", "a4", "a5"]));
        // 上限 1：能收的都收（a2～a6、aa），但不碰鎖住的（a8）、有行程的（a9）、**剛用完的（a7）**、也不碰自己——留下的比上限多也沒關係。
        assert_eq!(pick(&GcPolicy { shared_idle_secs: 6 * 3600, max_shared: 1 }), names(&["a1", "a2", "a3", "a4", "a5", "a6", "aa"]));
    }

    /// 政策從設定來：閒置小時數至少算 1（0 不能變成「馬上收光」），預設 3 小時、最多 12 份（issue #417），舊設定沒寫這兩個 key 也是預設。
    #[test]
    fn the_gc_policy_comes_from_the_remote_settings() {
        assert_eq!(GcPolicy::default(), GcPolicy { shared_idle_secs: 3 * 3600, max_shared: 12 });
        let cfg = BuildRemoteCfg { shared_idle_hours: 0, max_shared_dirs: 3, ..Default::default() };
        assert_eq!(GcPolicy::from_cfg(&cfg), GcPolicy { shared_idle_secs: 3600, max_shared: 3 });
        let old: BuildRemoteCfg = toml::from_str("host = \"h\"\n").unwrap();
        assert_eq!(GcPolicy::from_cfg(&old), GcPolicy::default());
    }

    /// remote_root 設錯（例如設成家目錄）也只碰自己命名規則的目錄。
    #[test]
    fn gc_only_ever_names_directories_in_its_own_layout() {
        let old = SHARED_IDLE_SECS * 10;
        let foreign = [
            "/etc/00000000000000aa/111".to_string(),
            format!("{ROOT}-other/00000000000000aa/111"),
            format!("{ROOT}/nothex/111"),
            format!("{ROOT}/00000000000000AA/111"),
            format!("{ROOT}/00000000000000aa/src"),
            format!("{ROOT}/00000000000000aa/12-3"),
            format!("{ROOT}/00000000000000aa/job-"),
            format!("{ROOT}/00000000000000aa/111/nested"),
            format!("{ROOT}/00000000000000aa/.."),
        ];
        let dirs = foreign
            .iter()
            .map(|p| RemoteDir { path: p.clone(), locked: false, busy: false, idle_secs: old })
            .collect();
        assert!(select_gc(&hs(dirs, Some((1, 100))), &GcPolicy::default()).is_empty());
        assert_eq!(leaf_kind(ROOT, &format!("{ROOT}/00000000000000aa/shared")), Some(LeafKind::Shared));
        assert_eq!(leaf_kind(ROOT, &format!("{ROOT}/00000000000000aa/job-12-34")), Some(LeafKind::Job));
        assert_eq!(leaf_kind(ROOT, &format!("{ROOT}/00000000000000aa/4711")), Some(LeafKind::Job));
    }

    #[test]
    fn the_handshake_skips_login_noise_and_fails_if_the_guard_dies_first() {
        // 排隊時的心跳（`Q`）也是雜訊，略過。
        let out = "Welcome!\nQ\nQ\nROOT\t/r\nDIR\t/r/0123456789abcdef/shared\tfeedfacefeedfacefeedfacefeedface\nSLOT\t3\t6\n\
                   D\t/r/00000000000000aa/111\t0\t1\t-3\nD\t/r/00000000000000aa/222\t1\t0\t900\n\
                   F\t500\t1000\nEND\nD\t/late\t0\t0\t9\n";
        let h = read_handshake(&mut std::io::Cursor::new(out)).unwrap();
        assert_eq!(h.slot, Some((3, 6)));
        assert_eq!(h.root, "/r");
        assert_eq!(h.dir, "/r/0123456789abcdef/shared");
        assert_eq!(h.token, "feedfacefeedfacefeedfacefeedface");
        assert_eq!(h.disk, Some((500, 1000)));
        assert_eq!(
            h.dirs,
            vec![
                RemoteDir { path: "/r/00000000000000aa/111".into(), locked: false, busy: true, idle_secs: 0 },
                RemoteDir { path: "/r/00000000000000aa/222".into(), locked: true, busy: false, idle_secs: 900 },
            ]
        );
        // 守門在交出目錄前就死了（沒 flock、磁碟滿…）：不能當成拿到目錄。
        assert!(read_handshake(&mut std::io::Cursor::new("ROOT\t/r\nEND\n")).is_err());
        assert!(read_handshake(&mut std::io::Cursor::new("")).is_err());
    }

    /// run 要先記 pgid、再核 owner、最後才跑 cargo：守門清理是「先撤 owner 再讀 pgid」，順序反了會漏收。
    #[test]
    fn the_remote_run_records_its_group_before_checking_the_lease() {
        let tok = LeaseToken::parse("0123456789abcdef0123456789abcdef").unwrap();
        let s = run_script("/r/0123456789abcdef/shared", "", &tok, 4, 8, &["test".into(), "-p".into(), "x; rm -rf ~".into()]);
        let pgid = s.find("> \"$d.pgid-0123456789abcdef0123456789abcdef\"").expect(&s);
        let owner = s.find("\"$d.owner\"").expect(&s);
        let cargo = s.find("cargo --config").expect(&s);
        assert!(pgid < owner && owner < cargo, "{s}");
        assert!(s.contains("exit 126"), "{s}");
        assert!(s.contains("'x; rm -rf ~'"), "{s}");
        assert!(s.contains("PATH=\"$HOME/.cargo/bin:$PATH\" CARGO_BUILD_JOBS=4 RUST_TEST_THREADS=8 env -u CARGO_TARGET_DIR -u CARGO_BUILD_TARGET_DIR -u CARGO_BUILD_BUILD_DIR cargo --config"), "{s}");
        assert!(s.contains("build.target-dir=\"/r/0123456789abcdef/shared/target\""), "{s}");
        assert!(s.contains("build.build-dir=\"/r/0123456789abcdef/shared/target\""), "{s}");
        assert!(!s.contains("CARGO_TARGET_DIR="), "nested Cargo calls must not inherit this cache: {s}");
        assert!(s.contains("env -u CARGO_TARGET_DIR -u CARGO_BUILD_TARGET_DIR -u CARGO_BUILD_BUILD_DIR"), "remote cache environment must not escape the lease: {s}");
        // 0＝不設：不帶這個環境變數（libtest 用自己的預設）。
        let s = run_script("/r/0123456789abcdef/shared", "", &tok, 4, 0, &["test".into()]);
        assert!(!s.contains("RUST_TEST_THREADS"), "{s}");
    }

    /// issue #202：遠端是 32 vCPU 的超賣主機，測試預設開 32 個執行緒反而更慢（283 秒 vs 限 8 個 183 秒）。預設 8、可設定；呼叫端自己設了
    /// `RUST_TEST_THREADS` 就尊重它（`--test-threads` 命令列旗標本來就優先於環境變數）；`0`＝不設。
    #[test]
    fn the_remote_test_thread_cap_defaults_to_eight_and_yields_to_the_caller() {
        assert_eq!(BuildRemoteCfg::default().test_threads, 8);
        let old: BuildRemoteCfg = toml::from_str("host = \"h\"\n").unwrap();
        assert_eq!(old.test_threads, 8, "舊設定沒有這個 key：用預設");
        assert_eq!(pick_test_threads(None, 8), 8);
        assert_eq!(pick_test_threads(Some("4"), 8), 4, "呼叫端自己設的優先");
        assert_eq!(pick_test_threads(Some(" 16 "), 8), 16);
        assert_eq!(pick_test_threads(Some("0"), 8), 8, "0 或看不懂的不算數");
        assert_eq!(pick_test_threads(Some("many"), 8), 8);
        assert_eq!(pick_test_threads(None, 0), 0, "設定成 0＝不設");
    }

    /// #148：租約身份固定格式（32 個小寫 hex），嵌進遠端 shell 與檔名都不會變成 injection；
    /// 每次都是新的——不是 PID、不是時間，撞不到。
    #[test]
    fn a_lease_token_is_fixed_format_random_and_never_repeats() {
        let a = LeaseToken::new();
        assert_eq!(a.as_str().len(), 32);
        assert!(a.as_str().bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')), "{a:?}");
        assert_eq!(LeaseToken::parse(a.as_str()), Some(a.clone()));
        let seen: std::collections::HashSet<String> = (0..2000).map(|_| LeaseToken::new().as_str().to_string()).collect();
        assert_eq!(seen.len(), 2000, "隨機 token 不能重複");

        for bad in [
            "",
            "4242",                                     // 舊版的 `$$`
            "0123456789ABCDEF0123456789ABCDEF",         // 大寫
            "0123456789abcdef0123456789abcde",          // 少一個
            "0123456789abcdef0123456789abcdef0",        // 多一個
            "0123456789abcdef0123456789abcde;",         // shell 字元
            "../../../../../../../../../../etc/passwd", // 路徑穿越
            "0123456789abcdef0123456789abcd\n",         // 換行
            "0123456789abcdef0123456789abcd'x",         // 引號
        ] {
            assert!(LeaseToken::parse(bad).is_none(), "{bad:?}");
        }
        let script = guard_script("/r", "0123456789abcdef", "job-1-1", &a, Admission::from_cfg(&BuildRemoteCfg::default()));
        assert!(script.contains(&format!("token={}", a.as_str())), "token 要原樣、不帶引號地進腳本");
        assert!(!script.contains("@TOKEN@"), "沒替換乾淨");
    }

    /// 守門交回來的 token 不是我們給的那個＝出事了，不能當成拿到目錄（不然後面 run 會拿錯身份去核 owner）。
    #[test]
    fn a_guard_that_answers_with_another_token_is_not_a_lease() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("printf 'ROOT\\t/r\\nDIR\\t/r/0123456789abcdef/shared\\tfeedfacefeedfacefeedfacefeedface\\nEND\\n'; cat >/dev/null");
        let err = Lease::start(cmd, LeaseToken::new(), &Deadline::new(0)).err().expect("token mismatch must fail").to_string();
        assert!(err.contains("lease token"), "{err}");
    }

    /// #1971：`@LOADAVG@` 代入正式路徑（`/proc/loadavg`，經 `sh_quote` 不加引號）後，守門腳本跟改動前逐 byte 相同。
    /// 基準是改動前用同一組輸入組出的 sha256（試用與排隊各一份）；腳本變了就要先確認是不是這裡的預期。
    #[test]
    fn the_production_guard_script_is_byte_identical_to_before_the_loadavg_placeholder() {
        use sha2::Digest as _;
        let tok = LeaseToken::parse("0123456789abcdef0123456789abcdef").unwrap();
        let trial = Admission { max: 3, jobs: 5, shared_wait_secs: 900, queue_wait_secs: 3, pool: 0, try_only: true, max_load: Some(12.5) };
        let queued = Admission { max: 0, jobs: 12, shared_wait_secs: 900, queue_wait_secs: 3, pool: 0, try_only: false, max_load: None };
        for (adm, want) in [
            (trial, "06a8fec910633d9586672591a0c6394a863238ee308c54902db6308cba16f436"),
            (queued, "9d162e08a7680da36ed6bef9c35771ffeca974ada034cbee13feaece8012f71e"),
        ] {
            let s = guard_script("/r", "0123456789abcdef", "job-1-1", &tok, adm);
            let got: String = sha2::Sha256::digest(s.as_bytes()).iter().map(|x| format!("{x:02x}")).collect();
            assert_eq!(got, want, "守門腳本的 byte 變了");
            assert!(s.contains("awk -v m=\"$maxload\" '{ exit !($1 > m) }' /proc/loadavg 2>/dev/null"), "正式路徑要讀 /proc/loadavg");
            assert!(!s.contains("@LOADAVG@"), "沒替換乾淨");
        }
    }

    /// 守門腳本裡那條自動上限算式的 Rust 版（測試用的對照；遠端才知道自己的核數與 RAM，正式的只有腳本那份）。
    pub(super) fn auto_max_concurrent(ncpu: u64, mem_kb: u64, jobs: usize) -> usize {
        let by_cpu = ncpu * 3 / jobs.max(1) as u64;
        let by_mem = mem_kb.saturating_sub(AUTO_RESERVE_KB) / AUTO_PER_JOB_KB;
        by_cpu.min(by_mem).max(1) as usize
    }

    /// issue #104／#1956：`max_concurrent = 0`（預設）依遠端核數與 RAM 算——CPU 容許 3 倍超賣，記憶體每個編譯 14 GiB（`agents_managerd`
    /// 那顆 rustc 實測 13.5 GB）、留 8 GiB 給系統。本機（16 核／62 GiB）配 `cargo_jobs = 12` 是 3；小機器至少 1；設定寫了數字就用設定、最多 64。
    #[test]
    fn the_remote_cap_defaults_to_what_the_hosts_cores_and_ram_can_take() {
        assert_eq!(BuildRemoteCfg::default().max_concurrent, 0, "預設＝自動");
        let old: BuildRemoteCfg = toml::from_str("host = \"h\"\n").unwrap();
        assert_eq!(old.max_concurrent, 0, "舊設定沒有這個 key：自動");
        assert_eq!(auto_max_concurrent(16, 65_000_000, 12), 3, "本機：min(16×3÷12, (62−8)÷14) = min(4, 3)");
        assert_eq!(auto_max_concurrent(16, 65_000_000, 24), 2, "cargo_jobs 很大時換 CPU 卡住");
        assert_eq!(auto_max_concurrent(8, 15 * 1024 * 1024, 8), 1, "r7（15 GiB）：一次一個");
        assert_eq!(auto_max_concurrent(2, 4 * 1024 * 1024, 8), 1, "再小也至少 1");
        assert_eq!(auto_max_concurrent(64, 256 * 1024 * 1024, 4), 17);
        let set = BuildRemoteCfg { max_concurrent: 3, cargo_jobs: 5, ..Default::default() };
        let adm = Admission::from_cfg(&set);
        assert_eq!((adm.max, adm.jobs, adm.shared_wait_secs, adm.queue_wait_secs), (3, 5, SHARED_WAIT_SECS, QUEUE_WAIT_SECS));
        let script = guard_script("/r", "0123456789abcdef", "job-1-1", &LeaseToken::new(), adm);
        for want in [&format!("max=3; jobs=5; swait={SHARED_WAIT_SECS}; qwait=3"), &format!("(mem - {AUTO_RESERVE_KB}) / {AUTO_PER_JOB_KB}")] {
            assert!(script.contains(want), "{want}");
        }
        assert!(!script.contains('@'), "沒替換乾淨");
        // #1937：24 小時統計要 grep 的四種字樣（拿到 shared 並等了幾秒、shared 逾時、改用獨立目錄、轉本機）都在守門裡，彼此分得開。
        for want in [
            "等了 $hw 秒拿到共用遠端 target",
            "秒共用遠端 target 還沒空出來",
            "這次改用獨立目錄（冷編譯，結束就刪）",
            "這次改在本機排隊",
        ] {
            assert!(script.contains(want), "{want}");
        }
        assert!(!script.contains("拿到遠端名額"), "shared 鎖的字樣不能跟名額那層（「排到遠端名額」）混在一起");
    }

    /// issue #104：排隊時守門可能很久才交出目錄，這段時間收到終止訊號（Ctrl-C／TERM）也要馬上停，不能卡到排到名額為止。
    #[test]
    fn a_queued_handshake_still_honours_a_termination_signal() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("exec sleep 60");
        let ctl = Deadline::new(0);
        let abort = ctl.abort.clone();
        let flip = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            abort.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        let t0 = std::time::Instant::now();
        let err = Lease::start(cmd, LeaseToken::new(), &ctl).err().expect("interrupted");
        flip.join().unwrap();
        assert!(matches!(err.downcast_ref::<Stopped>(), Some(Stopped::Interrupted(_))), "{err:#}");
        assert!(t0.elapsed() < std::time::Duration::from_secs(50), "{:?}", t0.elapsed());
    }

    /// #323：cargo shim 被單獨殺掉（沒有人會再送訊號）時，helper 要發現父行程沒了、把遠端收乾淨，不能孤兒似地跑到 timeout。
    #[test]
    fn an_orphaned_helper_stops_when_its_parent_shim_is_gone() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("exec sleep 60");
        let mut ctl = Deadline::new(0);
        ctl.parent = Some((4242, || 1)); // 啟動時的父行程是 4242，現在變成 1（被 init 收養）
        let t0 = std::time::Instant::now();
        let err = Lease::start(cmd, LeaseToken::new(), &ctl).err().expect("interrupted");
        assert!(matches!(err.downcast_ref::<Stopped>(), Some(Stopped::Interrupted(_))), "{err:#}");
        assert!(t0.elapsed() < std::time::Duration::from_secs(50), "{:?}", t0.elapsed());
        let mut same = Deadline::new(0);
        same.parent = Some((7, || 7));
        assert!(same.stop().is_none(), "父行程沒變就照常跑");
    }

    fn secret_dir() -> PathBuf {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-remote-cargo-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// data-dir 裡的密碼檔（含暫存檔）：(檔名, 權限)。
    fn password_files(dir: &Path) -> Vec<(String, u32)> {
        use std::os::unix::fs::PermissionsExt as _;
        let mut v: Vec<(String, u32)> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(PASSWORD_FILE))
            .map(|n| {
                let mode = std::fs::metadata(dir.join(&n)).unwrap().permissions().mode() & 0o777;
                (n, mode)
            })
            .collect();
        v.sort();
        v
    }

    #[tokio::test]
    async fn password_file_is_private_and_metadata_does_not_echo_the_secret() {
        let dir = secret_dir();
        let store = crate::runners::app_ports_p2::load_config(dir.join("config.toml")).await.unwrap();
        let remote = commit_remote(&store, &dir, Some("super-secret"), |_| {}).await.unwrap();
        assert!(password_is_set(&dir, &remote));
        let files = password_files(&dir);
        assert_eq!(files.len(), 1, "{files:?}");
        assert_eq!(files[0].1, 0o600, "{files:?}");
        let v = sanitized(&remote, &dir);
        assert_eq!(v["password_set"], true);
        assert!(!v.to_string().contains("super-secret"));
        assert!(!std::fs::read_to_string(dir.join("config.toml")).unwrap().contains("super-secret"), "密碼不進 config.toml");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 舊版設定（沒有 `password_id`）照舊讀固定檔名；`None`＝沿用（換主機也一樣）、`Some("")`＝清掉、新密碼＝換一份新檔、舊檔刪掉。
    /// 設定明確指到一份密碼檔而它不見了是錯誤，不能悄悄改用 key/agent；手改壞的 id 不能變成路徑。
    #[tokio::test]
    async fn saving_settings_keeps_clears_or_replaces_the_password_as_asked() {
        let dir = secret_dir();
        let store = crate::runners::app_ports_p2::load_config(dir.join("config.toml")).await.unwrap();
        std::fs::write(dir.join(PASSWORD_FILE), "legacy-pw\n").unwrap();
        let legacy = store.get().await.build.remote;
        assert_eq!(legacy.password_id, None);
        assert_eq!(secret(&dir, &legacy).unwrap().as_deref(), Some("legacy-pw"), "舊版設定：讀固定檔名");

        let r = commit_remote(&store, &dir, None, |r| r.host = "b".into()).await.unwrap();
        assert_eq!((r.host.as_str(), secret(&dir, &r).unwrap().as_deref()), ("b", Some("legacy-pw")), "None＝沿用");
        let names: Vec<String> = password_files(&dir).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec![PASSWORD_FILE.to_string()], "沿用：舊版固定檔名留著");

        let r = commit_remote(&store, &dir, Some("pw2"), |_| {}).await.unwrap();
        let id = r.password_id.clone().expect("新密碼要有自己的 id");
        assert_eq!(secret(&dir, &r).unwrap().as_deref(), Some("pw2"));
        assert_eq!(password_files(&dir), vec![(format!("{PASSWORD_FILE}.{id}"), 0o600)], "舊版固定檔名要刪掉");

        let r = commit_remote(&store, &dir, None, |r| r.host = "c".into()).await.unwrap();
        assert_eq!((r.password_id.as_deref(), secret(&dir, &r).unwrap().as_deref()), (Some(id.as_str()), Some("pw2")));

        let r = commit_remote(&store, &dir, Some(""), |_| {}).await.unwrap();
        assert_eq!(r.password_id.as_deref(), Some(""));
        assert_eq!(secret(&dir, &r).unwrap(), None);
        assert!(!password_is_set(&dir, &r));
        assert!(password_files(&dir).is_empty(), "清掉＝一份密碼檔都不留：{:?}", password_files(&dir));

        let gone = BuildRemoteCfg { password_id: Some("0123456789abcdef".into()), ..Default::default() };
        let err = secret(&dir, &gone).unwrap_err().to_string();
        assert!(err.contains("不見了"), "{err}");
        for bad in ["../../etc/passwd", "0123456789ABCDEF", "123", "0123456789abcdef/"] {
            let r = BuildRemoteCfg { password_id: Some(bad.into()), ..Default::default() };
            assert!(secret(&dir, &r).is_err(), "{bad}");
            assert!(!password_is_set(&dir, &r), "{bad}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// issue #104：換設定＋密碼時，任何一步失敗、或行程死在任何一步之後，重讀磁碟只會是「舊主機＋舊密碼」或「新主機＋新密碼」，
    /// 而且 data-dir 裡任何時候都沒有權限比 0600 寬的密碼檔。
    /// 以前先改 config 再 `fs::write` 密碼、寫完才 chmod：寫密碼失敗＝新主機配舊密碼（舊密碼會被送去新主機）；
    /// write 與 chmod 之間 crash＝密碼檔永遠是 umask 決定的 0644。
    #[tokio::test]
    async fn a_failure_or_crash_at_any_step_leaves_the_old_or_the_new_pair_never_a_mix() {
        use std::os::unix::fs::PermissionsExt as _;
        for step in ["created", "written", "synced", "renamed", "config", "cleanup"] {
            for how in [Fault::Fail, Fault::Crash] {
                let dir = secret_dir();
                let cfg_path = dir.join("config.toml");
                let store = crate::runners::app_ports_p2::load_config(cfg_path.clone()).await.unwrap();
                store.update(|c| {
                    c.build.remote.host = "old-host".into();
                    Ok(())
                })
                .await
                .unwrap();
                let old_file = dir.join(PASSWORD_FILE);
                std::fs::write(&old_file, "old-pw").unwrap();
                std::fs::set_permissions(&old_file, std::fs::Permissions::from_mode(0o600)).unwrap();

                SECRET_FAULT.with(|f| f.set(Some((step, how))));
                let res = commit_remote(&store, &dir, Some("new-pw"), |r| r.host = "new-host".into()).await;
                SECRET_FAULT.with(|f| f.set(None));
                let why = format!("{step}/{how:?}: {res:?}");

                // 重開 daemon：只看磁碟上的東西。
                let fresh = crate::runners::app_ports_p2::load_config(cfg_path.clone()).await.unwrap().get().await.build.remote;
                let pair = (fresh.host.clone(), secret(&dir, &fresh).unwrap());
                if step == "cleanup" {
                    assert_eq!(pair, ("new-host".to_string(), Some("new-pw".to_string())), "已經提交：{why}");
                } else {
                    assert_eq!(pair, ("old-host".to_string(), Some("old-pw".to_string())), "還沒提交：{why}");
                }
                assert_eq!(res.is_ok(), step == "cleanup" && how == Fault::Fail, "提交之後清舊檔失敗不算失敗，其他都要回錯：{why}");
                let files = password_files(&dir);
                assert!(files.iter().all(|(_, mode)| *mode == 0o600), "每一份密碼檔（含暫存檔）都要是 0600：{why} {files:?}");
                if how == Fault::Fail && step != "cleanup" {
                    assert_eq!(files, vec![(PASSWORD_FILE.to_string(), 0o600)], "失敗要收乾淨，只剩舊檔：{why}");
                }

                // 下一次成功的儲存把殘骸（沒人指到的暫存檔、舊檔）收掉。
                let store = crate::runners::app_ports_p2::load_config(cfg_path.clone()).await.unwrap();
                let r = commit_remote(&store, &dir, Some("newer-pw"), |r| r.host = "newer-host".into()).await.unwrap();
                assert_eq!(secret(&dir, &r).unwrap().as_deref(), Some("newer-pw"), "{why}");
                let id = r.password_id.clone().unwrap();
                assert_eq!(password_files(&dir), vec![(format!("{PASSWORD_FILE}.{id}"), 0o600)], "{why}");
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }
}

/// 守門腳本的真實行為（#141）。要 flock 與 /proc，只在 Linux 跑——remote Cargo 本身就是把測試丟到
/// Linux 主機執行，所以這組在那台會真的跑到；不走 ssh，直接 `sh -c` 同一份腳本。
#[cfg(all(test, target_os = "linux"))]
mod guard_tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
    use std::time::{Duration, Instant};

    pub(super) const HASH: &str = "0123456789abcdef";

    pub(super) fn base() -> PathBuf {
        let d = crate::testing::track(std::env::temp_dir().join(format!("am-r141-{}", crate::db::ulid())));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 既有測試用的：名額夠多、shared 被佔著就馬上改用 job 目錄（等待另有測試）。
    pub(super) const QUICK: Admission = Admission { max: 64, jobs: 1, shared_wait_secs: 0, queue_wait_secs: 30, pool: 1, try_only: false, max_load: None };

    fn guard(base: &Path, hash: &str, job: &str) -> Lease {
        guard_with(base, hash, job, QUICK)
    }

    pub(super) fn guard_cmd(base: &Path, hash: &str, job: &str, token: &LeaseToken, adm: Admission) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(guard_script("rc", hash, job, token, adm)).current_dir(base);
        cmd
    }

    pub(super) fn guard_with(base: &Path, hash: &str, job: &str, adm: Admission) -> Lease {
        let token = LeaseToken::new();
        Lease::start(guard_cmd(base, hash, job, &token, adm), token, &Deadline::new(0)).expect("guard should hand out a directory")
    }

    fn lock_free(path: &Path) -> bool {
        Command::new("flock").arg("-n").arg(path).arg("true").status().map(|s| s.success()).unwrap_or(false)
    }

    fn wait_until(what: &str, f: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !f() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// 假 cargo：記一筆「跑起來了」，然後一直跑（像一個還在編的遠端 cargo）。
    fn fake_home(base: &Path) -> PathBuf {
        let home = base.join("home");
        let bin = home.join(".cargo/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let cargo = bin.join("cargo");
        crate::testing::write_exec(&cargo, "#!/bin/sh\ntouch \"$PWD/../cargo-started-$$\"\nexec sleep 60\n");
        home
    }

    fn spawn_run(base: &Path, home: &Path, dir: &str, token: &LeaseToken) -> Child {
        // 遠端每條 ssh 都是自己的 session（sshd setsid）；這裡用獨立的 process group 模擬，收 pgid 才不會打到測試本身。
        Command::new("sh")
            .arg("-c")
            .arg(run_script(dir, "", token, 1, 0, &["test".into()]))
            .env("HOME", home)
            .current_dir(base)
            .process_group(0)
            .spawn()
            .unwrap()
    }

    fn started(dir: &str) -> bool {
        let parent = Path::new(dir).parent().unwrap();
        std::fs::read_dir(parent)
            .map(|rd| rd.flatten().any(|e| e.file_name().to_string_lossy().starts_with("cargo-started-")))
            .unwrap_or(false)
    }

    /// 結束後自己的 job 目錄不在了；shared 留著給下一次（target 還在），而且被佔用時第二個呼叫拿不到它。
    #[test]
    fn a_finished_call_removes_its_private_dir_and_keeps_the_shared_target() {
        let base = base();
        let mut a = guard(&base, HASH, "job-1-1");
        assert!(a.hs.dir.ends_with(&format!("/{HASH}/shared")), "{:?}", a.hs);
        std::fs::create_dir_all(Path::new(&a.hs.dir).join("target")).unwrap();
        std::fs::write(Path::new(&a.hs.dir).join("target/marker"), "x").unwrap();

        let mut b = guard(&base, HASH, "job-2-2");
        assert!(b.hs.dir.ends_with(&format!("/{HASH}/job-2-2")), "shared 被 a 佔著，b 要退回自己的目錄：{:?}", b.hs);
        let bdir = b.hs.dir.clone();
        assert!(Path::new(&bdir).is_dir());
        assert!(b.finish().unwrap().success());
        assert!(!Path::new(&bdir).exists(), "job 目錄結束後要刪掉");
        assert!(!Path::new(&format!("{bdir}.lock")).exists());

        let adir = a.hs.dir.clone();
        assert!(a.finish().unwrap().success());
        assert!(Path::new(&adir).join("target/marker").is_file(), "shared 的 target 要留給下一次");

        let mut c = guard(&base, HASH, "job-3-3");
        assert_eq!(c.hs.dir, adir, "shared 還回去之後，下一次要重用它");
        c.finish();
        let _ = std::fs::remove_dir_all(base);
    }

    /// 被中斷（helper／ssh 死掉，遠端只看到 stdin EOF）：還在跑的遠端 cargo 整組收掉，目錄刪掉。
    #[test]
    fn an_interrupted_call_stops_the_remote_cargo_and_removes_its_dir() {
        let base = base();
        let home = fake_home(&base);
        let mut holder = guard(&base, HASH, "job-1-1"); // 佔住 shared，讓下一個拿 job 目錄
        let mut lease = guard(&base, HASH, "job-2-2");
        let dir = lease.hs.dir.clone();
        let mut run = spawn_run(&base, &home, &dir, &lease.token);
        wait_until("fake cargo to start", || started(&dir));

        drop(lease.stdin.take()); // 本機那端沒了
        assert!(lease.finish().unwrap().success());
        let deadline = Instant::now() + Duration::from_secs(15);
        let status = loop {
            if let Some(st) = run.try_wait().unwrap() {
                break st;
            }
            if Instant::now() > deadline {
                let _ = run.kill();
                panic!("remote cargo still running after its lease ended");
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(status.signal().is_some() || !status.success(), "{status:?}");
        assert!(!Path::new(&dir).exists(), "中斷也要刪目錄");
        holder.finish();
        let _ = std::fs::remove_dir_all(base);
    }

    /// helper 已經死了、目錄也還回去了，才姍姍來遲的 run（孤兒 ssh）不能在別人的目錄裡跑 cargo。
    #[test]
    fn a_late_run_after_the_lease_ended_does_not_start_cargo() {
        let base = base();
        let home = fake_home(&base);
        let mut lease = guard(&base, HASH, "job-1-1");
        let (dir, token) = (lease.hs.dir.clone(), lease.token.clone());
        lease.finish();
        let status = spawn_run(&base, &home, &dir, &token).wait().unwrap();
        assert_eq!(status.code(), Some(126));
        assert!(!started(&dir));
        let _ = std::fs::remove_dir_all(base);
    }

    /// #177：從子目錄呼叫時，遠端也要在同一個子目錄下跑 cargo（相對路徑的參數才跟本機是同一個意思）；
    /// 就在根的呼叫照舊在根目錄跑。
    #[test]
    fn the_remote_run_starts_in_the_same_subdirectory_as_the_local_call() {
        let base = base();
        let home = fake_home(&base);
        // 這裡的假 cargo 把自己的 cwd 記下來就結束。
        let cargo = home.join(".cargo/bin/cargo");
        std::fs::write(&cargo, "#!/bin/sh\npwd > \"$AM_TEST_PWD\"\n").unwrap();
        let mut lease = guard(&base, HASH, "job-1-1");
        let dir = lease.hs.dir.clone();
        std::fs::create_dir_all(Path::new(&dir).join("daemon/src")).unwrap();
        for sub in ["daemon/src", ""] {
            let out = base.join("pwd.out");
            let _ = std::fs::remove_file(&out);
            let status = Command::new("sh")
                .arg("-c")
                .arg(run_script(&dir, sub, &lease.token, 1, 0, &["check".into()]))
                .env("HOME", &home)
                .env("AM_TEST_PWD", &out)
                .current_dir(&base)
                .status()
                .unwrap();
            assert!(status.success(), "{sub}: {status:?}");
            let cwd = std::fs::read_to_string(&out).unwrap();
            let want = Path::new(&dir).join(sub);
            assert_eq!(std::fs::canonicalize(cwd.trim()).unwrap(), std::fs::canonicalize(&want).unwrap(), "sub={sub:?}");
        }
        lease.finish();
        let _ = std::fs::remove_dir_all(base);
    }

    fn wait_run(run: &mut Child) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if let Some(st) = run.try_wait().unwrap() {
                return st;
            }
            if Instant::now() > deadline {
                let _ = unsafe { libc::kill(-(run.id() as i32), libc::SIGKILL) };
                let _ = run.wait();
                panic!("the stale run went on to start cargo");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// #148：租約身份不能是遠端 PID。上一代結束、下一代租到同一個 shared 之後，姍姍來遲的舊 run
    /// （帶的是上一代的 token）不能通過 owner 檢查；下一代自己的 run 照常跑。
    /// 修正前 token 是守門的 `$$`，兩代剛好同一個 remote PID 時 `.owner` 又變回舊值、舊 run 誤通過
    /// （當時用把 `$$` 釘成同一個數字模擬，紅在「the stale run went on to start cargo」）。
    #[test]
    fn a_stale_run_is_refused_after_the_next_lease_takes_over_the_same_dir() {
        let base = base();
        let home = fake_home(&base);
        let mut a = guard(&base, HASH, "job-1-1");
        let (dir, stale) = (a.hs.dir.clone(), a.token.clone());
        a.finish();
        let mut b = guard(&base, HASH, "job-2-2");
        assert_eq!(b.hs.dir, dir, "b 要租到同一個 shared");
        assert_ne!(b.token, stale, "每一代都要有新的 token");

        let status = wait_run(&mut spawn_run(&base, &home, &dir, &stale));
        assert_eq!(status.code(), Some(126), "{status:?}");
        assert!(!started(&dir), "舊 run 不能在下一代的目錄裡跑 cargo");

        // 同一次租約的 run 照常跑。
        let mut run = spawn_run(&base, &home, &dir, &b.token);
        wait_until("the current lease's cargo to start", || started(&dir));
        assert!(b.finish().unwrap().success());
        wait_run(&mut run);
        let _ = std::fs::remove_dir_all(base);
    }

    /// `.owner`、pgid sidecar 與交給 run 的 token 都是本機給的那個，不是守門 shell 的 PID；
    /// 下一代結束時只收自己那一代的 sidecar，舊 run 留下的別人的 sidecar 不當成自己的去殺。
    #[test]
    fn the_lease_identity_is_the_callers_token_and_cleanup_only_reaps_its_own_generation() {
        let base = base();
        let home = fake_home(&base);
        let mut a = guard(&base, HASH, "job-1-1");
        let (dir, stale) = (a.hs.dir.clone(), a.token.clone());
        assert_eq!(a.hs.token, stale.as_str(), "守門要把 token 原樣交回來");
        assert_eq!(std::fs::read_to_string(format!("{dir}.owner")).unwrap().trim(), stale.as_str());
        assert_ne!(stale.as_str(), a.child.id().to_string(), "身份不能是守門 shell 的 PID");
        a.finish();

        let mut b = guard(&base, HASH, "job-2-2");
        assert_eq!(std::fs::read_to_string(format!("{dir}.owner")).unwrap().trim(), b.token.as_str());
        // 舊 run 被拒之前已經寫下的 sidecar（指向一個不屬於這一代的 process group）。
        let mut decoy = Command::new("sleep").arg("60").process_group(0).spawn().unwrap();
        std::fs::write(format!("{dir}.pgid-{}", stale.as_str()), format!("{}\n", decoy.id())).unwrap();
        let mut run = spawn_run(&base, &home, &dir, &b.token);
        wait_until("the current lease's cargo to start", || started(&dir));
        assert!(Path::new(&format!("{dir}.pgid-{}", b.token.as_str())).is_file(), "sidecar 用 token 命名");

        assert!(b.finish().unwrap().success());
        wait_run(&mut run);
        assert!(decoy.try_wait().unwrap().is_none(), "別人那一代的 sidecar 指到的行程不能被收");
        let _ = decoy.kill();
        let _ = decoy.wait();
        let left: Vec<_> = std::fs::read_dir(Path::new(&dir).parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".pgid-"))
            .collect();
        assert!(left.is_empty(), "結束後 sidecar 要清乾淨：{left:?}");
        let _ = std::fs::remove_dir_all(base);
    }

    /// 回收只刪沒人在用的孤兒：有行程 cwd 在裡面、鎖被持有、不是自己命名規則的、根目錄外的、
    /// 自己這次的，就算被點名也不刪（守門自己再看一次，不只相信 helper 的篩選）。
    #[test]
    fn gc_removes_orphans_but_never_a_dir_in_use_or_outside_its_layout() {
        let base = base();
        let rc = base.join("rc");
        let (h, h2) = ("00000000000000aa", "00000000000000bb");
        for d in [format!("{h}/111"), format!("{h}/222"), format!("{h}/333"), format!("{h}/src"), format!("{h2}/444"), "nothex/555".into()] {
            std::fs::create_dir_all(rc.join(d)).unwrap();
        }
        std::fs::create_dir_all(base.join("outside")).unwrap();
        let mut busy = Command::new("sleep").arg("60").current_dir(rc.join(h).join("222")).spawn().unwrap();
        let lock = rc.join(h).join("333.lock");
        let mut locker = Command::new("flock").arg(&lock).arg("sleep").arg("60").spawn().unwrap();
        wait_until("the lock to be held", || {
            !Command::new("flock").arg("-n").arg(&lock).arg("true").status().unwrap().success()
        });

        let mut g = guard(&base, HASH, "job-9-9");
        let root = g.hs.root.clone();
        let find = |leaf: &str| g.hs.dirs.iter().find(|d| d.path == format!("{root}/{leaf}")).cloned();
        let orphan = find(&format!("{h}/111")).expect("orphan listed");
        assert!(!orphan.locked && !orphan.busy, "{orphan:?}");
        assert!(find(&format!("{h}/222")).unwrap().busy);
        assert!(find(&format!("{h}/333")).unwrap().locked);
        assert!(find(&format!("{h}/src")).is_none() && find("nothex/555").is_none(), "{:?}", g.hs.dirs);
        assert!(select_gc(&g.hs, &GcPolicy::default()).is_empty(), "剛建的目錄還沒到閒置門檻：{:?}", g.hs.dirs);

        let own = g.hs.dir.clone();
        let named: Vec<String> = [
            format!("{root}/{h}/111"),
            format!("{root}/{h}/222"),
            format!("{root}/{h}/333"),
            format!("{root}/{h}/src"),
            format!("{root}/{h2}/444"),
            format!("{root}/nothex/555"),
            format!("{root}/{h}/../{h2}"),
            base.join("outside").to_string_lossy().into_owned(),
            own.clone(),
        ]
        .into();
        g.collect(&named);
        assert!(g.finish().unwrap().success());

        assert!(!rc.join(h).join("111").exists(), "閒置孤兒要收");
        assert!(!rc.join(h2).exists(), "收完變空的 hash 目錄一起收");
        assert!(rc.join(h).join("222").is_dir(), "有行程在用的不能刪");
        assert!(rc.join(h).join("333").is_dir(), "鎖被持有的不能刪");
        assert!(rc.join(h).join("src").is_dir() && rc.join("nothex/555").is_dir(), "不是自己命名的不能刪");
        assert!(base.join("outside").is_dir(), "根目錄外的不能刪");
        assert!(Path::new(&own).is_dir(), "自己這次的 shared 不能被自己回收");

        let _ = busy.kill();
        let _ = locker.kill();
        let _ = busy.wait();
        let _ = locker.wait();
        let _ = std::fs::remove_dir_all(base);
    }

    // ── 整體執行上限（issue #194）：用假的 ssh／rsync 把本機當成遠端，跑真的 `run_offload`＋真的守門腳本 ──

    /// 寫一支假腳本（`body` 接在 `#!/bin/sh` 之後）並確定它已經可以被 exec（issue #189）：見 [`crate::testing::write_exec`]。
    pub(super) fn write_exec(path: &Path, body: &str) {
        crate::testing::write_exec(path, format!("#!/bin/sh\n{body}"))
    }

    /// 假遠端：`ssh` 把最後一個參數（要在遠端跑的指令字串）在**本機**用 `sh -c` 跑，跟 sshd 一樣每條連線自成一個 session
    /// （`setsid`；收 process group 才不會打到測試本身）；`rsync` 把來源目錄複製到 `user@host:` 後面那個路徑。
    /// `$HOME` 指到沙盒，裡面的假 cargo 就是「遠端的 cargo」。回傳 (設定, cwd, data_dir)。
    pub(super) fn fake_remote(base: &Path, timeout_secs: u64, cargo_body: &str) -> (BuildRemoteCfg, PathBuf, PathBuf) {
        let bin = base.join("fakebin");
        let home = base.join("home");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(home.join(".cargo/bin")).unwrap();
        write_exec(&home.join(".cargo/bin/cargo"), cargo_body);
        write_exec(
            &bin.join("ssh"),
            &format!("for a; do last=$a; done\nHOME='{}' exec setsid sh -c \"$last\"\n", home.display()),
        );
        write_exec(&bin.join("rsync"), "for a; do src=$dest; dest=$a; done\nd=${dest#*:}\nmkdir -p \"$d\" && cp -a \"${src}.\" \"$d\"\n");
        let cwd = base.join("work");
        std::fs::create_dir_all(cwd.join("src")).unwrap();
        std::fs::write(cwd.join("Cargo.toml"), "[package]\nname = \"p\"\nversion = \"0.1.0\"\n").unwrap();
        let data = base.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let remote = BuildRemoteCfg {
            enabled: true,
            host: "fake".into(),
            user: "me".into(),
            ssh_port: 22,
            remote_root: base.join("rc").display().to_string(),
            cargo_jobs: 1,
            test_threads: 0,
            timeout_secs,
            ..Default::default()
        };
        (remote, cwd, data)
    }

    /// 這條執行緒上的等待與負載都密封住再跑 `f`：shared 等待只等 [`SEALED_SHARED_WAIT_SECS`] 秒，負載檔是低負載（`base/loadavg`）。
    /// 巢狀安全：結束時還原外層設的值。要測長等待的測試在 `f` 裡自己覆寫 [`TEST_SHARED_WAIT_SECS`]。
    pub(super) fn with_sealed_waits<T>(base: &Path, f: impl FnOnce() -> T) -> T {
        let file = base.join("loadavg");
        std::fs::write(&file, "0.10 0.10 0.10 1/1 1\n").unwrap();
        let prev_wait = TEST_SHARED_WAIT_SECS.with(|w| w.replace(Some(SEALED_SHARED_WAIT_SECS)));
        let prev_load = TEST_LOADAVG.with(|p| p.replace(Some(file)));
        let out = f();
        TEST_SHARED_WAIT_SECS.with(|w| w.set(prev_wait));
        TEST_LOADAVG.with(|p| *p.borrow_mut() = prev_load);
        out
    }

    /// 測試預設的 shared 等待上限（秒）：正式是 [`SHARED_WAIT_SECS`] 的 900 秒，測試只給 10 秒。
    pub(super) const SEALED_SHARED_WAIT_SECS: u64 = 10;

    /// 在這條執行緒上把 `ssh`／`rsync` 換成 `fake_remote` 寫的假腳本、等待與負載密封住，再跑 `f`。
    pub(super) fn with_fake_transport<T>(base: &Path, f: impl FnOnce() -> T) -> T {
        let bin = base.join("fakebin");
        TEST_PROGRAMS.with(|p| *p.borrow_mut() = Some((bin.join("ssh").display().to_string(), bin.join("rsync").display().to_string())));
        let out = with_sealed_waits(base, f);
        TEST_PROGRAMS.with(|p| *p.borrow_mut() = None);
        out
    }

    /// 接上本機真 rsync，並要求生產程式傳入快取安全旗標。
    fn use_real_rsync_for_fake_remote(base: &Path) {
        write_exec(
            &base.join("fakebin/rsync"),
            r#"src=; dest=; skip=; checksum=; no_times=; exclude_count=0; e1=; e2=; e3=; e4=; e5=; e6=; e7=; e8=
for a; do
  if [ "$skip" = exclude ]; then
    exclude_count=$((exclude_count + 1))
    case "$exclude_count" in 1) e1=$a ;; 2) e2=$a ;; 3) e3=$a ;; 4) e4=$a ;; 5) e5=$a ;; 6) e6=$a ;; 7) e7=$a ;; 8) e8=$a ;; *) echo 'too many excludes' >&2; exit 98 ;; esac
    skip=; continue
  fi
  if [ "$skip" = value ]; then skip=; continue; fi
  case "$a" in
    --checksum) checksum=1 ;;
    --no-times) no_times=1 ;;
    --exclude) skip=exclude ;;
    -e) skip=value ;;
    -az|--delete|--quiet) ;;
    -*) ;;
    *) if [ -z "$src" ]; then src=$a; else dest=$a; fi ;;
  esac
done
[ -n "$checksum" ] && [ -n "$no_times" ] || { echo 'missing cache safety flags' >&2; exit 97; }
d=${dest#*:}
set -- -az --delete --checksum --no-times --quiet
for e in "$e1" "$e2" "$e3" "$e4" "$e5" "$e6" "$e7" "$e8"; do [ -z "$e" ] || set -- "$@" --exclude "$e"; done
set -- "$@" "$src" "$d"
rsync "$@"; rc=$?
fail_marker="${src%/}/../.rsync-fail-after-copy"
if [ -f "$fail_marker" ]; then
  n=$(cat "$fail_marker"); case "$n" in ''|*[!0-9]*) n=0 ;; esac
  if [ "$n" -gt 0 ]; then n=$((n - 1)); if [ "$n" -eq 0 ]; then rm -f "$fail_marker"; else printf '%s\n' "$n" > "$fail_marker"; fi; echo 'rsync: vanished file: No such file or directory (2)' >&2; exit 24; fi
fi
exit "$rc"
"#,
        );
    }

    /// node_modules 只有專案明確選擇排除時才不傳；切換政策時也清掉遠端舊值。
    #[test]
    fn remote_cargo_exclusions_are_opt_in_and_prune_old_remote_inputs() {
        if !has_program("rsync") {
            return;
        }
        let base = base();
        let (remote, cwd, data) = fake_remote(&base, 60, "exit 0\n");
        std::fs::write(
            cwd.join("Cargo.toml"),
            "[workspace]\n[workspace.metadata.agents-manager.remote-cargo]\nexclude = [\"/web/node_modules\"]\n",
        )
        .unwrap();
        std::fs::create_dir_all(cwd.join("web/node_modules")).unwrap();
        std::fs::write(cwd.join("web/node_modules/package.json"), "current\n").unwrap();
        let dir = format!("{}/rc/{}/shared", base.display(), cache_hash(&cwd));
        std::fs::create_dir_all(Path::new(&dir).join("web/node_modules")).unwrap();
        std::fs::write(Path::new(&dir).join("web/node_modules/stale.json"), "old\n").unwrap();
        use_real_rsync_for_fake_remote(&base);

        let sync = || {
            with_fake_transport(&base, || {
                sync_source(&remote, &data, &cwd, &dir, &Deadline::new(60))
            })
        };
        sync().expect("explicit node_modules exclusion should sync");
        assert!(
            !Path::new(&dir).join("web/node_modules").exists(),
            "excluded paths must not leave old remote inputs behind"
        );

        std::fs::write(cwd.join("Cargo.toml"), "[workspace]\n").unwrap();
        sync().expect("default policy should preserve node_modules");
        assert_eq!(
            std::fs::read(Path::new(&dir).join("web/node_modules/package.json")).unwrap(),
            b"current\n"
        );
        let _ = std::fs::remove_dir_all(base);
    }

    /// #1861 review：遠端殘留的根 `.claude`（worktree 與其 target 可能十幾 GB）不能被整棵 touch 遍歷；一般原始碼照樣 touch。
    #[test]
    fn the_remote_touch_skips_a_leftover_root_claude_tree() {
        let base = base();
        let (remote, _cwd, data) = fake_remote(&base, 60, "exit 0\n");
        let dir = base.join("shared");
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let stale = dir.join(".claude/worktrees/x/target/big");
        let stale_dist = dir.join("web/dist/index.html");
        let live = dir.join("src/lib.rs");
        for p in [&stale, &stale_dist, &live] {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "x").unwrap();
            std::fs::File::options().write(true).open(p).unwrap().set_times(std::fs::FileTimes::new().set_modified(old)).unwrap();
        }
        with_fake_transport(&base, || touch_remote_workspace(&remote, &data, &dir.display().to_string(), &Deadline::new(60))).expect("touch succeeds");
        assert_eq!(std::fs::metadata(&stale).unwrap().modified().unwrap(), old, "遠端殘留的 .claude 不被 touch");
        assert!(std::fs::metadata(&stale_dist).unwrap().modified().unwrap() > old, "workspace touch includes frontend build inputs");
        assert!(std::fs::metadata(&live).unwrap().modified().unwrap() > old, "一般原始碼照樣 touch");

        std::fs::File::options().write(true).open(&live).unwrap().set_times(std::fs::FileTimes::new().set_modified(old)).unwrap();
        with_fake_transport(&base, || {
            touch_remote_files(
                &remote,
                &data,
                &dir.display().to_string(),
                &[
                    PathBuf::from(".claude/worktrees/x/target/big"),
                    PathBuf::from("web/dist/index.html"),
                    PathBuf::from("src/lib.rs"),
                ],
                &Deadline::new(60),
            )
        })
        .expect("individual touch must skip the excluded root .claude tree");
        assert_eq!(std::fs::metadata(&stale).unwrap().modified().unwrap(), old, "逐檔 touch 也略過根 .claude");
        assert!(std::fs::metadata(&stale_dist).unwrap().modified().unwrap() > old, "individual touch includes frontend build inputs");
        assert!(std::fs::metadata(&live).unwrap().modified().unwrap() > old, "逐檔 touch 仍更新一般原始碼");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Exclusion cleanup must fail closed when an intermediate remote path is a symlink.
    #[test]
    fn remote_exclusion_pruning_refuses_symlink_ancestors() {
        if !has_program("rsync") {
            return;
        }
        let base = base();
        let (remote, cwd, data) = fake_remote(&base, 60, "exit 0\n");
        std::fs::write(
            cwd.join("Cargo.toml"),
            "[workspace]\n[workspace.metadata.agents-manager.remote-cargo]\nexclude = [\"/web/node_modules\"]\n",
        )
        .unwrap();
        std::fs::create_dir_all(cwd.join("web/node_modules")).unwrap();
        std::fs::write(cwd.join("web/node_modules/package.json"), "source\n").unwrap();

        let dir = format!("{}/rc/{}/shared", base.display(), cache_hash(&cwd));
        let outside = base.join("outside");
        std::fs::create_dir_all(outside.join("node_modules")).unwrap();
        std::fs::write(outside.join("node_modules/keep.json"), "must survive\n").unwrap();
        std::fs::create_dir_all(Path::new(&dir)).unwrap();
        symlink(&outside, Path::new(&dir).join("web")).unwrap();
        use_real_rsync_for_fake_remote(&base);

        let result = with_fake_transport(&base, || {
            sync_source(&remote, &data, &cwd, &dir, &Deadline::new(60))
        });
        assert!(result.is_err(), "an excluded path behind a symlink ancestor must be refused");
        assert_eq!(
            std::fs::read(outside.join("node_modules/keep.json")).unwrap(),
            b"must survive\n",
            "cleanup must not traverse outside the leased directory"
        );
        assert!(std::fs::symlink_metadata(Path::new(&dir).join("web")).unwrap().file_type().is_symlink());
        let _ = std::fs::remove_dir_all(base);
    }

    /// Removed parents and directory-to-file transitions must not be recreated or passed to touch.
    #[test]
    fn workspace_invalidation_ignores_removed_paths_and_prunes_target() {
        if !has_program("rsync") {
            return;
        }
        let base = base();
        let (remote, cwd, data) = fake_remote(&base, 60, "exit 0\n");
        std::fs::create_dir_all(cwd.join("removed/child")).unwrap();
        std::fs::write(cwd.join("removed/child/input.txt"), "old\n").unwrap();
        std::fs::create_dir_all(cwd.join("switched/child")).unwrap();
        std::fs::write(cwd.join("switched/child/input.txt"), "old\n").unwrap();
        let dir = format!("{}/rc/{}/shared", base.display(), cache_hash(&cwd));
        std::fs::create_dir_all(Path::new(&dir)).unwrap();
        use_real_rsync_for_fake_remote(&base);

        let sync = || {
            with_fake_transport(&base, || {
                sync_source(&remote, &data, &cwd, &dir, &Deadline::new(60))
            })
        };
        sync().expect("initial tree sync should succeed");
        let target_file = Path::new(&dir).join("target/debug/sentinel");
        std::fs::create_dir_all(target_file.parent().unwrap()).unwrap();
        std::fs::write(&target_file, "keep target\n").unwrap();
        // 固定在過去的時間：任何會動到 target 的同步都會把 mtime 改成現在，不必等檔案系統的秒數精度。
        set_mtime(&target_file, std::time::UNIX_EPOCH + Duration::from_secs(10));
        let target_mtime = std::fs::metadata(&target_file).unwrap().modified().unwrap();

        std::fs::remove_dir_all(cwd.join("removed")).unwrap();
        std::fs::remove_dir_all(cwd.join("switched")).unwrap();
        std::fs::write(cwd.join("switched"), "now a regular file\n").unwrap();
        sync().expect("deletion and directory-to-file transition should synchronize");

        assert!(!Path::new(&dir).join("removed").exists(), "deleted paths must not be recreated by invalidation");
        assert_eq!(std::fs::read(Path::new(&dir).join("switched")).unwrap(), b"now a regular file\n");
        assert_eq!(std::fs::metadata(&target_file).unwrap().modified().unwrap(), target_mtime, "workspace invalidation must leave target artifacts alone");
        let _ = std::fs::remove_dir_all(base);
    }

    fn source_manifest(entries: &[(&str, u8, &[u8], u32)]) -> SourceManifest {
        SourceManifest {
            entries: entries
                .iter()
                .map(|(path, kind, value, mode)| {
                    (
                        path.as_bytes().to_vec(),
                        SourceEntry {
                            kind: *kind,
                            mode: *mode,
                            value: value.to_vec(),
                        },
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn source_manifest_only_narrows_safe_regular_file_changes() {
        let root = Path::new("/workspace");
        let previous = source_manifest(&[
            ("Cargo.toml", b'F', b"manifest", 0o644),
            ("src/lib.rs", b'F', b"old", 0o644),
            ("src/other.rs", b'F', b"same", 0o644),
        ]);
        let changed = source_manifest(&[
            ("Cargo.toml", b'F', b"manifest", 0o644),
            ("src/lib.rs", b'F', b"new", 0o644),
            ("src/other.rs", b'F', b"same", 0o644),
        ]);
        assert_eq!(
            source_invalidation(root, Some(&previous), &changed),
            SourceInvalidation::Files(vec![PathBuf::from("src/lib.rs")]),
            "an unchanged-type Rust file can be invalidated by itself"
        );
        assert_eq!(
            source_invalidation(root, Some(&previous), &previous),
            SourceInvalidation::Unchanged,
            "an unchanged source tree must keep the hot target intact"
        );

        let mode_changed = source_manifest(&[
            ("Cargo.toml", b'F', b"manifest", 0o644),
            ("src/lib.rs", b'F', b"old", 0o755),
            ("src/other.rs", b'F', b"same", 0o644),
        ]);
        assert_eq!(
            source_invalidation(root, Some(&previous), &mode_changed),
            SourceInvalidation::Files(vec![PathBuf::from("src/lib.rs")]),
            "a permission-only change to a regular file can be invalidated by itself"
        );

        for path in [
            "Cargo.toml",
            "Cargo.lock",
            "build.rs",
            ".cargo/config.toml",
            "rust-toolchain",
            "rust-toolchain.toml",
            "crates/core/Cargo.toml",
            "crates/core/build.rs",
            "crates/core/rust-toolchain.toml",
        ] {
            let before = source_manifest(&[(path, b'F', b"old", 0o644)]);
            let after = source_manifest(&[(path, b'F', b"new", 0o644)]);
            assert_eq!(
                source_invalidation(root, Some(&before), &after),
                SourceInvalidation::Workspace,
                "Cargo control input {path:?} must invalidate the full workspace"
            );
            let added = source_manifest(&[(path, b'F', b"new", 0o644)]);
            let removed = source_manifest(&[]);
            assert_eq!(
                source_invalidation(root, Some(&removed), &added),
                SourceInvalidation::Workspace,
                "adding Cargo control input {path:?} must invalidate the full workspace"
            );
            assert_eq!(
                source_invalidation(root, Some(&added), &removed),
                SourceInvalidation::Workspace,
                "removing Cargo control input {path:?} must invalidate the full workspace"
            );
        }

        let before = source_manifest(&[
            ("src", b'D', b"", 0o755),
            ("src/lib.rs", b'F', b"old", 0o644),
            ("src/removed.rs", b'F', b"old", 0o644),
        ]);
        let added_and_removed = source_manifest(&[
            ("src", b'D', b"", 0o755),
            ("src/lib.rs", b'F', b"old", 0o644),
            ("src/new.txt", b'F', b"new", 0o644),
        ]);
        let directory_changed = source_manifest(&[
            ("src", b'D', b"", 0o755),
            ("src/new-dir", b'D', b"", 0o755),
        ]);
        let type_changed = source_manifest(&[
            ("src", b'D', b"", 0o755),
            ("src/lib.rs", b'L', b"target.rs", 0o777),
        ]);
        assert_eq!(
            source_invalidation(root, Some(&before), &added_and_removed),
            SourceInvalidation::Workspace,
            "even non-Rust additions may change untracked compile-time directory listings"
        );
        assert_eq!(
            source_invalidation(root, Some(&before), &directory_changed),
            SourceInvalidation::Workspace,
            "directory changes are structural"
        );
        assert_eq!(
            source_invalidation(root, Some(&before), &type_changed),
            SourceInvalidation::Workspace,
            "type changes must conservatively invalidate the workspace"
        );
        assert_eq!(
            source_invalidation(root, None, &before),
            SourceInvalidation::Workspace,
            "missing or corrupt baselines must invalidate the workspace"
        );

        let existing_module_pair = source_manifest(&[
            ("src", b'D', b"", 0o755),
            ("src/lib.rs", b'F', b"mod foo;", 0o644),
            ("src/foo", b'D', b"", 0o755),
            ("src/foo/mod.rs", b'F', b"", 0o644),
        ]);
        let ambiguous_module = source_manifest(&[
            ("src", b'D', b"", 0o755),
            ("src/lib.rs", b'F', b"mod foo;", 0o644),
            ("src/foo", b'D', b"", 0o755),
            ("src/foo/mod.rs", b'F', b"", 0o644),
            ("src/foo.rs", b'F', b"", 0o644),
        ]);
        let report = source_invalidation_report(root, Some(&existing_module_pair), &ambiguous_module);
        assert_eq!(report.action, SourceInvalidation::Workspace);
        assert_eq!(
            report.fallback["file added"].paths,
            vec!["src/foo.rs".to_string()],
            "a new Rust module candidate can affect old dep-info inputs"
        );
    }

    #[test]
    fn workspace_root_permission_changes_are_manifest_inputs() {
        use std::os::unix::fs::PermissionsExt as _;
        let base = base();
        let root = base.join("source");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        let before = scan_source_manifest(&root, &[]).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o750)).unwrap();
        let after = scan_source_manifest(&root, &[]).unwrap();
        assert_eq!(source_invalidation(&root, Some(&before), &after), SourceInvalidation::Workspace);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn source_manifest_report_lists_fallback_reasons_and_safe_files() {
        let root = Path::new("/workspace");
        let before = source_manifest(&[
            ("src", b'D', b"", 0o755),
            ("src/lib.rs", b'F', b"old", 0o644),
            ("src/removed-dir", b'D', b"", 0o755),
            ("Cargo.toml", b'F', b"old", 0o644),
        ]);
        let after = source_manifest(&[
            ("src", b'D', b"", 0o755),
            ("src/added-dir", b'D', b"", 0o755),
            ("src/lib.rs", b'F', b"new", 0o644),
            ("Cargo.toml", b'F', b"new", 0o644),
        ]);

        let report = source_invalidation_report(root, Some(&before), &after);
        assert_eq!(report.action, SourceInvalidation::Workspace);
        assert_eq!(report.safe_files, vec![PathBuf::from("src/lib.rs")]);
        assert_eq!(
            report.fallback["added directory"].paths,
            vec!["src/added-dir".to_string()]
        );
        assert_eq!(
            report.fallback["removed directory"].paths,
            vec!["src/removed-dir".to_string()]
        );
        assert_eq!(
            report.fallback["Cargo control input changed"].paths,
            vec!["Cargo.toml".to_string()]
        );
        let added_control = source_invalidation_report(
            root,
            Some(&source_manifest(&[])),
            &source_manifest(&[("rust-toolchain.toml", b'F', b"stable", 0o644)]),
        );
        assert_eq!(
            added_control.fallback["Cargo control input added"].paths,
            vec!["rust-toolchain.toml".to_string()]
        );
    }

    #[test]
    fn added_and_removed_frontend_files_are_source_inputs() {
        let before = source_manifest(&[
            ("web", b'D', b"", 0o755),
            ("web/dist", b'D', b"", 0o755),
            ("web/dist/assets", b'D', b"", 0o755),
            ("web/dist/index.html", b'F', b"old frontend", 0o644),
        ]);
        let after = source_manifest(&[
            ("web", b'D', b"", 0o755),
            ("web/dist", b'D', b"", 0o755),
            ("web/dist/assets", b'D', b"", 0o755),
            ("web/dist/index.html", b'F', b"old frontend", 0o644),
            ("web/dist/assets/new.js", b'F', b"new frontend", 0o644),
        ]);

        assert_eq!(
            source_invalidation_report(Path::new("/workspace"), Some(&before), &after).action,
            SourceInvalidation::Workspace,
            "new generated assets may change compiler inputs absent from old dep-info"
        );
        assert_eq!(
            source_invalidation_report(Path::new("/workspace"), Some(&after), &before).action,
            SourceInvalidation::Files(vec![PathBuf::from("web/dist/assets")]),
            "removing a generated asset invalidates via the surviving watched directory"
        );
    }

    #[test]
    fn directory_and_external_symlinks_force_workspace_invalidation() {
        use std::os::unix::fs::symlink;

        for target_is_directory in [false, true] {
            let base = base();
            let root = base.join("source");
            let target = if target_is_directory {
                base.join("outside-dir")
            } else {
                base.join("outside-file")
            };
            if target_is_directory {
                std::fs::create_dir_all(&target).unwrap();
                std::fs::write(target.join("input.txt"), "included\n").unwrap();
            } else {
                std::fs::write(&target, "included\n").unwrap();
            }
            std::fs::create_dir_all(root.join("src")).unwrap();
            std::fs::write(root.join("src/lib.rs"), "old\n").unwrap();
            symlink(&target, root.join("include-link")).unwrap();
            let before = scan_source_manifest(&root, &[]).unwrap();
            std::fs::write(root.join("src/lib.rs"), "new\n").unwrap();
            let after = scan_source_manifest(&root, &[]).unwrap();
            assert_eq!(
                source_invalidation(&root, Some(&before), &after),
                SourceInvalidation::Workspace,
                "directory and workspace-external symlinks make changed-input scope unknown"
            );
            let _ = std::fs::remove_dir_all(base);
        }

        let base = base();
        let root = base.join("source");
        std::fs::create_dir_all(root.join("assets")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("assets/value.txt"), "old included input\n").unwrap();
        std::fs::write(root.join("src/lib.rs"), "old source\n").unwrap();
        symlink("assets", root.join("assets-link")).unwrap();
        let before = scan_source_manifest(&root, &[]).unwrap();
        std::fs::write(root.join("assets/value.txt"), "new included input\n").unwrap();
        let after = scan_source_manifest(&root, &[]).unwrap();
        assert_eq!(
            source_invalidation(&root, Some(&before), &after),
            SourceInvalidation::Workspace,
            "content changes below an unchanged in-workspace directory symlink must invalidate the full workspace"
        );
        let _ = std::fs::remove_dir_all(base);
    }

    fn fake_cargo_mtime_paths(dir: &Path) -> (PathBuf, PathBuf) {
        let fingerprint = dir.join("target/debug/.fingerprint/test-crate-hash/lib-test_crate.json");
        let dep_info = dir.join("target/debug/deps/test_crate-hash.d");
        std::fs::create_dir_all(fingerprint.parent().unwrap()).unwrap();
        std::fs::create_dir_all(dep_info.parent().unwrap()).unwrap();
        std::fs::write(&fingerprint, "fingerprint\n").unwrap();
        std::fs::write(&dep_info, "dep-info\n").unwrap();
        (fingerprint, dep_info)
    }

    #[test]
    fn file_touch_moves_past_equal_and_future_cargo_mtimes() {
        for artifact_offset in [0, 3600] {
            let base = base();
            let (remote, _cwd, data) = fake_remote(&base, 60, "exit 0\n");
            let dir = base.join("rc/shared");
            std::fs::create_dir_all(dir.join("src")).unwrap();
            let changed = dir.join("src/lib.rs");
            std::fs::write(&changed, "source\n").unwrap();
            let artifact_time = std::time::SystemTime::now()
                + Duration::from_secs(artifact_offset);
            set_mtime(&changed, artifact_time);
            let (fingerprint, dep_info) = fake_cargo_mtime_paths(&dir);
            set_mtime(&fingerprint, artifact_time);
            set_mtime(&dep_info, artifact_time);

            with_fake_transport(&base, || {
                touch_remote_files(
                    &remote,
                    &data,
                    &dir.display().to_string(),
                    &[PathBuf::from("src/lib.rs")],
                    &Deadline::new(60),
                )
            })
            .expect("file invalidation should move beyond the Cargo cache baseline");

            if artifact_offset == 0 {
                let changed_mtime = std::fs::metadata(&changed).unwrap().modified().unwrap();
                assert!(changed_mtime > std::fs::metadata(&fingerprint).unwrap().modified().unwrap());
                assert!(changed_mtime > std::fs::metadata(&dep_info).unwrap().modified().unwrap());
            } else {
                assert!(!fingerprint.exists(), "future-dated Cargo fingerprints must be invalidated after a clock rollback");
                assert!(!dep_info.exists(), "future-dated dep-info must be invalidated after a clock rollback");
            }
            let _ = std::fs::remove_dir_all(base);
        }
    }

    #[test]
    fn workspace_touch_moves_past_equal_and_future_cargo_mtimes() {
        for artifact_offset in [0, 3600] {
            let base = base();
            let (remote, _cwd, data) = fake_remote(&base, 60, "exit 0\n");
            let dir = base.join("rc/shared");
            std::fs::create_dir_all(dir.join("src")).unwrap();
            let source = dir.join("src/lib.rs");
            std::fs::write(&source, "source\n").unwrap();
            let artifact_time = std::time::SystemTime::now()
                + Duration::from_secs(artifact_offset);
            set_mtime(&source, artifact_time);
            let (fingerprint, dep_info) = fake_cargo_mtime_paths(&dir);
            set_mtime(&fingerprint, artifact_time);
            set_mtime(&dep_info, artifact_time);

            with_fake_transport(&base, || {
                touch_remote_workspace(
                    &remote,
                    &data,
                    &dir.display().to_string(),
                    &Deadline::new(60),
                )
            })
            .expect("workspace invalidation should move beyond the Cargo cache baseline");

            if artifact_offset == 0 {
                let source_mtime = std::fs::metadata(&source).unwrap().modified().unwrap();
                assert!(source_mtime > std::fs::metadata(&fingerprint).unwrap().modified().unwrap());
                assert!(source_mtime > std::fs::metadata(&dep_info).unwrap().modified().unwrap());
            } else {
                assert!(!fingerprint.exists(), "future-dated Cargo fingerprints must be invalidated after a clock rollback");
                assert!(!dep_info.exists(), "future-dated dep-info must be invalidated after a clock rollback");
            }
            let _ = std::fs::remove_dir_all(base);
        }
    }

    #[test]
    fn cargo_target_symlinks_fail_closed_for_file_and_workspace_invalidation() {
        for (link_path, external_name, external_artifact, visible_trigger) in [
            (
                "target/debug",
                "external-debug",
                ".fingerprint/test-crate-hash/lib-test_crate.json",
                "target/release/deps/trigger.d",
            ),
            (
                "target/debug/.fingerprint",
                "external-fingerprint",
                "test-crate-hash/lib-test_crate.json",
                "target/debug/deps/test-crate-hash.d",
            ),
            (
                "target/debug/deps",
                "external-deps",
                "test_crate-hash.d",
                "target/debug/.fingerprint/test-crate-hash/lib-test_crate.json",
            ),
        ] {
            let base = base();
            let (remote, _cwd, data) = fake_remote(&base, 60, "exit 0\n");
            let dir = base.join("rc/shared");
            std::fs::create_dir_all(dir.join("src")).unwrap();
            let changed = dir.join("src/lib.rs");
            std::fs::write(&changed, "source\n").unwrap();
            set_mtime(&changed, std::time::UNIX_EPOCH + Duration::from_secs(10));
            let _ = fake_cargo_mtime_paths(&dir);

            let link = dir.join(link_path);
            let external = base.join(external_name);
            std::fs::create_dir_all(external.parent().unwrap()).unwrap();
            std::fs::rename(&link, &external).unwrap();
            symlink(&external, &link).unwrap();

            let future = std::time::SystemTime::now() + Duration::from_secs(3600);
            let external_artifact = external.join(external_artifact);
            set_mtime(&external_artifact, future);
            let visible_trigger = dir.join(visible_trigger);
            std::fs::create_dir_all(visible_trigger.parent().unwrap()).unwrap();
            if !visible_trigger.exists() {
                std::fs::write(&visible_trigger, "visible cache trigger\n").unwrap();
            }
            set_mtime(&visible_trigger, future);
            let source_before = std::fs::metadata(&changed).unwrap().modified().unwrap();
            let external_artifact_before = std::fs::metadata(&external_artifact).unwrap().modified().unwrap();

            with_fake_transport(&base, || {
                touch_remote_files(
                    &remote,
                    &data,
                    &dir.display().to_string(),
                    &[PathBuf::from("src/lib.rs")],
                    &Deadline::new(60),
                )
            })
            .expect("a symlinked Cargo cache path must discard the shared target before touching sources");
            assert!(
                !dir.join("target").exists(),
                "a symlink at {link_path} must invalidate the complete local target tree"
            );
            assert!(std::fs::metadata(&changed).unwrap().modified().unwrap() > source_before);
            assert!(external_artifact.exists(), "external cache artifacts must not be followed or removed");
            assert_eq!(std::fs::metadata(&external_artifact).unwrap().modified().unwrap(), external_artifact_before);
            let _ = std::fs::remove_dir_all(base);
        }

        let base = base();
        let (remote, _cwd, data) = fake_remote(&base, 60, "exit 0\n");
        let dir = base.join("rc/shared");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let source = dir.join("src/lib.rs");
        std::fs::write(&source, "source\n").unwrap();
        set_mtime(&source, std::time::UNIX_EPOCH + Duration::from_secs(10));
        let (_, dep_info) = fake_cargo_mtime_paths(&dir);
        let fingerprint_dir = dir.join("target/debug/.fingerprint");
        let external = base.join("external-workspace-fingerprint");
        std::fs::rename(&fingerprint_dir, &external).unwrap();
        symlink(&external, &fingerprint_dir).unwrap();
        let future = std::time::SystemTime::now() + Duration::from_secs(3600);
        set_mtime(&external.join("test-crate-hash/lib-test_crate.json"), future);
        set_mtime(&dep_info, future);
        let source_before = std::fs::metadata(&source).unwrap().modified().unwrap();
        let fingerprint_before = std::fs::metadata(&external.join("test-crate-hash/lib-test_crate.json"))
            .unwrap()
            .modified()
            .unwrap();

        with_fake_transport(&base, || {
            touch_remote_workspace(
                &remote,
                &data,
                &dir.display().to_string(),
                &Deadline::new(60),
            )
        })
        .expect("workspace invalidation must discard a target tree with nested cache symlinks");
        assert!(!dir.join("target").exists());
        assert!(std::fs::metadata(&source).unwrap().modified().unwrap() > source_before);
        assert_eq!(
            std::fs::metadata(&external.join("test-crate-hash/lib-test_crate.json"))
                .unwrap()
                .modified()
                .unwrap(),
            fingerprint_before
        );
        assert!(!dep_info.exists(), "discarding the target must remove its local dep-info");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn sync_touches_only_a_changed_regular_file() {
        if !has_program("rsync") {
            return;
        }
        let base = base();
        let (remote, cwd, data) = fake_remote(&base, 60, "exit 0\n");
        std::fs::write(cwd.join("src/lib.rs"), "old\n").unwrap();
        std::fs::write(cwd.join("unrelated.txt"), "same\n").unwrap();
        let dir = format!("{}/rc/{}/shared", base.display(), cache_hash(&cwd));
        std::fs::create_dir_all(&dir).unwrap();
        use_real_rsync_for_fake_remote(&base);
        let sync = || {
            with_fake_transport(&base, || {
                sync_source(&remote, &data, &cwd, &dir, &Deadline::new(60))
            })
        };
        sync().expect("initial sync should establish a successful manifest");

        let unrelated = Path::new(&dir).join("unrelated.txt");
        set_mtime(&unrelated, std::time::UNIX_EPOCH + Duration::from_secs(10));
        let unrelated_mtime = std::fs::metadata(&unrelated).unwrap().modified().unwrap();
        let changed = Path::new(&dir).join("src/lib.rs");
        std::fs::write(cwd.join("src/lib.rs"), "new\n").unwrap();
        sync().expect("one regular source-file change should synchronize");

        assert_eq!(
            std::fs::metadata(&unrelated).unwrap().modified().unwrap(),
            unrelated_mtime,
            "unrelated source files must keep their mtimes"
        );
        assert!(
            std::fs::metadata(&changed).unwrap().modified().unwrap() > unrelated_mtime,
            "the changed source file must be newer than the shared target baseline"
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn unchanged_sync_refuses_lease_and_target_symlinks_before_rsync() {
        if !has_program("rsync") {
            return;
        }
        let base = base();
        let (remote, cwd, data) = fake_remote(&base, 60, "exit 0\n");
        std::fs::write(cwd.join("src/lib.rs"), "old source\n").unwrap();
        let dir = base.join("rc").join(cache_hash(&cwd)).join("shared");
        std::fs::create_dir_all(&dir).unwrap();
        use_real_rsync_for_fake_remote(&base);
        let sync = || with_fake_transport(&base, || sync_source(
            &remote, &data, &cwd, &dir.display().to_string(), &Deadline::new(60),
        ));
        sync().unwrap();
        let outside = base.join("outside-cache");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep"), "must survive\n").unwrap();
        symlink(&outside, dir.join("target")).unwrap();
        assert!(sync().is_err(), "even unchanged inputs must refuse a target symlink");
        std::fs::remove_file(dir.join("target")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        symlink(&outside, dir.join("target/debug")).unwrap();
        sync().expect("an unchanged run may safely discard a cache containing symlinks");
        assert!(!dir.join("target").exists());
        assert_eq!(std::fs::read_to_string(outside.join("keep")).unwrap(), "must survive\n");

        let outside_lease = base.join("outside-lease");
        std::fs::rename(&dir, &outside_lease).unwrap();
        symlink(&outside_lease, &dir).unwrap();
        std::fs::write(cwd.join("src/lib.rs"), "new source\n").unwrap();
        assert!(sync().is_err(), "a lease symlink must fail before rsync can follow it");
        assert_eq!(std::fs::read_to_string(outside_lease.join("src/lib.rs")).unwrap(), "old source\n");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn corrupt_source_manifest_falls_back_to_workspace_touch() {
        if !has_program("rsync") {
            return;
        }
        let base = base();
        let (remote, cwd, data) = fake_remote(&base, 60, "exit 0\n");
        std::fs::write(cwd.join("unrelated.txt"), "same\n").unwrap();
        let dir = format!("{}/rc/{}/shared", base.display(), cache_hash(&cwd));
        std::fs::create_dir_all(&dir).unwrap();
        use_real_rsync_for_fake_remote(&base);
        let sync = || {
            with_fake_transport(&base, || {
                sync_source(&remote, &data, &cwd, &dir, &Deadline::new(60))
            })
        };
        sync().expect("initial sync should establish a successful manifest");

        let unrelated = Path::new(&dir).join("unrelated.txt");
        set_mtime(&unrelated, std::time::UNIX_EPOCH + Duration::from_secs(10));
        let before = std::fs::metadata(&unrelated).unwrap().modified().unwrap();
        std::fs::write(remote_manifest_path(&dir), b"corrupt manifest\n").unwrap();
        sync().expect("a corrupt manifest should recover by syncing and full invalidation");
        assert!(
            std::fs::metadata(&unrelated).unwrap().modified().unwrap() > before,
            "a corrupt baseline must full-touch the remote workspace"
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn changing_one_crate_rechecks_only_it_and_its_dependent_crate() {
        if !has_program("rsync") {
            return;
        }
        let base = base();
        let (remote, cwd, data) = fake_remote(&base, 60, "exit 0\n");
        std::fs::write(
            cwd.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/base\", \"apps/app\", \"crates/unrelated\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        std::fs::write(cwd.join("Cargo.lock"), "version = 4\n").unwrap();
        for (name, path) in [
            ("base", "crates/base"),
            ("app", "apps/app"),
            ("unrelated", "crates/unrelated"),
        ] {
            std::fs::create_dir_all(cwd.join(path).join("src")).unwrap();
            if name == "base" {
                std::fs::create_dir_all(cwd.join(path).join("src/assets")).unwrap();
                std::fs::write(cwd.join(path).join("src/assets/one.txt"), "one\n").unwrap();
                std::fs::write(
                    cwd.join(path).join("build.rs"),
                    "use std::{env, fs, path::Path};\nfn main() { let assets = Path::new(\"src/assets\"); println!(\"cargo:rerun-if-changed={}\", assets.display()); let count = fs::read_dir(assets).unwrap().count(); fs::write(Path::new(&env::var(\"OUT_DIR\").unwrap()).join(\"assets.rs\"), format!(\"pub const ASSET_COUNT: u32 = {count};\\n\")).unwrap(); }\n",
                )
                .unwrap();
            }
            let dependencies = if name == "app" {
                "[dependencies]\nbase = { path = \"../../crates/base\" }\n"
            } else {
                ""
            };
            let build = if name == "base" { "build = \"build.rs\"\n" } else { "" };
            std::fs::write(
                cwd.join(path).join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{build}{dependencies}"),
            )
            .unwrap();
            let source = if name == "base" {
                "include!(concat!(env!(\"OUT_DIR\"), \"/assets.rs\"));\npub fn value() -> u32 { ASSET_COUNT + 1 }\n"
            } else if name == "app" {
                "pub fn value() -> u32 { base::value() }\n"
            } else if name == "unrelated" {
                std::fs::create_dir_all(cwd.join(path).join("src/foo")).unwrap();
                std::fs::write(cwd.join(path).join("src/foo/mod.rs"), "pub fn nested() {}\n").unwrap();
                "mod foo;\npub fn value() -> u32 { 9 }\n"
            } else {
                "pub fn value() -> u32 { 9 }\n"
            };
            std::fs::write(cwd.join(path).join("src/lib.rs"), source).unwrap();
        }

        let dir = format!("{}/rc/{}/shared", base.display(), cache_hash(&cwd));
        std::fs::create_dir_all(&dir).unwrap();
        let cargo = TestToolchain::current();
        let cargo_home = base.join("cargo-home");
        std::fs::create_dir_all(&cargo_home).unwrap();
        let lock = cargo
            .command()
            .args(["generate-lockfile", "--offline"])
            .current_dir(&cwd)
            .env("CARGO_HOME", &cargo_home)
            .output()
            .unwrap();
        assert!(
            lock.status.success(),
            "cargo generate-lockfile failed: {}",
            String::from_utf8_lossy(&lock.stderr)
        );
        use_real_rsync_for_fake_remote(&base);
        let sync = || {
            with_fake_transport(&base, || {
                sync_source(&remote, &data, &cwd, &dir, &Deadline::new(60))
            })
        };
        sync().expect("initial workspace sync should succeed");
        let check = || {
            let output = cargo
                .command()
                .args(["check", "--offline", "--workspace", "--target-dir"])
                .arg(Path::new(&dir).join("target"))
                .current_dir(&dir)
                .env("CARGO_HOME", &cargo_home)
                .env("CARGO_TERM_COLOR", "never")
                .output()
                .unwrap();
            let output_text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(output.status.success(), "cargo check failed: {output_text}");
            output_text
                .lines()
                .filter_map(|line| {
                    let line = line.trim_start();
                    line.strip_prefix("Checking ")
                        .or_else(|| line.strip_prefix("Compiling "))
                })
                .filter_map(|line| line.split_whitespace().next())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            check().len(),
            3,
            "the cold check should build all workspace packages"
        );

        std::fs::write(
            cwd.join("crates/base/src/lib.rs"),
            "include!(concat!(env!(\"OUT_DIR\"), \"/assets.rs\"));\npub fn value() -> u32 { ASSET_COUNT + 2 }\n",
        )
        .unwrap();
        sync().expect("one crate source-file change should sync");
        let mut checked = check();
        checked.sort();
        assert_eq!(
            checked,
            vec!["app".to_owned(), "base".to_owned()],
            "only the changed crate and its dependent should be checked"
        );

        std::fs::write(cwd.join("crates/base/src/assets/two.txt"), "two\n").unwrap();
        sync().expect("adding a regular build input must invalidate the workspace");
        let mut checked = check();
        checked.sort();
        assert_eq!(
            checked,
            vec!["app".to_owned(), "base".to_owned(), "unrelated".to_owned()],
            "an added file must rebuild possible consumers absent from old dep-info"
        );

        std::fs::remove_file(cwd.join("crates/base/src/assets/one.txt")).unwrap();
        sync().expect("deleting a regular build input should sync narrowly");
        let mut checked = check();
        checked.sort();
        assert_eq!(
            checked,
            vec!["app".to_owned(), "base".to_owned()],
            "the surviving parent-directory touch must rerun its build-script consumer"
        );

        std::fs::write(cwd.join("crates/unrelated/src/foo.rs"), "pub fn sibling() {}\n").unwrap();
        sync().expect("adding a Rust module candidate should synchronize with full invalidation");
        let ambiguous = cargo
            .command()
            .args(["check", "--offline", "--workspace", "--target-dir"])
            .arg(Path::new(&dir).join("target"))
            .current_dir(&dir)
            .env("CARGO_HOME", &cargo_home)
            .env("CARGO_TERM_COLOR", "never")
            .output()
            .unwrap();
        let ambiguity_output = format!(
            "{}{}",
            String::from_utf8_lossy(&ambiguous.stdout),
            String::from_utf8_lossy(&ambiguous.stderr)
        );
        assert!(!ambiguous.status.success(), "expected E0761 after adding an ambiguous module: {ambiguity_output}");
        assert!(ambiguity_output.contains("E0761"), "expected Rust module ambiguity E0761: {ambiguity_output}");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn run_keeps_project_cache_overrides_inside_the_lease() {
        if !has_program("rsync") {
            return;
        }
        let base = base();
        let cargo = TestToolchain::current();
        let (remote, cwd, data) = fake_remote(&base, 60, &format!(
            "RUSTC={} RUSTDOC={} CARGO_HOME={} exec {} \"$@\"\n",
            sh_quote(&cargo.rustc.display().to_string()),
            sh_quote(&cargo.rustdoc.display().to_string()),
            sh_quote(&base.join("cargo-home").display().to_string()),
            sh_quote(&cargo.cargo.display().to_string()),
        ));
        std::fs::write(cwd.join("src/lib.rs"), "pub fn value() {}\n").unwrap();
        std::fs::create_dir_all(cwd.join(".cargo")).unwrap();
        let outside_target = base.join("outside-target");
        let outside_target_alias = base.join("outside-target-alias");
        let outside_build = base.join("outside-build");
        // Model variables inherited by the SSH shell; the helper must remove them before Cargo
        // starts (some Cargo initialization happens before command-line config is applied).
        write_exec(&base.join("fakebin/ssh"), &format!(
            "for a; do last=$a; done\nHOME={} CARGO_TARGET_DIR={} CARGO_BUILD_TARGET_DIR={} CARGO_BUILD_BUILD_DIR={} exec setsid sh -c \"$last\"\n",
            sh_quote(&base.join("home").display().to_string()),
            sh_quote(&outside_target.display().to_string()),
            sh_quote(&outside_target_alias.display().to_string()),
            sh_quote(&outside_build.display().to_string()),
        ));
        std::fs::write(cwd.join(".cargo/config.toml"), format!(
            "[build]\ntarget-dir = {:?}\nbuild-dir = {:?}\n", outside_target, outside_build,
        )).unwrap();
        use_real_rsync_for_fake_remote(&base);
        let rc = with_fake_transport(&base, || run_offload(
            &remote, &data, &cwd, &["check".into(), "--offline".into()], &Deadline::new(60),
        ));
        assert_eq!(rc, 0, "Cargo should run with its cache inside the shared lease");
        let shared = base.join("rc").join(cache_hash(&cwd)).join("shared");
        assert!(shared.join("target/debug/.fingerprint").is_dir());
        assert!(!outside_target.exists(), "project target-dir must not evade invalidation: {}",
            String::from_utf8_lossy(&Command::new("find").arg(&outside_target).args(["-maxdepth", "5"]).output().unwrap().stdout));
        assert!(!outside_build.exists(), "project build-dir must not evade invalidation");
        assert!(!outside_target_alias.exists(), "inherited CARGO_BUILD_TARGET_DIR must not escape the lease");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn added_non_rust_input_rebuilds_a_compile_time_directory_listing() {
        if !has_program("rsync") {
            return;
        }
        let base = base();
        let (remote, cwd, data) = fake_remote(&base, 60, "exit 0\n");
        std::fs::write(cwd.join("Cargo.toml"), r#"[package]
name = "app"
version = "0.1.0"
edition = "2021"
[dependencies]
listing = { path = "listing" }
[workspace]
members = ["listing"]
"#).unwrap();
        std::fs::create_dir_all(cwd.join("listing/src")).unwrap();
        std::fs::write(cwd.join("listing/Cargo.toml"), r#"[package]
name = "listing"
version = "0.1.0"
edition = "2021"
[lib]
proc-macro = true
"#).unwrap();
        // Like asset embedding macros, this tracks every existing input through include_bytes!,
        // but a newly discovered input is absent from the previous compiler dep-info.
        std::fs::write(cwd.join("listing/src/lib.rs"), r#"extern crate proc_macro;
#[proc_macro]
pub fn count(_: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let root = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let mut inputs = std::fs::read_dir(root.join("assets")).unwrap()
        .map(|e| format!("include_bytes!({:?}).len()", e.unwrap().path()))
        .collect::<Vec<_>>();
    inputs.sort();
    format!("[{}].len()", inputs.join(",")).parse().unwrap()
}
"#).unwrap();
        std::fs::create_dir_all(cwd.join("assets")).unwrap();
        std::fs::write(cwd.join("assets/one.txt"), "one").unwrap();
        std::fs::write(cwd.join("src/lib.rs"), r#"pub const COUNT: usize = listing::count!();
#[test]
fn listing_matches_current_tree() {
    assert_eq!(COUNT, std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/assets")).unwrap().count());
}
"#).unwrap();
        let dir = base.join("rc").join(cache_hash(&cwd)).join("shared");
        std::fs::create_dir_all(&dir).unwrap();
        use_real_rsync_for_fake_remote(&base);
        let run = || {
            with_fake_transport(&base, || sync_source(&remote, &data, &cwd, &dir.display().to_string(), &Deadline::new(60)))
                .expect("source sync should succeed");
            let cargo = TestToolchain::current();
            let output = cargo
                .command()
                .args(["test", "--offline", "-p", "app", "--target-dir"])
                .arg(dir.join("target"))
                .args(["--", "listing_matches_current_tree"])
                .current_dir(&dir)
                .env("CARGO_HOME", base.join("cargo-home"))
                .output().unwrap();
            assert!(output.status.success(), "the embedded listing must match the synchronized tree: {}\n{}",
                String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        };
        run();
        std::fs::write(cwd.join("assets/two.txt"), "two").unwrap();
        run();
        std::fs::remove_file(cwd.join("assets/one.txt")).unwrap();
        run();
        let _ = std::fs::remove_dir_all(base);
    }

    fn set_mtime(path: &Path, modified: std::time::SystemTime) {
        std::fs::File::options().write(true).open(path).unwrap().set_times(std::fs::FileTimes::new().set_modified(modified)).unwrap();
    }

    fn add_linked_worktree(a: &Path, b: &Path) {
        let git = |args: &[&str]| {
            let out = Command::new("git").args(args).current_dir(a).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "cache-test@example.invalid"]);
        git(&["config", "user.name", "cache test"]);
        git(&["add", "-A"]);
        git(&["commit", "-qm", "initial"]);
        git(&["worktree", "add", "-q", "-b", "cache-test-b", b.to_str().unwrap()]);
    }

    /// 同 repo 的第二個同時呼叫無法拿 shared.lock 時用獨立 job target；任何時刻都沒有兩個 writer 指向同一 target。
    #[test]
    fn concurrent_worktrees_never_write_the_same_shared_target() {
        let base = base();
        let a = base.join("repo-a");
        std::fs::create_dir_all(a.join("src")).unwrap();
        std::fs::write(a.join("Cargo.toml"), "[package]\nname = \"p\"\nversion = \"0.1.0\"\n").unwrap();
        std::fs::write(a.join("src/value"), "A").unwrap();
        let b = base.join("repo-b");
        add_linked_worktree(&a, &b);
        let hash_a = cache_hash(&a);
        let hash_b = cache_hash(&b);
        assert_eq!(hash_a, hash_b, "both callers must contend for the same repo lease");

        let mut first = guard_with(&base, &hash_a, "job-1-1", QUICK);
        let mut second = guard_with(&base, &hash_b, "job-2-2", QUICK);
        assert_eq!(first.hs.dir, format!("{}/rc/{hash_a}/shared", base.display()));
        assert_eq!(second.hs.dir, format!("{}/rc/{hash_b}/job-2-2", base.display()));
        let a_target = PathBuf::from(&first.hs.dir).join("target");
        let b_target = PathBuf::from(&second.hs.dir).join("target");
        assert_ne!(a_target, b_target, "concurrent Cargo writers must never share target");
        std::fs::create_dir_all(&a_target).unwrap();
        std::fs::create_dir_all(&b_target).unwrap();
        std::fs::write(a_target.join("marker"), "A").unwrap();
        std::fs::write(b_target.join("marker"), "B").unwrap();
        second.finish();
        assert_eq!(std::fs::read(a_target.join("marker")).unwrap(), b"A", "the second writer must not corrupt shared artifacts");
        assert!(!b_target.exists(), "the isolated job target is removed after its lease");
        first.finish();
        assert_eq!(std::fs::read(a_target.join("marker")).unwrap(), b"A", "the shared cache remains intact");
        let _ = std::fs::remove_dir_all(base);
    }

    /// 兩棵 linked worktree 輪流使用同一個 target。B 的來源內容不同且 mtime 比舊產物早；rsync 必須更新遠端來源 mtime，
    /// 才能讓 Cargo 的時間戳 freshness check 重編，而不是沿用 A 的產物。
    #[test]
    fn worktrees_share_cache_and_older_changed_sources_rebuild() {
        if !has_program("rsync") {
            return;
        }
        let base = base();
        let (remote, a, data) = fake_remote(
            &base,
            60,
            "mkdir -p target\nif [ ! -f target/fingerprint ] || [ src/value -nt target/fingerprint ]; then\n  echo build >> \"$HOME/build-count\"\n  cp src/value target/built-from\n  touch target/fingerprint\nfi\nexit 0\n",
        );
        std::fs::write(a.join("src/value"), "tree A\n").unwrap();
        let b = base.join("worktree-b");
        add_linked_worktree(&a, &b);
        std::fs::write(b.join("src/value"), "tree B\n").unwrap();
        set_mtime(&b.join("src/value"), std::time::UNIX_EPOCH + Duration::from_secs(10));
        assert_eq!(cache_hash(&a), cache_hash(&b), "linked worktrees must share the repo cache key");

        use_real_rsync_for_fake_remote(&base);
        let rc_a = with_fake_transport(&base, || run_offload(&remote, &data, &a, &["check".into()], &Deadline::new(60)));
        assert_eq!(rc_a, 0, "worktree A should populate the shared target");
        let shared = base.join("rc").join(cache_hash(&a)).join("shared");
        let fingerprint = shared.join("target/fingerprint");
        assert!(fingerprint.is_file());
        set_mtime(&fingerprint, std::time::UNIX_EPOCH + Duration::from_secs(20));
        assert!(std::fs::metadata(b.join("src/value")).unwrap().modified().unwrap() < std::fs::metadata(&fingerprint).unwrap().modified().unwrap());

        let rc_b = with_fake_transport(&base, || run_offload(&remote, &data, &b, &["check".into()], &Deadline::new(60)));
        assert_eq!(rc_b, 0, "worktree B should use the same shared target");
        assert_eq!(std::fs::read(shared.join("src/value")).unwrap(), b"tree B\n");
        assert!(std::fs::metadata(shared.join("src/value")).unwrap().modified().unwrap() > std::time::UNIX_EPOCH + Duration::from_secs(20));
        assert_eq!(std::fs::read(shared.join("target/built-from")).unwrap(), b"tree B\n", "the stale target artifact must rebuild from B");
        assert_eq!(std::fs::read_to_string(base.join("home/build-count")).unwrap().lines().count(), 2, "A and then B both rebuild");
        let _ = std::fs::remove_dir_all(base);
    }

    /// 權限變更會被 rsync 套用，但檔案內容不重傳；同步後必須推進遠端 mtime，Cargo 才會重跑監看的 build script。
    #[test]
    fn chmod_only_rsync_change_reruns_a_cargo_build_script() {
        if !has_program("rsync") {
            return;
        }
        use std::os::unix::fs::PermissionsExt as _;

        let base = base();
        let (remote, cwd, data) = fake_remote(&base, 60, "exit 0\n");
        std::fs::write(
            cwd.join("Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = \"build.rs\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(cwd.join("tools")).unwrap();
        let tool = cwd.join("tools/probe");
        std::fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::write(
            cwd.join("build.rs"),
            r#"use std::{env, fs, os::unix::fs::PermissionsExt, path::PathBuf};
fn main() {
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let tool = root.join("tools/probe");
    println!("cargo:rerun-if-changed={}", tool.display());
    let executable = fs::metadata(&tool).unwrap().permissions().mode() & 0o111 != 0;
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    fs::write(out.join("mode.rs"), format!("pub const TOOL_EXECUTABLE: bool = {executable};")).unwrap();
    use std::io::Write as _;
    writeln!(fs::OpenOptions::new().create(true).append(true).open(out.join("runs")).unwrap(), "{executable}").unwrap();
}
"#,
        )
        .unwrap();
        std::fs::write(cwd.join("src/lib.rs"), "include!(concat!(env!(\"OUT_DIR\"), \"/mode.rs\"));\n").unwrap();
        let dir = format!("{}/rc/{}/shared", base.display(), cache_hash(&cwd));
        std::fs::create_dir_all(&dir).unwrap();
        use_real_rsync_for_fake_remote(&base);

        let sync = || with_fake_transport(&base, || sync_source(&remote, &data, &cwd, &dir, &Deadline::new(60)));
        sync().expect("initial source sync should succeed");
        let target = Path::new(&dir).join("target");
        let run_cargo = || {
            let output = TestToolchain::current()
                .command()
                .args(["check", "--offline", "--quiet", "--target-dir"])
                .arg(&target)
                .current_dir(&dir)
                .output()
                .unwrap();
            assert!(output.status.success(), "cargo check failed: {}", String::from_utf8_lossy(&output.stderr));
        };
        let runs_file = || {
            std::fs::read_dir(target.join("debug/build"))
                .unwrap()
                .flatten()
                .map(|entry| entry.path().join("out/runs"))
                .find(|path| path.is_file())
                .expect("build script should write its run counter")
        };
        run_cargo();
        let runs = runs_file();
        assert_eq!(std::fs::read_to_string(&runs).unwrap().lines().collect::<Vec<_>>(), ["false"]);
        let remote_tool = Path::new(&dir).join("tools/probe");
        // source 與 remote 的 mtime 都固定在過去：沒有真的 touch 時 remote 就還是這個時間（斷言會紅），
        // 有 touch 才會變成現在。不必等檔案系統的秒數精度，也不會因為 rsync 複製了「現在」的 source mtime 而假綠。
        set_mtime(&tool, std::time::UNIX_EPOCH + Duration::from_secs(10));
        set_mtime(&remote_tool, std::time::UNIX_EPOCH + Duration::from_secs(10));
        let before = std::fs::metadata(&remote_tool).unwrap().modified().unwrap();
        let source_mtime = std::fs::metadata(&tool).unwrap().modified().unwrap();
        // Cargo compares rerun-if-changed inputs against build/<hash>/output, not just
        // .fingerprint or .d. A clock rollback may leave only that baseline in the future.
        let build_output = runs.parent().unwrap().parent().unwrap().join("output");
        assert!(build_output.is_file());
        set_mtime(&build_output, std::time::SystemTime::now() + Duration::from_secs(3600));
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(std::fs::metadata(&tool).unwrap().modified().unwrap(), source_mtime, "chmod must not change the source mtime");
        sync().expect("permission-only source sync should succeed");
        let after = std::fs::metadata(&remote_tool).unwrap().modified().unwrap();
        assert!(after > before, "metadata-only rsync changes need a newer remote mtime");
        run_cargo();

        // Simulate rsync applying chmod before returning 23/24. The failed attempt must not
        // replace the last successful manifest; the next successful call still touches the file.
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o644)).unwrap();
        let baseline_manifest = with_fake_transport(&base, || {
            read_remote_manifest(&remote, &data, &dir, &Deadline::new(60))
        })
        .unwrap()
        .unwrap()
        .encode();
        std::fs::write(base.join(".rsync-fail-after-copy"), "3\n").unwrap();
        assert!(sync().is_err(), "three rsync 24 exits should fail the sync");
        let after_failed_manifest = with_fake_transport(&base, || {
            read_remote_manifest(&remote, &data, &dir, &Deadline::new(60))
        })
        .unwrap()
        .unwrap()
        .encode();
        assert_eq!(
            after_failed_manifest, baseline_manifest,
            "failed sync must preserve last successful manifest"
        );
        let failed_mtime = std::fs::metadata(&remote_tool).unwrap().modified().unwrap();
        assert_eq!(
            failed_mtime, after,
            "chmod applied by failed retries must not be treated as invalidated"
        );
        // 恢復同步的 touch 要能看得出來：兩邊先固定在過去（同上，不靠 sleep 跨秒數精度）。
        set_mtime(&tool, std::time::UNIX_EPOCH + Duration::from_secs(20));
        set_mtime(&remote_tool, std::time::UNIX_EPOCH + Duration::from_secs(20));
        sync().expect("the next successful sync must invalidate the earlier failed chmod");
        let recovered_mtime = std::fs::metadata(&remote_tool).unwrap().modified().unwrap();
        assert!(
            recovered_mtime > failed_mtime,
            "manifest difference from the last success must touch the source"
        );
        run_cargo();
        assert_eq!(std::fs::read_to_string(&runs).unwrap().lines().collect::<Vec<_>>(), ["false", "true", "false"], "Cargo must rerun after permission-only changes even when an earlier rsync retry failed");
        let _ = std::fs::remove_dir_all(base);
    }

    /// 長度前綴的二進位 manifest 可完整表示含 newline 的檔名，檔名內容也不能偽造成另一筆變更。
    #[test]
    fn source_manifest_handles_newline_names_without_forging_touch_paths() {
        if !has_program("rsync") {
            return;
        }
        use std::os::unix::ffi::OsStringExt as _;
        use std::os::unix::fs::PermissionsExt as _;

        let base = base();
        let (remote, cwd, data) = fake_remote(&base, 60, "exit 0\n");
        let name = std::ffi::OsString::from_vec(b"line\n[900] .f...p..... victim".to_vec());
        let source = cwd.join(&name);
        std::fs::write(&source, "script\n").unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o644)).unwrap();
        let dir = format!("{}/rc/{}/shared", base.display(), cache_hash(&cwd));
        std::fs::create_dir_all(&dir).unwrap();
        use_real_rsync_for_fake_remote(&base);

        let sync = || {
            with_fake_transport(&base, || {
                sync_source(&remote, &data, &cwd, &dir, &Deadline::new(60))
            })
        };
        sync().expect("first source manifest should synchronize");
        let remote_source = Path::new(&dir).join(&name);
        let remote_forged_paths = [Path::new(&dir).join("line"), Path::new(&dir).join("victim")];
        // 兩邊固定在過去（同 chmod 測試）：只有真的 touch 才會讓 remote 的 mtime 變新，不靠 sleep 跨秒數精度。
        set_mtime(&source, std::time::UNIX_EPOCH + Duration::from_secs(10));
        set_mtime(&remote_source, std::time::UNIX_EPOCH + Duration::from_secs(10));
        let before = std::fs::metadata(&remote_source)
            .unwrap()
            .modified()
            .unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o755)).unwrap();
        sync().expect("newline path metadata change should synchronize");

        let after = std::fs::metadata(&remote_source)
            .unwrap()
            .modified()
            .unwrap();
        assert!(
            after > before,
            "the exact newline-containing path must be invalidated"
        );
        for forged in &remote_forged_paths {
            assert!(!forged.exists(), "filename bytes must not forge a second path to touch: {forged:?}");
        }
        let manifest = with_fake_transport(&base, || {
            read_remote_manifest(&remote, &data, &dir, &Deadline::new(60))
        })
        .unwrap()
        .unwrap();
        use std::os::unix::ffi::OsStrExt as _;
        assert!(
            manifest.entries.contains_key(name.as_bytes()),
            "manifest must retain the complete filename bytes"
        );
        assert!(!manifest.entries.contains_key(b"victim".as_slice()), "the filename suffix must not become a second manifest path");
        let _ = std::fs::remove_dir_all(base);
    }

    struct TestToolchain {
        cargo: PathBuf,
        rustc: PathBuf,
        rustdoc: PathBuf,
    }

    impl TestToolchain {
        fn current() -> Self {
            // Cargo supplies its own executable path. Enumerating rustup installations can pick
            // an unrelated old release (CI also has 1.57), even when stable built these tests.
            let cargo = PathBuf::from(
                std::env::var_os("CARGO")
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| env!("CARGO").into()),
            );
            let bin = cargo.parent().expect("Cargo must have a parent directory");
            let tool = |key, name| {
                std::env::var_os(key)
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from)
                    .unwrap_or_else(|| bin.join(name))
            };
            Self {
                rustc: tool("RUSTC", "rustc"),
                rustdoc: tool("RUSTDOC", "rustdoc"),
                cargo,
            }
        }

        fn command(&self) -> Command {
            let mut command = Command::new(&self.cargo);
            // rustdoc loads proc-macro metadata too; leaving it on PATH can mix compiler versions.
            command.env("RUSTC", &self.rustc).env("RUSTDOC", &self.rustdoc);
            command
        }
    }

    /// 真 Cargo path dependency 的 A→B→A：同一個測試名稱每次讀取 rsync 過去的來源檔，並與編入 test artifact 的 crate 值比較。
    /// 若沿用另一棵 worktree 的 crate artifact，當前來源檔與編入的回傳值不同，該次測試就會失敗。
    #[test]
    fn path_dependency_worktrees_rebuild_the_matching_real_cargo_artifacts() {
        if !has_program("rsync") {
            return;
        }
        let base = base();
        let (remote, a, data) = fake_remote(&base, 60, "exit 0\n");
        std::fs::write(a.join("Cargo.toml"), "[workspace]\nmembers = [\"crates/am-base\", \"app\"]\nresolver = \"2\"\n").unwrap();
        std::fs::create_dir_all(a.join("crates/am-base/src")).unwrap();
        std::fs::write(
            a.join("crates/am-base/Cargo.toml"),
            "[package]\nname = \"am-base\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(a.join("crates/am-base/src/a.txt"), "B\n").unwrap();
        std::fs::write(a.join("crates/am-base/src/b.txt"), "B\n").unwrap();
        symlink("a.txt", a.join("crates/am-base/src/input.txt")).unwrap();
        std::fs::write(
            a.join("crates/am-base/src/lib.rs"),
            "pub fn marker() -> &'static str { include_str!(\"input.txt\").trim() }\n",
        )
        .unwrap();
        std::fs::create_dir_all(a.join("app/src")).unwrap();
        std::fs::write(a.join("app/Cargo.toml"), "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nam-base = { path = \"../crates/am-base\" }\n").unwrap();
        std::fs::write(a.join("app/src/lib.rs"), "#[cfg(test)] mod tests { #[test] fn source_matches_current_tree() { let source = std::fs::read_to_string(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/../crates/am-base/src/input.txt\")).unwrap(); assert_eq!(am_base::marker(), source.trim()); } }\n").unwrap();
        let b = base.join("worktree-b");
        add_linked_worktree(&a, &b);
        std::fs::write(a.join("crates/am-base/src/a.txt"), "A\n").unwrap();
        std::fs::remove_file(b.join("crates/am-base/src/input.txt")).unwrap();
        symlink("b.txt", b.join("crates/am-base/src/input.txt")).unwrap();
        let old_source_mtime = std::time::UNIX_EPOCH + Duration::from_secs(10);
        set_mtime(&a.join("crates/am-base/src/a.txt"), old_source_mtime);
        set_mtime(&b.join("crates/am-base/src/b.txt"), old_source_mtime);
        assert_eq!(cache_hash(&a), cache_hash(&b));
        let dir = base.join("rc").join(cache_hash(&a)).join("shared");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(base.join("cargo-home")).unwrap();
        let cargo = TestToolchain::current();
        let run = |tree: &Path| {
            let sync = with_fake_transport(&base, || sync_source(&remote, &data, tree, &dir.display().to_string(), &Deadline::new(60)));
            sync.expect("source tree sync should succeed");
            assert_eq!(
                std::fs::read_to_string(dir.join("crates/am-base/src/input.txt")).unwrap(),
                std::fs::read_to_string(tree.join("crates/am-base/src/input.txt")).unwrap(),
                "the shared source tree must match the current worktree before Cargo runs"
            );
            let output = cargo
                .command()
                .args(["test", "--offline", "-p", "app", "--target-dir"])
                .arg(dir.join("target"))
                .args(["--", "source_matches_current_tree"])
                .current_dir(&dir)
                .env("CARGO_HOME", base.join("cargo-home"))
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success() && stdout.contains("running 1 test") && stdout.contains("source_matches_current_tree"), "{tree:?}: {stdout}\n{stderr}");
        };
        use_real_rsync_for_fake_remote(&base);
        run(&a);
        run(&b);
        run(&a);
        let _ = std::fs::remove_dir_all(base);
    }

    /// Root-level directory symlinks can feed include_str! from another workspace package;
    /// any manifest difference must invalidate the consumer crate's Cargo fingerprint.
    #[test]
    fn cross_package_directory_symlink_rebuilds_the_matching_real_cargo_artifacts() {
        if !has_program("rsync") {
            return;
        }
        let base = base();
        let (remote, a, data) = fake_remote(&base, 60, "exit 0\n");
        std::fs::write(
            a.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/am-base\", \"app\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(a.join("crates/am-base/src")).unwrap();
        std::fs::write(
            a.join("crates/am-base/Cargo.toml"),
            "[package]\nname = \"am-base\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(a.join("crates/am-base/src/lib.rs"), "pub fn marker() -> &'static str { \"ok\" }\n").unwrap();
        std::fs::create_dir_all(a.join("app/src")).unwrap();
        std::fs::write(
            a.join("app/Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nam-base = { path = \"../crates/am-base\" }\n",
        )
        .unwrap();
        std::fs::write(
            a.join("app/src/lib.rs"),
            "pub const INCLUDED: &str = include_str!(\"../../assets/value.txt\");\n#[cfg(test)] mod tests { #[test] fn source_matches_current_tree() { let source = std::fs::read_to_string(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/../assets/value.txt\")).unwrap(); assert_eq!(super::INCLUDED.trim(), source.trim()); assert_eq!(am_base::marker(), \"ok\"); } }\n",
        )
        .unwrap();
        std::fs::create_dir_all(a.join("assets-a")).unwrap();
        std::fs::create_dir_all(a.join("assets-b")).unwrap();
        std::fs::write(a.join("assets-a/value.txt"), "asset A\n").unwrap();
        std::fs::write(a.join("assets-b/value.txt"), "asset B\n").unwrap();
        symlink("assets-a", a.join("assets")).unwrap();

        let b = base.join("worktree-b");
        add_linked_worktree(&a, &b);
        std::fs::remove_file(b.join("assets")).unwrap();
        symlink("assets-b", b.join("assets")).unwrap();
        let old_mtime = std::time::UNIX_EPOCH + Duration::from_secs(10);
        set_mtime(&a.join("assets-a/value.txt"), old_mtime);
        set_mtime(&a.join("assets-b/value.txt"), old_mtime);
        set_mtime(&b.join("assets-a/value.txt"), old_mtime);
        set_mtime(&b.join("assets-b/value.txt"), old_mtime);

        assert_eq!(cache_hash(&a), cache_hash(&b), "linked worktrees must share the repo cache key");
        let dir = base.join("rc").join(cache_hash(&a)).join("shared");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(base.join("cargo-home")).unwrap();
        let cargo = TestToolchain::current();
        let run = |tree: &Path| {
            let sync = with_fake_transport(&base, || {
                sync_source(&remote, &data, tree, &dir.display().to_string(), &Deadline::new(60))
            });
            sync.expect("source tree sync should succeed");
            let current = std::fs::read_to_string(tree.join("assets/value.txt")).unwrap();
            assert_eq!(std::fs::read_to_string(dir.join("assets/value.txt")).unwrap(), current);
            let output = cargo
                .command()
                .args(["test", "--offline", "-p", "app", "--target-dir"])
                .arg(dir.join("target"))
                .args(["--", "source_matches_current_tree"])
                .current_dir(&dir)
                .env("CARGO_HOME", base.join("cargo-home"))
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success() && stdout.contains("running 1 test") && stdout.contains("source_matches_current_tree"), "{tree:?}: {stdout}\n{stderr}");
        };
        use_real_rsync_for_fake_remote(&base);
        run(&a);
        run(&b);
        run(&a);
        let _ = std::fs::remove_dir_all(base);
    }

    /// 假 cargo 留下的 `cargo-started-<pid>`（在 `<root>/<hash>/`）裡的 pid。
    fn started_cargo_pids(base: &Path) -> Vec<i32> {
        let mut pids = Vec::new();
        for hash in std::fs::read_dir(base.join("rc")).into_iter().flatten().flatten() {
            for e in std::fs::read_dir(hash.path()).into_iter().flatten().flatten() {
                if let Some(pid) = e.file_name().to_string_lossy().strip_prefix("cargo-started-").and_then(|p| p.parse().ok()) {
                    pids.push(pid);
                }
            }
        }
        pids
    }

    /// 連線正常、遠端的 cargo 卡住（睡 300 秒）：超過上限（這裡 3 秒）就中止——回 124、遠端那顆 cargo 整組收掉、鎖與目錄還回去；
    /// 而且**不退回本機**（124 不是 shim 認的 125）。以前沒有上限：一直等到有人 Ctrl-C。
    #[test]
    fn a_remote_build_over_the_limit_is_aborted_and_its_remote_processes_are_reaped() {
        let base = base();
        let (remote, cwd, data) = fake_remote(&base, 3, "touch \"$PWD/../cargo-started-$$\"\nexec sleep 45\n");
        let t0 = Instant::now();
        let rc = with_fake_transport(&base, || run_offload(&remote, &data, &cwd, &["test".to_string()], &Deadline::new(remote.timeout_secs)));
        let took = t0.elapsed();
        assert_eq!(rc, 124, "超過上限要回 124（不是 125＝退回本機，也不是 126＝失敗）");
        assert!(took >= Duration::from_secs(3), "上限沒到不能中止：{took:?}");
        assert!(took < Duration::from_secs(60), "超過上限之後要很快收乾淨：{took:?}");
        let pids = started_cargo_pids(&base);
        assert_eq!(pids.len(), 1, "假 cargo 真的起來過：{pids:?}");
        wait_until("the remote cargo to be reaped", || unsafe { libc::kill(pids[0], 0) } != 0);
        // 守門收尾做完才回來：這次的 owner 撤掉了（＝目錄已還回去，晚到的 run 不會再跑）。
        let owners: Vec<_> = std::fs::read_dir(base.join("rc")).unwrap().flatten().flat_map(|h| std::fs::read_dir(h.path()).unwrap().flatten()).filter(|e| e.file_name().to_string_lossy().ends_with(".owner")).collect();
        assert!(owners.is_empty(), "目錄要還回去：{owners:?}");
        let _ = std::fs::remove_dir_all(base);
    }

    /// issue #428：ssh 連不進去（認證被拒、連不上、host key 不合）時，遠端**什麼都還沒跑**——
    /// 這時要退回本機（125，shim 才會去拿本機名額），而不是把呼叫端的編譯整個弄失敗（126）。
    /// 訊息要講得出「連的是誰、ssh 怎麼了、這次拿什麼登入」，結論要記進 data-dir 給 `/api/build-slots`。
    #[test]
    fn a_remote_that_refuses_the_login_falls_back_to_the_local_build_and_says_why() {
        let base = base();
        let (remote, cwd, data) = fake_remote(&base, 60, "exit 0\n");
        // 真的 ssh 被拒時就是這樣：訊息進 stderr、退出碼 255，stdout 一個字都沒有。
        // 多寫一行前置訊息：`reason` 要帶的是**最後**那一行（ssh 的結論），而且這一段等得到 stderr 讀完
        // （issue #467：以前沒等，Linux 上量到空字串、`reason` 變成「什麼都沒說」）。
        write_exec(
            &base.join("fakebin/ssh"),
            "echo 'OpenSSH_9.0, LibreSSL 3.3.6' >&2\necho 'me@fake: Permission denied (publickey,password).' >&2\nexit 255\n",
        );
        let rc = with_fake_transport(&base, || run_offload(&remote, &data, &cwd, &["test".to_string()], &Deadline::new(remote.timeout_secs)));
        assert_eq!(rc, EXIT_RUN_LOCALLY, "連不進去＝遠端沒動手，退回本機（不是 126＝驗證失敗）");

        let h = crate::remote_health::read(&data, "me@fake:22").expect("結論要記下來");
        assert!(!h.reachable, "{h:?}");
        assert_eq!(h.target, "me@fake:22", "記的是連哪一台（設定換主機之後看得出來是舊的）");
        let why = h.reason.as_deref().unwrap_or_default();
        assert!(why.contains("Permission denied"), "要帶 ssh 自己說的那一句，不是只有「回 255」：{h:?}");
        assert!(!why.contains("LibreSSL"), "帶的是最後那一行（結論），不是開頭的版本行：{h:?}");

        // 這一段就是印在 stderr 的東西：第一行要自己講完「連不上誰、改在本機跑」。
        let note = unreachable_note(&remote, &auth_desc(&remote, &data), why);
        let first = note.lines().next().unwrap();
        assert!(first.contains("me@fake:22") && first.contains("改在本機跑"), "第一行就要看得懂：{first}");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// #1853：遠端只有一個名額且被別的 worktree 佔滿，helper 必須讓 shim 排本機。
    #[test]
    fn a_full_remote_overflows_to_local_without_running_cargo() {
        let base = base();
        let (mut remote, cwd, data) = fake_remote(&base, 60, "exit 99\n");
        remote.max_concurrent = 1;
        let mut holder = guard_with(&base, "00000000000000a1", "job-1-1", Admission { max: 1, ..QUICK });
        let t0 = Instant::now();
        let rc = with_fake_transport(&base, || run_offload(&remote, &data, &cwd, &["test".to_string()], &Deadline::new(remote.timeout_secs)));
        assert_eq!(rc, EXIT_RUN_LOCALLY, "滿額不能回 75 或執行遠端 cargo");
        assert!(t0.elapsed() < Duration::from_secs(30), "應短暫等候後退回本機");
        assert!(crate::remote_health::read(&data, "me@fake:22").unwrap().reachable);
        let events = std::fs::read_to_string(data.join(am_base::remote_event::EVENT_FILE)).unwrap();
        let events: Vec<serde_json::Value> = events.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["outcome"], "local_fallback");
        assert_eq!(events[0]["host"], remote.host);
        assert!(events[0]["slot_wait_secs"].as_u64().is_some());
        holder.finish();
        let _ = std::fs::remove_dir_all(base);
    }

    /// 同一件事的另一面（issue #428）：連得上、是遠端的 cargo 自己失敗——那是**真的驗證失敗**，
    /// 原樣帶回它的退出碼，不能偷偷改成「在本機再跑一次」；而且這次算連得上。
    #[test]
    fn a_reachable_remote_whose_cargo_fails_is_not_a_fallback() {
        let base = base();
        let (remote, cwd, data) = fake_remote(&base, 60, "exit 101\n");
        let rc = with_fake_transport(&base, || run_offload(&remote, &data, &cwd, &["test".to_string()], &Deadline::new(remote.timeout_secs)));
        assert_eq!(rc, 101, "cargo 自己的退出碼原樣帶回，不是 125");
        let h = crate::remote_health::read(&data, "me@fake:22").expect("結論要記下來");
        assert!(h.reachable && h.reason.is_none(), "連得上：{h:?}");
        let events = std::fs::read_to_string(data.join(am_base::remote_event::EVENT_FILE)).unwrap();
        let events: Vec<serde_json::Value> = events.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(events.len(), 1, "record the selected lease even when cargo fails");
        assert_eq!(events[0]["outcome"], "shared");
        assert_eq!(events[0]["host"], remote.host);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn event_write_failure_or_lock_contention_does_not_change_build_result() {
        use std::os::unix::io::AsRawFd;
        for busy in [false, true] {
            let base = base();
            let (remote, cwd, data) = fake_remote(&base, 60, "exit 0\n");
            let lock = data.join(am_base::remote_event::LOCK_FILE);
            std::fs::create_dir_all(&data).unwrap();
            let holder = if busy {
                let file = std::fs::File::create(&lock).unwrap();
                assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
                Some(file)
            } else {
                std::fs::create_dir(&lock).unwrap();
                None
            };
            let rc = with_fake_transport(&base, || run_offload(&remote, &data, &cwd, &["test".into()], &Deadline::new(remote.timeout_secs)));
            assert_eq!(rc, 0, "event writer must not change a successful build");
            assert!(!data.join(am_base::remote_event::EVENT_FILE).exists());
            drop(holder);
            let _ = std::fs::remove_dir_all(base);
        }
    }

    /// 假 rsync：每跑一次在 `<base>/rsync-calls` 記一筆；第 N 次（1 起算）照 `fails` 第 N 個字元決定：`v`＝印 rsync 真的會印的
    /// ENOENT 那行、回 23；`p`＝權限錯、回 23；`x`＝回 12（協定錯）；超出長度＝照常複製。回傳 sync_source 的結果與呼叫次數。
    fn sync_with_fake_rsync(fails: &str) -> (anyhow::Result<()>, usize) {
        let base = base();
        let (remote, cwd, data) = fake_remote(&base, 60, "exit 0\n");
        let calls = base.join("rsync-calls");
        write_exec(
            &base.join("fakebin/rsync"),
            &format!(
                "echo x >> '{calls}'\nn=$(wc -l < '{calls}' | tr -d ' ')\nc=$(printf '%s' '{fails}' | cut -c\"$n\")\n\
                 case \"$c\" in\n\
                 v) echo \"rsync(95387): error: $PWD/web/dist/assets/index-Cy7N34fd.js: open (2) in $PWD: No such file or directory\" >&2; exit 23 ;;\n\
                 p) echo 'rsync: opendir \"/x\" failed: Permission denied (13)' >&2; exit 23 ;;\n\
                 x) exit 12 ;;\n\
                 esac\n\
                 for a; do src=$dest; dest=$a; done\nd=${{dest#*:}}\nmkdir -p \"$d\" && cp -a \"${{src}}.\" \"$d\"\n",
                calls = calls.display()
            ),
        );
        let dir = base.join("dest").display().to_string();
        let r = with_fake_transport(&base, || sync_source(&remote, &data, &cwd, &dir, &Deadline::new(60)));
        let n = std::fs::read_to_string(&calls).unwrap_or_default().lines().count();
        if r.is_ok() {
            assert!(base.join("dest/Cargo.toml").exists(), "成功就真的同步過去了");
        }
        let _ = std::fs::remove_dir_all(&base);
        (r, n)
    }

    /// am-lead 2026-09-25：別的 bot 在主樹 `bun run build`，rsync 列完清單後 `web/dist` 的舊 hash 檔不見 → exit 23，整個 cargo 判死。
    /// 來源在同步中途消失要短暫重試；一直消失才失敗、而且要講是來源樹被改動；其他 rsync 錯誤不重試。
    #[test]
    fn rsync_retries_only_when_the_source_changes_under_it() {
        let (r, n) = sync_with_fake_rsync("v");
        assert!(r.is_ok(), "第一次撞到檔案消失、第二次成功＝成功：{r:?}");
        assert_eq!(n, 2);

        let (r, n) = sync_with_fake_rsync("vvvvvv");
        let msg = format!("{:#}", r.expect_err("一直消失要失敗"));
        assert_eq!(n, SYNC_ATTEMPTS as usize, "重試有上限");
        assert!(msg.contains("exit Some(23)") && msg.contains("來源樹在同步時一直被改動") && msg.contains("web/dist"), "{msg}");

        for fails in ["p", "x"] {
            let (r, n) = sync_with_fake_rsync(fails);
            let msg = format!("{:#}", r.expect_err("別的錯誤照舊失敗"));
            assert_eq!(n, 1, "{fails}：不是來源消失就不重試");
            assert!(!msg.contains("來源樹"), "{fails}：不能誤報成來源被改動：{msg}");
        }
        assert!(source_changed_during_sync(Some(24), ""), "24＝vanished，一律算");
        assert!(!source_changed_during_sync(None, "No such file or directory"), "被訊號砍掉不算");
    }

    /// 訊息本身（純組字串）：這台機器 2026-09-24 的實況就是「沒設 identity_file、資料目錄也沒有密碼檔」，
    /// 於是 ssh 拿預設金鑰去連、被拒。看訊息的人要能直接知道少了什麼，而不是只看到一行 Permission denied。
    #[test]
    fn the_note_names_the_host_and_which_credentials_were_used() {
        let base = base();
        let data = base.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let mut remote = BuildRemoteCfg { enabled: true, host: "box".into(), user: "me".into(), ..Default::default() };

        let note = unreachable_note(&remote, &auth_desc(&remote, &data), "ssh 回 255");
        assert!(note.contains("me@box:22") && note.contains("ssh 回 255"), "{note}");
        assert!(note.lines().next().unwrap().contains("改在本機跑"), "第一行就要講接下來會怎麼樣：{note}");
        assert!(note.contains("identity_file") && note.contains("密碼檔"), "兩個都沒有就要兩個都點名：{note}");

        remote.identity_file = "/keys/build".into();
        let note = unreachable_note(&remote, &auth_desc(&remote, &data), "ssh 回 255");
        assert!(note.contains("/keys/build"), "設了金鑰就講是哪一把（被拒的是它）：{note}");

        std::fs::write(data.join(PASSWORD_FILE), "pw").unwrap();
        assert!(auth_desc(&remote, &data).contains("密碼檔"), "有密碼檔就先講密碼檔");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 沒超過上限的照常回傳結果（成功回 0、cargo 失敗回它的退出碼），不會被誤判成逾時。
    #[test]
    fn a_remote_build_within_the_limit_returns_its_own_result() {
        let base = base();
        let (remote, cwd, data) = fake_remote(&base, 60, "exit 101\n");
        let rc = with_fake_transport(&base, || run_offload(&remote, &data, &cwd, &["test".to_string()], &Deadline::new(remote.timeout_secs)));
        assert_eq!(rc, 101, "cargo 自己的退出碼原樣帶回");
        let _ = std::fs::remove_dir_all(&base);
        let (remote, cwd, data) = fake_remote(&base, 60, "exit 0\n");
        let rc = with_fake_transport(&base, || run_offload(&remote, &data, &cwd, &["check".to_string()], &Deadline::new(remote.timeout_secs)));
        assert_eq!(rc, 0);
        let _ = std::fs::remove_dir_all(base);
    }

    /// `timeout_secs = 0`＝不設上限：跑得比「一般的上限」久也照常等到結束。
    #[test]
    fn a_zero_limit_means_no_limit() {
        let base = base();
        let (remote, cwd, data) = fake_remote(&base, 0, "sleep 3\nexit 0\n");
        let rc = with_fake_transport(&base, || run_offload(&remote, &data, &cwd, &["test".to_string()], &Deadline::new(remote.timeout_secs)));
        assert_eq!(rc, 0);
        let _ = std::fs::remove_dir_all(base);
    }

    // ── 本機 helper 被砍時遠端也要停（issue #201）──

    /// 本機 helper 被 `kill -9`：kernel 收掉它所有的 fd，守門那條 ssh 的 stdin 關掉（**不是**我們自己做清理）——遠端還在跑的 cargo 整組要在
    /// 幾秒內消失，shared 的鎖也要放掉（不然同一棵 worktree 的下一次編譯會卡在等鎖，只能連到遠端手動清）。
    #[test]
    fn a_dead_helper_leaves_no_running_remote_process_and_releases_the_shared_lock() {
        let base = base();
        let home = fake_home(&base);
        let mut lease = guard(&base, HASH, "job-1-1"); // 搶到 shared
        let dir = lease.hs.dir.clone();
        assert!(dir.ends_with("/shared"), "{dir}");
        let mut run = spawn_run(&base, &home, &dir, &lease.token);
        wait_until("fake cargo to start", || started(&dir));
        let lock = format!("{dir}.lock");
        let lock_free = || Command::new("flock").args(["-n", &lock, "true"]).status().map(|s| s.success()).unwrap_or(false);
        assert!(!lock_free(), "cargo 在跑：鎖被守門持有");

        drop(lease.stdin.take()); // helper 死了：只剩連線中斷
        let status = wait_run(&mut run);
        assert!(status.signal().is_some() || !status.success(), "遠端的 cargo 要被收掉：{status:?}");
        wait_until("the shared lock to be released", lock_free);
        let _ = lease.child.wait();
        let _ = std::fs::remove_dir_all(base);
    }

    /// 離開了 process group 的殘留（cargo 底下自己 `setsid` 的輔助行程）`reap` 追不到：守門收尾時，還把 cwd 放在這個目錄裡的一併收掉。
    #[test]
    fn a_process_that_left_the_process_group_is_still_reaped_when_the_lease_ends() {
        let base = base();
        let home = fake_home(&base);
        write_exec(
            &home.join(".cargo/bin/cargo"),
            "touch \"$PWD/../cargo-started-$$\"\nsetsid sleep 45 >/dev/null 2>&1 &\necho $! > \"$PWD/../escaped.pid\"\nexec sleep 45\n",
        );
        let mut holder = guard(&base, HASH, "job-1-1"); // 佔住 shared，讓下一個拿 job 目錄
        let mut lease = guard(&base, HASH, "job-2-2");
        let dir = lease.hs.dir.clone();
        let mut run = spawn_run(&base, &home, &dir, &lease.token);
        let escaped_pid = || std::fs::read_to_string(Path::new(&dir).parent().unwrap().join("escaped.pid")).ok().and_then(|s| s.trim().parse::<i32>().ok());
        wait_until("the escaped helper to be recorded", || escaped_pid().is_some());
        let pid = escaped_pid().unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0, "離開 process group 的輔助行程還活著");

        drop(lease.stdin.take());
        assert!(lease.finish().unwrap().success());
        wait_until("the escaped helper to be reaped", || unsafe { libc::kill(pid, 0) } != 0);
        let _ = run.wait();
        holder.finish();
        let _ = std::fs::remove_dir_all(base);
    }

    /// 收到終止訊號（Ctrl-C／TERM／HUP）：helper 不再直接被打死、留下還連著的 ssh 與遠端的 cargo——把本機的 ssh／rsync 收掉、等守門把遠端那組行程
    /// 收乾淨，才用 `128 + 訊號` 結束。這裡用測試專用的 abort 旗標模擬「收到 SIGTERM」（真的訊號只有正式 helper 才看）。
    #[test]
    fn an_interrupted_helper_reaps_the_remote_group_before_it_exits() {
        let base = base();
        let (remote, cwd, data) = fake_remote(&base, 0, "touch \"$PWD/../cargo-started-$$\"\nexec sleep 45\n");
        let ctl = Deadline::new(0);
        let abort = ctl.abort.clone();
        let watcher = {
            let base = base.clone();
            std::thread::spawn(move || {
                wait_until("the remote cargo to start", || !started_cargo_pids(&base).is_empty());
                abort.store(true, std::sync::atomic::Ordering::SeqCst);
            })
        };
        let t0 = Instant::now();
        let rc = with_fake_transport(&base, || run_offload(&remote, &data, &cwd, &["test".to_string()], &ctl));
        watcher.join().unwrap();
        assert_eq!(rc, 128 + libc::SIGTERM, "被訊號中止：128＋訊號");
        assert!(t0.elapsed() < Duration::from_secs(60), "{:?}", t0.elapsed());
        let pids = started_cargo_pids(&base);
        assert_eq!(pids.len(), 1, "{pids:?}");
        wait_until("the remote cargo to be reaped", || unsafe { libc::kill(pids[0], 0) } != 0);
        let _ = std::fs::remove_dir_all(base);
    }

    // ── 遠端名額（issue #104）──

    /// 不同 worktree 共用同一組遠端名額：上限 2 時第三棵要排隊，前面有人還回名額它才拿到（拿到的就是空出來的那個）。
    #[test]
    fn different_worktrees_share_one_remote_cap() {
        let base = base();
        let adm = Admission { max: 2, queue_wait_secs: 60, ..QUICK };
        let mut a = guard_with(&base, "00000000000000a1", "job-1-1", adm);
        let mut b = guard_with(&base, "00000000000000b2", "job-2-2", adm);
        let mut got = vec![a.hs.slot, b.hs.slot];
        got.sort();
        assert_eq!(got, vec![Some((1, 2)), Some((2, 2))]);
        let (tx, rx) = std::sync::mpsc::channel();
        let b2 = base.clone();
        let waiter = std::thread::spawn(move || tx.send(guard_with(&b2, "00000000000000c3", "job-3-3", adm)).unwrap());
        assert!(rx.recv_timeout(Duration::from_millis(2500)).is_err(), "名額滿了，第三棵 worktree 要排隊");
        let freed = a.hs.slot;
        a.finish();
        let mut c = rx.recv_timeout(Duration::from_secs(15)).expect("a 還回名額之後 c 要排到");
        waiter.join().unwrap();
        assert_eq!(c.hs.slot, freed);
        b.finish();
        c.finish();
        let _ = std::fs::remove_dir_all(base);
    }

    /// 端到端（真的 `run_offload`＋真的守門）：四棵 worktree 同時跑、上限 2，任何時刻最多兩個遠端 cargo，而且四個都跑完、結果原樣帶回。
    /// 修正前沒有遠端上限：四個一起跑（2026-09-19 就是 9 個冷編譯同時跑把遠端壓垮）。
    /// 正式的排隊等待只有 3 秒（#1853），機器一忙前兩個超過 3 秒，後兩個就合法地退回本機（125）——這條驗的是上限與排隊，
    /// 所以每條執行緒各自把排隊等待設長（#1880：thread_local 不會跟著 spawn 走）。
    #[test]
    fn concurrent_calls_from_different_worktrees_never_exceed_the_remote_cap() {
        let base = base();
        let marks = base.join("running");
        std::fs::create_dir_all(&marks).unwrap();
        let body = format!(
            "touch '{m}/'$$\nls '{m}' | wc -l >> '{m}.log'\nsleep 2\nrm -f '{m}/'$$\nexit 0\n",
            m = marks.display()
        );
        let (mut remote, _, data) = fake_remote(&base, 60, &body);
        remote.max_concurrent = 2;
        let calls: Vec<_> = (0..4)
            .map(|i| {
                let cwd = base.join(format!("wt{i}"));
                std::fs::create_dir_all(&cwd).unwrap();
                std::fs::write(
                    cwd.join("Cargo.toml"),
                    format!("[package]\nname = \"wt{i}\"\nversion = \"0.1.0\"\n"),
                )
                .unwrap();
                let (remote, data, base) = (remote.clone(), data.clone(), base.clone());
                std::thread::spawn(move || {
                    TEST_QUEUE_WAIT_SECS.with(|w| w.set(Some(60)));
                    with_fake_transport(&base, || run_offload(&remote, &data, &cwd, &["test".to_string()], &Deadline::new(remote.timeout_secs)))
                })
            })
            .collect();
        let rcs: Vec<i32> = calls.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(rcs, vec![0; 4]);
        let seen: Vec<u32> = std::fs::read_to_string(base.join("running.log")).unwrap().lines().map(|l| l.trim().parse().unwrap()).collect();
        assert_eq!(seen.len(), 4, "{seen:?}");
        assert!(seen.iter().all(|n| *n <= 2), "同時在跑的遠端 cargo 超過上限 2：{seen:?}");
        assert!(seen.contains(&2), "兩個名額都該用上：{seen:?}");
        let _ = std::fs::remove_dir_all(base);
    }

    /// 排隊的時間不算進整體上限：上限 3 秒、前面的人佔名額 4 秒、自己的 cargo 跑 1 秒 → 照常成功（不是 124）。
    /// #1971：shared 被佔著的時候，正式會等 900 秒；測試密封後只等 [`SEALED_SHARED_WAIT_SECS`] 秒就改用獨立目錄（名額滿則退回本機）。
    /// 這條能在 30 秒內結束＝覆寫確實生效（沒生效就會等滿 900 秒）。
    #[test]
    fn a_shared_wait_is_capped_under_test() {
        let base = base();
        let (mut remote, cwd, data) = fake_remote(&base, 60, "exit 0\n");
        remote.max_concurrent = 1;
        let hash = cache_hash(&cwd);
        let mut holder = guard_with(&base, &hash, "job-1-1", Admission { max: 1, pool: 1, ..QUICK });
        let t0 = Instant::now();
        let rc = with_fake_transport(&base, || run_offload(&remote, &data, &cwd, &["test".to_string()], &Deadline::new(remote.timeout_secs)));
        let took = t0.elapsed();
        holder.finish();
        assert_eq!(rc, EXIT_RUN_LOCALLY, "shared 等完改獨立目錄，但唯一的名額還被佔著＝排隊逾時退回本機");
        assert!(took >= Duration::from_secs(SEALED_SHARED_WAIT_SECS - 1), "確實等了上限那麼久：{took:?}");
        assert!(took < Duration::from_secs(30), "shared 等待沒被密封：{took:?}");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn time_spent_queueing_does_not_count_toward_the_overall_limit() {
        let base = base();
        let (mut remote, cwd, data) = fake_remote(&base, 3, "sleep 1\nexit 0\n");
        remote.max_concurrent = 1;
        // #1853：名額只等 3 秒後會 overflow；改等同 worktree 的 shared 鎖，仍驗證排隊不算執行逾時。
        let hash = cache_hash(&cwd);
        let mut holder = guard_with(&base, &hash, "job-1-1", Admission { max: 1, ..QUICK });
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(4));
            holder.finish();
        });
        let t0 = Instant::now();
        let rc = with_fake_transport(&base, || run_offload(&remote, &data, &cwd, &["test".to_string()], &Deadline::new(remote.timeout_secs)));
        let took = t0.elapsed();
        release.join().unwrap();
        assert!(took >= Duration::from_secs(4), "真的有排隊：{took:?}");
        assert_eq!(rc, 0, "排隊 4 秒＋跑 1 秒，上限 3 秒只算後面那段");
        let _ = std::fs::remove_dir_all(base);
    }

    /// shared 被佔著時先等它用完（拿到同一個 shared、增量編譯），不是馬上改用冷編譯的 job 目錄；等超過上限才改用 job 目錄。
    #[test]
    fn a_busy_shared_target_is_waited_for_and_only_given_up_after_a_limit() {
        let base = base();
        let mut a = guard(&base, HASH, "job-1-1");
        let adir = a.hs.dir.clone();
        assert!(adir.ends_with("/shared"), "{adir}");
        let (tx, rx) = std::sync::mpsc::channel();
        let b2 = base.clone();
        let waiter = std::thread::spawn(move || {
            tx.send(guard_with(&b2, HASH, "job-2-2", Admission { shared_wait_secs: 30, ..QUICK })).unwrap()
        });
        assert!(rx.recv_timeout(Duration::from_secs(2)).is_err(), "shared 被佔著：要等，不是馬上冷編譯");
        a.finish();
        let mut b = rx.recv_timeout(Duration::from_secs(15)).expect("a 用完之後 b 要拿到");
        waiter.join().unwrap();
        assert_eq!(b.hs.dir, adir, "b 拿到同一個 shared，不是 job-2-2");

        // 佔著不放：等 2 秒還沒空出來就改用 job 目錄。
        let t0 = Instant::now();
        let mut c = guard_with(&base, HASH, "job-3-3", Admission { shared_wait_secs: 2, ..QUICK });
        assert!(c.hs.dir.ends_with(&format!("/{HASH}/job-3-3")), "{:?}", c.hs);
        // guard 腳本用 `date +%s`（整秒）算已等多久：上限 2 秒實際會在 1～2 秒之間放棄（負載高時更明顯），
        // 要擋的是「馬上改用冷編譯」，所以門檻是至少等過 1 秒，不是剛好 2 秒。
        assert!(t0.elapsed() >= Duration::from_secs(1), "{:?}", t0.elapsed());
        assert_eq!(c.obs.outcome.as_deref(), Some("isolated"));
        assert!(c.obs.shared_wait_secs.is_some_and(|seconds| seconds >= 1));
        assert!(c.obs.slot_wait_secs.is_some());
        c.finish();
        b.finish();
        let _ = std::fs::remove_dir_all(base);
    }

    /// 名額一直全滿：排到上限就放棄——不交目錄、exit 75（[`QueueFull`]），自己佔的 shared 鎖也放掉。
    ///
    /// 「至少等滿 `queue_wait_secs`」這條以前是碰運氣的（issue #486）：守門用 `date +%s` 算經過時間，
    /// truncate 到整秒會多算最多 1 秒，`qwait=2` 時真的只等約 1 秒就放棄，整樹平行下就偶發紅在下面那行
    /// （量到 1.04 秒，而守門自己的 log 還說「等了 2 秒」）。守門改成 `-le` 之後這條是確定的。
    #[test]
    fn a_queue_that_never_moves_gives_up_with_exit_75() {
        let base = base();
        let mut holder = guard_with(&base, "00000000000000a1", "job-1-1", Admission { max: 1, ..QUICK });
        let token = LeaseToken::new();
        let cmd = guard_cmd(&base, HASH, "job-2-2", &token, Admission { max: 1, queue_wait_secs: 2, ..QUICK });
        let t0 = Instant::now();
        let err = Lease::start(cmd, token, &Deadline::new(0)).err().expect("queue full");
        assert!(err.downcast_ref::<QueueFull>().is_some(), "{err:#}");
        assert!(
            t0.elapsed() >= Duration::from_secs(2) && t0.elapsed() < Duration::from_secs(60),
            "要等滿 queue_wait_secs 才放棄（issue #486：整秒時鐘會少等將近一秒），實際等了 {:?}",
            t0.elapsed()
        );
        assert!(lock_free(&base.join(format!("rc/{HASH}/shared.lock"))), "放棄時要放掉 shared 的鎖");
        holder.finish();
        let _ = std::fs::remove_dir_all(base);
    }

    /// 開一個會排隊的守門，等它打出第一聲心跳，然後像 helper 那條 ssh 斷掉時遠端 sshd 做的一樣：**只關它的 stdin**，
    /// stdout 繼續有人讀（2026-09-19 用真的 sshd 量：本機 ssh 被砍之後守門的 stdout 照樣寫得進去，只有 stdin 會讀到 EOF）。
    /// 它要在幾秒內自己結束。
    fn hang_up_while_queued(base: &Path, hash: &str, adm: Admission) {
        let token = LeaseToken::new();
        let mut child = guard_cmd(base, hash, "job-2-2", &token, adm)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut out = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        while line.trim_end() != "Q" {
            line.clear();
            assert!(out.read_line(&mut line).unwrap() > 0, "守門還沒排隊就結束了");
            assert!(!line.starts_with("ROOT"), "應該要排隊，不該拿到目錄");
        }
        drop(child.stdin.take());
        let drain = std::thread::spawn(move || std::io::copy(&mut out, &mut std::io::sink()));
        let deadline = Instant::now() + Duration::from_secs(10);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("helper 斷線了，排隊中的守門還一直佔著位置（{adm:?}）");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = drain.join();
    }

    /// 排隊中 helper 斷線了（被砍、Ctrl-C、網路斷）：守門靠心跳發現，自己結束、放掉手上的鎖，不佔著位置空等到上限。
    /// 兩種排隊都一樣：等遠端名額（這時它握著自己 worktree 的 shared 鎖）、等 shared。
    #[test]
    fn a_queued_guard_whose_helper_went_away_leaves_the_queue() {
        let base = base();
        let lock = base.join(format!("rc/{HASH}/shared.lock"));
        let mut slot_holder = guard_with(&base, "00000000000000a1", "job-1-1", Admission { max: 1, ..QUICK });
        hang_up_while_queued(&base, HASH, Admission { max: 1, queue_wait_secs: 120, ..QUICK });
        assert!(lock_free(&lock), "結束時放掉 shared 的鎖");
        slot_holder.finish();

        let mut shared_holder = guard(&base, HASH, "job-1-1");
        hang_up_while_queued(&base, HASH, Admission { shared_wait_secs: 120, ..QUICK });
        shared_holder.finish();
        let _ = std::fs::remove_dir_all(base);
    }

    /// #326：冷編譯的 job 排隊中被掛斷（或 queue full），`job-*.lock` 不能留在遠端沒人收。
    #[test]
    fn a_queued_job_that_gives_up_leaves_no_lock_file_behind() {
        let base = base();
        let mut holder = guard_with(&base, HASH, "job-1-1", Admission { max: 1, ..QUICK });
        hang_up_while_queued(&base, HASH, Admission { max: 1, queue_wait_secs: 120, ..QUICK });
        let lock = base.join(format!("rc/{HASH}/job-2-2.lock"));
        assert!(!lock.exists(), "排隊中放棄的 job 留下了 {}", lock.display());
        holder.finish();
        let _ = std::fs::remove_dir_all(base);
    }

    /// 守門腳本裡的自動上限跟文件寫的算式一致（這台就是遠端：用它自己的核數與 RAM 核對）。
    #[test]
    fn the_guards_automatic_cap_matches_the_documented_formula() {
        let base = base();
        let ncpu: u64 = String::from_utf8(Command::new("getconf").arg("_NPROCESSORS_ONLN").output().unwrap().stdout).unwrap().trim().parse().unwrap();
        let mem_kb: u64 = std::fs::read_to_string("/proc/meminfo")
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix("MemTotal:"))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
            .unwrap();
        for jobs in [1, 3, 8] {
            let mut g = guard_with(&base, HASH, "job-1-1", Admission { max: 0, jobs, ..QUICK });
            let want = super::tests::auto_max_concurrent(ncpu, mem_kb, jobs);
            assert_eq!(g.hs.slot.map(|s| s.1), Some(want), "jobs={jobs} ncpu={ncpu} mem_kb={mem_kb}");
            g.finish();
        }
        let _ = std::fs::remove_dir_all(base);
    }

    /// #1956：共用 target 是一個池（份數＝名額數）。第一份被佔著時下一個呼叫拿第二份——第一次用到時從第一份複製出來
    /// （連同 `.fingerprint` 與 source manifest），不是冷編譯；之後那份留著各自增量，不再重新複製。
    #[test]
    fn a_busy_shared_target_hands_the_next_call_a_seeded_copy_instead_of_a_cold_dir() {
        let base = base();
        let pool = Admission { max: 3, pool: 0, ..QUICK };
        let mut a = guard_with(&base, HASH, "job-1-1", pool);
        let adir = PathBuf::from(&a.hs.dir);
        assert!(a.hs.dir.ends_with("/shared"), "{:?}", a.hs);
        std::fs::create_dir_all(adir.join("target/debug/.fingerprint/p-1")).unwrap();
        std::fs::create_dir_all(adir.join("target/debug/deps")).unwrap();
        std::fs::write(adir.join("target/debug/.fingerprint/p-1/lib-p"), "fp").unwrap();
        std::fs::write(adir.join("target/debug/deps/libp.rlib"), "old").unwrap();
        std::fs::write(adir.join("Cargo.toml"), "[package]\n").unwrap();
        std::fs::write(format!("{}.source-manifest", a.hs.dir), "m").unwrap();

        let mut b = guard_with(&base, HASH, "job-2-2", pool);
        let bdir = PathBuf::from(&b.hs.dir);
        assert!(b.hs.dir.ends_with(&format!("/{HASH}/shared-2")), "第一份在用：拿池裡的第二份，不是 job 目錄：{:?}", b.hs);
        assert_eq!(std::fs::read_to_string(bdir.join("target/debug/deps/libp.rlib")).unwrap(), "old", "從第一份複製了產物");
        assert_eq!(std::fs::read_to_string(bdir.join("target/debug/.fingerprint/p-1/lib-p")).unwrap(), "fp", "指紋也一起複製");
        assert!(bdir.join("Cargo.toml").is_file(), "來源也一起");
        assert_eq!(std::fs::read_to_string(format!("{}.source-manifest", b.hs.dir)).unwrap(), "m", "manifest 跟著複製，失效 touch 只碰差異");
        assert!(!Path::new(&format!("{}.seed", b.hs.dir)).exists(), "暫存的 .seed 改名成正式的了");
        std::fs::write(bdir.join("target/own"), "b").unwrap();
        assert!(b.finish().unwrap().success());
        assert!(bdir.join("target/own").is_file(), "池裡的每一份都留著給下一次（不是用完就刪的 job 目錄）");
        assert!(a.finish().unwrap().success());

        // 都空著：照順序先拿第一份；第一份再被佔著時拿到的是已經在的第二份，不會重新複製蓋掉它。
        std::fs::write(adir.join("target/debug/deps/libp.rlib"), "new").unwrap();
        let mut c = guard_with(&base, HASH, "job-3-3", pool);
        assert_eq!(c.hs.dir, adir.display().to_string());
        let mut d = guard_with(&base, HASH, "job-4-4", pool);
        assert_eq!(d.hs.dir, bdir.display().to_string());
        assert!(bdir.join("target/own").is_file(), "已經在的那份不重新複製");
        assert_eq!(std::fs::read_to_string(bdir.join("target/debug/deps/libp.rlib")).unwrap(), "old");
        d.finish();
        c.finish();
        let _ = std::fs::remove_dir_all(base);
    }

    /// #1956：池全忙時每秒輪詢**每一份**，任何一份空出來就拿——不是只等第一份。
    #[test]
    fn a_full_pool_hands_out_whichever_copy_frees_up_first() {
        let base = base();
        let pool = Admission { max: 2, pool: 0, ..QUICK };
        let mut a = guard_with(&base, HASH, "job-1-1", pool);
        let mut b = guard_with(&base, HASH, "job-2-2", pool);
        assert!(b.hs.dir.ends_with("/shared-2"), "{:?}", b.hs);
        let bdir = b.hs.dir.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let b2 = base.clone();
        let waiter = std::thread::spawn(move || {
            tx.send(guard_with(&b2, HASH, "job-3-3", Admission { shared_wait_secs: 30, max: 3, pool: 2, ..QUICK })).unwrap()
        });
        assert!(rx.recv_timeout(Duration::from_secs(2)).is_err(), "兩份都在用：要等");
        b.finish();
        let mut c = rx.recv_timeout(Duration::from_secs(15)).expect("第二份空出來就要拿到");
        waiter.join().unwrap();
        assert_eq!(c.hs.dir, bdir, "拿到先空出來的那份（第二份），不是死等第一份");
        c.finish();
        a.finish();
        let _ = std::fs::remove_dir_all(base);
    }

    /// 池的其他份（`shared-<N>`）跟 `shared` 一樣算共用 target：回收照共用的門檻、計進 `max_shared_dirs`；長得像但不是的不碰。
    #[test]
    fn pool_copies_are_shared_targets_for_gc() {
        assert_eq!(leaf_kind("/r", "/r/0123456789abcdef/shared-2"), Some(LeafKind::Shared));
        assert_eq!(leaf_kind("/r", "/r/0123456789abcdef/shared-12"), Some(LeafKind::Shared));
        assert_eq!(leaf_kind("/r", "/r/0123456789abcdef/shared-"), None);
        assert_eq!(leaf_kind("/r", "/r/0123456789abcdef/shared-2.seed"), None);
        let dir = |leaf: &str, idle_secs| RemoteDir { path: format!("/r/0123456789abcdef/{leaf}"), locked: false, busy: false, idle_secs };
        let hs = Handshake {
            root: "/r".into(),
            dir: "/r/0123456789abcdef/shared-3".into(),
            dirs: vec![dir("shared", ORPHAN_IDLE_SECS + 5), dir("shared-2", ORPHAN_IDLE_SECS + 10)],
            ..Default::default()
        };
        let policy = GcPolicy { shared_idle_secs: 24 * 3600, max_shared: 2 };
        assert_eq!(select_gc(&hs, &policy), vec!["/r/0123456789abcdef/shared-2".to_string()], "連自己這份共 3 份、上限 2：收最久沒用的");
    }
}
