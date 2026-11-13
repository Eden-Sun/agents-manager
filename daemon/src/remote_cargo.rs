//! Developer-only whole-job Cargo offload (issue #104).
//!
//! V1 deliberately offloads verification commands (check/test/clippy) to one external SSH host.
//! It does not try to make a Linux/x86_64 binary runnable on macOS/aarch64, and therefore keeps
//! build/run/release work local.

use crate::cli::RemoteCargoEngine;
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

/// The selector is explicit; Blaze is accepted only for safe test/check invocations.
const EXIT_ENGINE_NOT_ENABLED: i32 = 2;
const BLAZE_ARGS_UNSUPPORTED_MESSAGE: &str = "remote Blaze supports only safe cargo check/test commands";

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
    eligible_with_engine(args, RemoteCargoEngine::Cargo).unwrap_or(false)
}

/// Engine-aware eligibility. Cargo keeps the existing verification route; Blaze accepts only
/// test/check invocations whose options cannot redirect or weaken the remote verification.
pub fn eligible_with_engine(args: &[String], engine: RemoteCargoEngine) -> Result<bool, &'static str> {
    match engine {
        RemoteCargoEngine::Cargo => Ok(eligible_cargo(args)),
        RemoteCargoEngine::Blaze => {
            if !eligible_cargo(args) {
                return Ok(false);
            }

            // Keep Cargo's global options ahead of its subcommand. `--color` consumes a value,
            // so that value must not be mistaken for the subcommand.
            let mut i = 0;
            while let Some(arg) = args.get(i).map(String::as_str) {
                match arg {
                    "-v" | "-vv" | "--verbose" | "-q" | "--quiet" | "--locked" => i += 1,
                    "--color" => i += 2,
                    _ if arg.starts_with("--color=") => i += 1,
                    _ => break,
                }
            }
            let sub = match args.get(i).map(String::as_str) {
                Some("c" | "check") => "check",
                Some("t" | "test") => "test",
                _ => return Ok(false),
            };

            let mut after_separator = false;
            let mut separator_args = false;
            let mut needs_value = false;
            for arg in args.iter().skip(i + 1).map(String::as_str) {
                if after_separator {
                    separator_args = true;
                    continue;
                }
                if arg == "--" {
                    after_separator = true;
                    continue;
                }
                if needs_value {
                    if arg.starts_with('-') {
                        return Ok(false);
                    }
                    needs_value = false;
                    continue;
                }

                // Never use a different toolchain, cache, target, profile, or offline view of
                // the source tree for an explicit Blaze verification.
                if arg.starts_with('+')
                    || matches!(
                        arg,
                        "-C"
                            | "--config"
                            | "--target-dir"
                            | "--target"
                            | "--release"
                            | "-r"
                            | "--profile"
                            | "--manifest-path"
                            | "--offline"
                            | "--frozen"
                            | "-Z"
                            | "-j"
                            | "--jobs"
                    )
                    || arg.starts_with("-Z")
                    || arg.starts_with("--target-dir=")
                    || arg.starts_with("--target=")
                    || arg.starts_with("--profile=")
                    || arg.starts_with("--manifest-path=")
                    || arg.starts_with("--config=")
                    || arg.starts_with("--jobs=")
                {
                    return Ok(false);
                }

                // Unknown options fail closed. The value-taking options below are the common
                // package/feature/target selectors; target redirection was rejected above.
                if arg.starts_with('-') {
                    match arg {
                        "-p" | "--package" | "--exclude" | "--features" | "--bin" | "--example" | "--test"
                        | "--bench" | "--message-format" | "--color" => needs_value = true,
                        "--workspace" | "--all" | "--all-targets" | "--all-features" | "--no-default-features"
                        | "--lib" | "--bins" | "--examples" | "--tests" | "--benches" | "--no-deps"
                        | "--keep-going" | "--no-run" | "--no-fail-fast" | "--doc" | "--show-output"
                        | "--ignored" | "--include-ignored" | "-v" | "-vv" | "--verbose" | "-q" | "--quiet"
                        | "--locked" => {}
                        _ if arg.starts_with("-p") && arg.len() > 2 => {}
                        _ if arg.starts_with("--package=")
                            || arg.starts_with("--exclude=")
                            || arg.starts_with("--features=")
                            || arg.starts_with("--bin=")
                            || arg.starts_with("--example=")
                            || arg.starts_with("--test=")
                            || arg.starts_with("--bench=")
                            || arg.starts_with("--message-format=")
                            || arg.starts_with("--color=") => {}
                        _ => return Ok(false),
                    }
                }
            }
            if needs_value || (sub == "check" && separator_args) {
                return Ok(false);
            }
            Ok(true)
        }
    }
}

fn eligible_cargo(args: &[String]) -> bool {
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
qftmp="$qf.tmp.$$"
trap 'rm -f "$qf" "$qftmp"' EXIT
# 試用（多台挑主機的第一輪）：池裡沒有空的份、或名額全滿就不等、不改走冷目錄；拿不到就回 `BUSY\t<排隊人數>`、exit 77。
busy() {
  q=0
  for f in "$root"/.queue/*; do
    [ -f "$f" ] || continue
    case "$f" in
      *.tmp.*)
        find "$f" -mmin +1 -exec flock -n {} rm -f {} \; 2>/dev/null
        continue
        ;;
    esac
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
if exec 6>>"$qftmp" && flock -n 6; then
  mv -f "$qftmp" "$qf"
else
  rm -f "$qftmp"
fi
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
#[cfg(test)]
fn run_script(dir: &str, sub: &str, token: &LeaseToken, jobs: usize, test_threads: usize, args: &[String]) -> String {
    run_script_with_engine(dir, sub, token, jobs, test_threads, RemoteCargoEngine::Cargo, args).expect("Cargo engine is enabled")
}

fn run_script_with_engine(
    dir: &str,
    sub: &str,
    token: &LeaseToken,
    jobs: usize,
    test_threads: usize,
    engine: RemoteCargoEngine,
    args: &[String],
) -> anyhow::Result<String> {
    let argv = args.iter().map(|a| sh_quote(a)).collect::<Vec<_>>().join(" ");
    let tok = token.as_str();
    let cache_path = format!("{dir}/target");
    let target_config = sh_quote(&format!("build.target-dir={cache_path:?}"));
    let build_config = sh_quote(&format!("build.build-dir={cache_path:?}"));
    match engine {
        RemoteCargoEngine::Cargo => {
            // Keep Cargo's cache under the inspected target without exporting cache overrides into
            // tests/build scripts that spawn another Cargo for an unrelated temporary project.
            // This is intentionally byte-for-byte the legacy Cargo script.
            Ok(format!(
                "d={dir}; g=$(ps -o pgid= -p $$ | tr -d ' ') && printf '%s\\n' \"$g\" > \"$d.pgid-{tok}\" \
                 && [ \"$(cat \"$d.owner\" 2>/dev/null)\" = {tok} ] \
                 || {{ echo 'agents-manager: 遠端工作目錄已經還回去了（helper 中途被砍？），不跑 cargo' >&2; exit 126; }}; \
                 cd \"$d\"/{sub} && PATH=\"$HOME/.cargo/bin:$PATH\" CARGO_BUILD_JOBS={jobs}{threads} env -u CARGO_TARGET_DIR -u CARGO_BUILD_TARGET_DIR -u CARGO_BUILD_BUILD_DIR cargo --config {target_config} --config {build_config} {argv}",
                dir = sh_quote(dir),
                target_config = target_config,
                build_config = build_config,
                sub = sh_quote(sub),
                jobs = jobs.max(1),
                threads = if test_threads > 0 { format!(" RUST_TEST_THREADS={test_threads}") } else { String::new() },
            ))
        }
        RemoteCargoEngine::Blaze => {
            if !eligible_with_engine(args, RemoteCargoEngine::Blaze).unwrap_or(false) {
                anyhow::bail!("{BLAZE_ARGS_UNSUPPORTED_MESSAGE}");
            }
            // Resolve tools only on the executor. The script stays inside the existing host
            // lease, verifies the installed capability marker, and invokes the Cargo subcommand
            // so it cannot start an unleased bare cargo-blaze process.
            Ok(format!(
                "d={dir}; g=$(ps -o pgid= -p $$ | tr -d ' ') && printf '%s\\n' \"$g\" > \"$d.pgid-{tok}\" \
                 && [ \"$(cat \"$d.owner\" 2>/dev/null)\" = {tok} ] \
                 || {{ echo 'agents-manager: 遠端工作目錄已經還回去了（helper 中途被砍？），不跑 cargo' >&2; exit 126; }}; \
                 cd \"$d\"/{sub} && {{ \
                   blaze_bin=\"${{AGM_BLAZE_BIN:-$HOME/.local/share/agents-manager/blaze/bin}}\"; \
                   PATH=\"$HOME/.cargo/bin:$PATH\"; export PATH; \
                   host_cargo=$(command -v cargo) && host_rustup=$(command -v rustup) || {{ echo 'agents-manager: host-local Cargo and rustup are required' >&2; exit 127; }}; \
                   PATH=\"$blaze_bin:$PATH\"; export PATH; \
                   if [ ! -x \"$host_rustup\" ]; then echo 'agents-manager: Blaze needs host-local rustup and Rust 1.99.0' >&2; exit 127; fi; \
                   if ! blaze_rustc=$(\"$host_rustup\" which --toolchain 1.99.0 rustc 2>/dev/null) || [ ! -f \"$blaze_rustc\" ] || [ ! -x \"$blaze_rustc\" ]; then echo 'agents-manager: Blaze requires host-local Rust 1.99.0' >&2; exit 127; fi; \
                   if [ -x \"$blaze_bin/cargo-blaze\" ]; then plugin=blaze; elif [ -x \"$blaze_bin/cargo-ra-jit\" ]; then plugin=ra-jit; else echo 'agents-manager: host-local Blaze plugin is not installed' >&2; exit 127; fi; \
                   case \"$plugin\" in blaze) engine_alias=\"${{CARGO_ALIAS_BLAZE:-}}\" ;; ra-jit) engine_alias=\"${{CARGO_ALIAS_RA_JIT:-}}\" ;; esac; \
                   if [ -n \"$engine_alias\" ]; then echo 'agents-manager: Cargo alias would override the verified Blaze plugin' >&2; exit 127; fi; \
                   RUSTUP_TOOLCHAIN=1.99.0; export RUSTUP_TOOLCHAIN; \
                   selected_plugin=$(\"$host_cargo\" --list --verbose 2>/dev/null | awk -v plugin=\"$plugin\" '$1 == plugin {{ sub(/^[[:space:]]*[^[:space:]]+[[:space:]]+/, \"\"); print }}'); \
                   if [ ! \"$selected_plugin\" -ef \"$blaze_bin/cargo-$plugin\" ]; then echo 'agents-manager: Cargo would select an unverified Blaze plugin or alias' >&2; exit 127; fi; \
                   capability=\"$blaze_bin/.agm-blaze-memory-v1\"; engine_path=\"$blaze_bin/ra-jit\"; \
                   if [ ! -f \"$capability\" ] || [ ! -x \"$engine_path\" ] || [ \"$engine_path\" -nt \"$capability\" ]; then echo 'agents-manager: a capability-verified host-local Blaze is required' >&2; exit 127; fi; \
                   RUSTUP_TOOLCHAIN=1.99.0; export RUSTUP_TOOLCHAIN; \
                   blaze_runtime_base=\"${{AGM_BLAZE_RUNTIME_HOME:-$HOME/.cache/agents-manager/blaze}}\"; \
                   RA_JIT_HOME=\"$blaze_runtime_base/native-base-v1\"; BLAZE_HOME=\"$RA_JIT_HOME\"; \
                   RA_JIT_RUSTC_BASE=0; BLAZE_RUSTC_BASE=0; \
                   export RA_JIT_HOME BLAZE_HOME RA_JIT_RUSTC_BASE BLAZE_RUSTC_BASE; \
                   RUSTC=\"$blaze_rustc\"; CARGO_BUILD_RUSTC=\"$blaze_rustc\"; RUSTC_WRAPPER=; RUSTC_WORKSPACE_WRAPPER=; CARGO_BUILD_RUSTC_WRAPPER=; CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER=; \
                   export RUSTC CARGO_BUILD_RUSTC RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER CARGO_BUILD_RUSTC_WRAPPER CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER; \
                   CARGO_BUILD_JOBS={jobs}{threads} env -u CARGO_TARGET_DIR -u CARGO_BUILD_TARGET_DIR -u CARGO_BUILD_BUILD_DIR \"$host_cargo\" --config \"build.rustc=\\\"$blaze_rustc\\\"\" --config 'build.rustc-wrapper=\"\"' --config 'build.rustc-workspace-wrapper=\"\"' --config {target_config} --config {build_config} \"$plugin\" {argv}; \
                 }}",
                dir = sh_quote(dir),
                target_config = target_config,
                build_config = build_config,
                sub = sh_quote(sub),
                jobs = jobs.max(1),
                threads = if test_threads > 0 { format!(" RUST_TEST_THREADS={test_threads}") } else { String::new() },
            ))
        }
    }
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
}

#[derive(Debug)]
struct SourceInvalidationReport {
    action: SourceInvalidation,
    safe_file_count: usize,
    touch_paths: BTreeSet<PathBuf>,
    fallback: BTreeMap<&'static str, ManifestDiffCause>,
    targeted: BTreeMap<&'static str, usize>,
}

impl SourceInvalidationReport {
    fn record_fallback(&mut self, cause: &'static str) {
        self.fallback.entry(cause).or_default().count += 1;
    }

    fn record_targeted(&mut self, cause: &'static str) {
        *self.targeted.entry(cause).or_default() += 1;
    }

    fn log(&self) {
        if let Some(message) = self.diagnostic() {
            eprintln!("{message}");
        }
    }

    fn diagnostic(&self) -> Option<String> {
        match &self.action {
            SourceInvalidation::Unchanged => None,
            SourceInvalidation::Files(paths) => {
                let mut categories = vec![format!("safe regular-file changes ({})", self.safe_file_count)];
                categories.extend(self
                    .targeted
                    .iter()
                    .map(|(cause, count)| format!("{cause} ({count})"))
                );
                Some(format!(
                    "agents-manager: source manifest: targeted touch ({} paths; {})",
                    paths.len(),
                    categories.join("; "),
                ))
            }
            SourceInvalidation::Workspace => {
                let reasons = self
                    .fallback
                    .iter()
                    .map(|(cause, detail)| format!("{cause} ({})", detail.count))
                    .collect::<Vec<_>>();
                let targeted = self
                    .targeted
                    .iter()
                    .map(|(cause, count)| format!("{cause} ({count})"))
                    .collect::<Vec<_>>();
                let targeted_note = if targeted.is_empty() { String::new() } else { format!("; targeted categories: {}", targeted.join("; ")) };
                Some(format!(
                    "agents-manager: source manifest: full-workspace touch fallback: {} ({} safe regular-file changes){targeted_note}",
                    reasons.join("; "),
                    self.safe_file_count,
                ))
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
/// while absent from old dep-info, so additions force workspace invalidation except ordinary
/// entries below the verified `web/dist` RustEmbed folder. Its stable source anchor is accepted
/// only when it is a regular non-symlink file with the expected RustEmbed derive and folder.
/// Cargo's dep-info detects missing file inputs,
/// while parent-directory mtimes cover build-script watches for directory contents. Structural
/// changes outside that embed folder, Cargo controls, and symlinks stay global. A non-web regular
/// file removal remains narrow by itself, but combined with embed structure changes it is global.
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
        safe_file_count: 0,
        touch_paths: BTreeSet::new(),
        fallback: BTreeMap::new(),
        targeted: BTreeMap::new(),
    };
    let Some(previous) = previous else {
        report.record_fallback("missing successful baseline");
        report.action = SourceInvalidation::Workspace;
        return report;
    };
    if previous == current {
        return report;
    }
    let embed_ui_anchor = verified_web_dist_embed_anchor(root);
    let mut non_web_regular_file_removals = 0;

    // A directory symlink can feed include_* inputs in another package. A link outside the tree
    // is also unknowable from this manifest. If either exists, any source change gets full touch.
    for (bytes, entry) in &current.entries {
        if entry.kind != b'L' {
            continue;
        }
        let Some(path) = path_from_bytes(bytes) else {
            report.record_fallback("unrepresentable symlink path");
            continue;
        };
        let Ok(target) = root.join(path).canonicalize() else {
            report.record_fallback("unresolved symlink");
            continue;
        };
        if !target.starts_with(root) {
            report.record_fallback("symlink target outside workspace");
        } else if target.is_dir() {
            report.record_fallback("directory symlink");
        }
    }

    for (bytes, now) in &current.entries {
        let Some(before) = previous.entries.get(bytes) else {
            if is_cargo_control_path(bytes) {
                report.record_fallback("Cargo control input added");
                continue;
            }
            if let Some(anchor) = embed_ui_anchor
                .as_ref()
                .filter(|_| is_web_dist_structural_entry(bytes, now.kind))
            {
                report.record_targeted("embedded UI entry added");
                report.touch_paths.insert(anchor.clone());
                continue;
            }
            // Compiler dep-info may list only existing files discovered by a proc macro, so a new
            // asset (any extension) can change an expansion without a tracked directory: those still
            // fall back. Two additions are narrowed. A new Rust source under a package's src/, tests/,
            // benches/ or examples/ is a module or target file, not a scanned asset: touch it and the
            // package's crate roots, so only that package recompiles (this also surfaces a new
            // `foo.rs` colliding with `foo/mod.rs`). A new directory adds nothing by itself; any file
            // inside it is classified on its own entry.
            match now.kind {
                b'F' => match safe_source_path(bytes).and_then(|path| added_rust_source_roots(current, &path).map(|roots| (path, roots))) {
                    Some((path, roots)) => {
                        report.record_targeted("Rust source added");
                        report.touch_paths.extend(roots);
                        report.touch_paths.insert(path);
                    }
                    None => report.record_fallback("file added"),
                },
                b'D' if safe_source_path(bytes).is_some() => report.record_targeted("directory added"),
                b'D' => report.record_fallback("added directory"),
                b'L' => report.record_fallback("symlink added"),
                _ => report.record_fallback("added non-file path"),
            }
            continue;
        };
        if before == now {
            continue;
        }
        if before.kind != now.kind {
            report.record_fallback("file type changed");
            continue;
        }
        if before.kind != b'F' || now.kind != b'F' {
            report.record_fallback("non-file entry changed");
            continue;
        }
        if is_cargo_control_path(bytes) {
            report.record_fallback("Cargo control input changed");
            continue;
        }
        let Some(path) = safe_source_path(bytes) else {
            report.record_fallback("unsafe file path");
            continue;
        };
        report.safe_file_count += 1;
        report.touch_paths.insert(path);
    }
    for bytes in previous.entries.keys() {
        if !current.entries.contains_key(bytes) {
            let before = &previous.entries[bytes];
            if is_cargo_control_path(bytes) {
                report.record_fallback("Cargo control input removed");
                continue;
            }
            if is_web_dist_structural_entry(bytes, before.kind) {
                if let Some(anchor) = &embed_ui_anchor {
                    report.record_targeted("embedded UI entry removed");
                    report.touch_paths.insert(anchor.clone());
                } else {
                    report.record_fallback("unverified web/dist structural entry removed");
                }
                continue;
            }
            if before.kind == b'F' && !is_cargo_control_path(bytes) {
                if let Some(path) = safe_source_path(bytes) {
                    let parent = source_parent(&path);
                    if parent_dir_exists_in_manifest(current, &parent) {
                        non_web_regular_file_removals += 1;
                        report.safe_file_count += 1;
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
            report.record_fallback(cause);
        }
    }

    if non_web_regular_file_removals > 0
        && (report.targeted.contains_key("embedded UI entry added")
            || report.targeted.contains_key("embedded UI entry removed"))
    {
        report.record_fallback("non-web regular files removed with embedded UI structure change");
    }

    if report.fallback.is_empty() && report.touch_paths.is_empty() && report.targeted.is_empty() {
        report.record_fallback("unclassified manifest difference");
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

const WEB_DIST_ROOT: &[u8] = b"web/dist";
const WEB_DIST_PREFIX: &[u8] = b"web/dist/";
const WEB_DIST_EMBED_ANCHOR: &str = "crates/am-base/src/assets.rs";
const WEB_DIST_EMBED_DECLARATION: [&str; 5] = [
    "#[cfg(feature = \"embed-ui\")]",
    "#[derive(rust_embed::Embed)]",
    "#[folder = \"../../web/dist\"]",
    "#[allow_missing = true]",
    "struct WebAssets;",
];
const WEB_DIST_EMBED_PREAMBLE_USES: [&str; 2] = [
    "use axum::http::{header, StatusCode, Uri};",
    "use axum::response::{IntoResponse, Response};",
];

fn is_web_dist_structural_entry(bytes: &[u8], kind: u8) -> bool {
    matches!(kind, b'D' | b'F')
        && safe_source_path(bytes).is_some()
        && (bytes == WEB_DIST_ROOT || bytes.starts_with(WEB_DIST_PREFIX))
}

fn verified_web_dist_embed_anchor(root: &Path) -> Option<PathBuf> {
    let relative = Path::new(WEB_DIST_EMBED_ANCHOR);
    let anchor = root.join(relative);
    let metadata = std::fs::symlink_metadata(&anchor).ok()?;
    let file_type = metadata.file_type();
    if !file_type.is_file() || file_type.is_symlink() {
        return None;
    }
    let source = std::fs::read_to_string(anchor).ok()?;
    let mut lines = source.lines().map(str::trim);
    while let Some(line) = lines.next() {
        if line.is_empty() || line.starts_with("//") || WEB_DIST_EMBED_PREAMBLE_USES.contains(&line) {
            continue;
        }
        if line != WEB_DIST_EMBED_DECLARATION[0] {
            return None;
        }
        for wanted in &WEB_DIST_EMBED_DECLARATION[1..] {
            if lines.next().map(str::trim) != Some(*wanted) {
                return None;
            }
        }
        return Some(relative.to_path_buf());
    }
    None
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

/// For a new `.rs` file under `<package>/{src,tests,benches,examples}/`, the existing crate-root
/// sources (`src/lib.rs`, `src/main.rs`, `build.rs`) of the nearest package with a `Cargo.toml` in
/// the manifest. Touching them recompiles that package's targets. `None` when the path is not such
/// a Rust source or the package has no root to touch.
fn added_rust_source_roots(manifest: &SourceManifest, path: &Path) -> Option<Vec<PathBuf>> {
    use std::os::unix::ffi::OsStrExt as _;
    if path.extension().is_none_or(|ext| ext != "rs") {
        return None;
    }
    let is_file = |p: &Path| manifest.entries.get(p.as_os_str().as_bytes()).is_some_and(|e| e.kind == b'F');
    let mut dir = path.parent();
    while let Some(d) = dir {
        if is_file(&d.join("Cargo.toml")) {
            let first = path.strip_prefix(d).ok()?.components().next()?;
            if !matches!(first.as_os_str().to_str(), Some("src" | "tests" | "benches" | "examples")) {
                return None;
            }
            let roots: Vec<PathBuf> = ["src/lib.rs", "src/main.rs", "build.rs"]
                .iter()
                .map(|r| d.join(r))
                .filter(|p| is_file(p))
                .collect();
            return (!roots.is_empty()).then_some(roots);
        }
        dir = d.parent();
    }
    None
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

fn run_remote(
    remote: &BuildRemoteCfg,
    data_dir: &Path,
    lease: &Lease,
    sub: &str,
    args: &[String],
    engine: RemoteCargoEngine,
    deadline: &Deadline,
) -> anyhow::Result<i32> {
    let threads = pick_test_threads(std::env::var("RUST_TEST_THREADS").ok().as_deref(), remote.test_threads);
    let script = run_script_with_engine(&lease.hs.dir, sub, &lease.token, remote.cargo_jobs, threads, engine, args)?;
    let mut cmd = host_shell(remote, data_dir, &script)?;
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

/// Cargo-default wrapper retained for existing callers and host-selection code.
pub fn decide_offload(config_path: &Path, args: &[String], env: impl IntoIterator<Item = (String, String)>) -> Offload {
    decide_offload_with_engine(config_path, args, env, RemoteCargoEngine::Cargo).expect("Cargo engine is enabled")
}

/// Engine-aware offload decision. The not-enabled error is distinct from local-decline code 125.
pub fn decide_offload_with_engine(
    config_path: &Path,
    args: &[String],
    env: impl IntoIterator<Item = (String, String)>,
    engine: RemoteCargoEngine,
) -> Result<Offload, &'static str> {
    if !eligible_with_engine(args, engine)? {
        return Ok(Offload::Local(None));
    }
    Ok(decide_offload_eligible(config_path, env))
}

/// Config and host checks after the engine and Cargo argv have been accepted.
fn decide_offload_eligible(config_path: &Path, env: impl IntoIterator<Item = (String, String)>) -> Offload {
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
    run_cli_with_engine(config_path, data_dir, cwd, args, only, RemoteCargoEngine::Cargo)
}

/// Single main.rs forwarding target. Reject unsupported engines before list-hosts or config access.
pub fn run_command_cli(
    config_path: &Path,
    data_dir: Option<&Path>,
    cwd: &Path,
    args: &[String],
    only: Option<&str>,
    list_hosts: bool,
    engine: RemoteCargoEngine,
) -> i32 {
    if let Err(why) = ensure_engine_enabled(engine, if list_hosts { &[] } else { args }) {
        eprintln!("agents-manager: {why}");
        return EXIT_ENGINE_NOT_ENABLED;
    }
    if list_hosts {
        return list_hosts_cli(config_path);
    }
    run_cli_with_engine(config_path, data_dir, cwd, args, only, engine)
}

/// Engine-aware entry point. Reject invalid explicit Blaze argv before configuration or host selection.
pub fn run_cli_with_engine(
    config_path: &Path,
    data_dir: Option<&Path>,
    cwd: &Path,
    args: &[String],
    only: Option<&str>,
    engine: RemoteCargoEngine,
) -> i32 {
    if engine == RemoteCargoEngine::Blaze && !eligible_with_engine(args, engine).unwrap_or(false) {
        eprintln!("agents-manager: {BLAZE_ARGS_UNSUPPORTED_MESSAGE}");
        return EXIT_ENGINE_NOT_ENABLED;
    }
    let hosts = match decide_offload_with_engine(config_path, args, std::env::vars(), engine) {
        Err(why) => {
            eprintln!("agents-manager: {why}");
            return EXIT_ENGINE_NOT_ENABLED;
        }
        Ok(Offload::Go(hosts)) => match only_host(hosts, only) {
            Some(hosts) => hosts,
            None => {
                eprintln!("agents-manager: 這次 cargo 不轉外部編譯：設定裡沒有建置主機 {}", only.unwrap_or(""));
                return EXIT_RUN_LOCALLY;
            }
        },
        Ok(Offload::Local(why)) => {
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
    run_offload_on_with_id_and_engine(&hosts, data_dir, cwd, args, &Deadline::new(hosts[0].timeout_secs).watching_signals(), &call_id, engine)
}

fn ensure_engine_enabled(engine: RemoteCargoEngine, args: &[String]) -> Result<(), &'static str> {
    match engine {
        RemoteCargoEngine::Cargo => Ok(()),
        RemoteCargoEngine::Blaze if eligible_with_engine(args, engine).unwrap_or(false) => Ok(()),
        RemoteCargoEngine::Blaze => Err(BLAZE_ARGS_UNSUPPORTED_MESSAGE),
    }
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
    run_offload_on_with_id_and_engine(hosts, data_dir, cwd, args, ctl, &call_id, RemoteCargoEngine::Cargo)
}

fn run_offload_on_with_id_and_engine(
    hosts: &[BuildRemoteCfg],
    data_dir: &Path,
    cwd: &Path,
    args: &[String],
    ctl: &Deadline,
    call_id: &str,
    engine: RemoteCargoEngine,
) -> i32 {
    if let Err(why) = ensure_engine_enabled(engine, args) {
        eprintln!("agents-manager: {why}");
        return EXIT_ENGINE_NOT_ENABLED;
    }
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
        let code = run_remote(remote, data_dir, &lease, &sub, args, engine, ctl)?;
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
#[path = "remote_cargo_tests/tests.rs"]
mod tests;

/// 守門腳本的真實行為（#141）。要 flock 與 /proc，只在 Linux 跑——remote Cargo 本身就是把測試丟到
/// Linux 主機執行，所以這組在那台會真的跑到；不走 ssh，直接 `sh -c` 同一份腳本。
#[cfg(all(test, target_os = "linux"))]
#[path = "remote_cargo_tests/guard_tests.rs"]
mod guard_tests;
