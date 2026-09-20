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
use std::io::{BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::Arc;

const PASSWORD_FILE: &str = "remote-cargo-password";

/// 整體上限最多設到一天（0＝不設上限）。
const MAX_TIMEOUT_SECS: u64 = 24 * 3600;

/// `[build.remote] max_concurrent` 最多設到這麼多。
const MAX_CONCURRENT: usize = 64;

/// 超過整體上限時 helper 的結束碼（跟 GNU `timeout` 一樣是 124）。shim 只把 125 當成「退回本機」，所以不會在本機重跑一次。
pub const EXIT_TIMEOUT: i32 = 124;

/// 排遠端名額排超過 [`QUEUE_WAIT_SECS`] 時 helper 的結束碼（EX_TEMPFAIL：稍後再試；跟 shim 自己「排程器用不了」的 75 同一個意思）。
/// 這時遠端什麼都還沒跑；原因守門已經印在 stderr。
pub const EXIT_QUEUE_FULL: i32 = 75;

/// 測試用：把 `ssh`／`rsync` 換成假腳本（本機當遠端）。只有測試 build 有這個入口，而且是**執行緒本地**的——並行的其他測試不受影響。
#[cfg(test)]
thread_local! {
    static TEST_PROGRAMS: std::cell::RefCell<Option<(String, String)>> = const { std::cell::RefCell::new(None) };
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
    })
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
    if input.enabled && (host.is_empty() || user.is_empty()) {
        return Err(LcError::Bad("remote Cargo 啟用時 host 與 user 都必填".into()));
    }
    let safe_host = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':');
    let safe_user = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_');
    let safe_root = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '/');
    if !host.chars().all(safe_host) || host.starts_with('-') {
        return Err(LcError::Bad("remote Cargo host 含不支援的字元".into()));
    }
    if !user.chars().all(safe_user) || user.starts_with('-') {
        return Err(LcError::Bad("remote Cargo user 含不支援的字元".into()));
    }
    if !remote_root.is_empty() && (!remote_root.chars().all(safe_root) || remote_root.contains("..")) {
        return Err(LcError::Bad("remote_root 只能使用英數、._-/，且不可含 ..".into()));
    }
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
    .map_err(|e| LcError::Upstream(format!("store remote Cargo settings: {e:#}")))?;
    Ok(Json(sanitized(&remote, &app.data_dir)))
}

pub async fn test_settings(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let cfg = app.cfg.get().await;
    let remote = cfg.build.remote.clone();
    if remote.host.trim().is_empty() || remote.user.trim().is_empty() {
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

fn probe(remote: &BuildRemoteCfg, data_dir: &Path) -> anyhow::Result<Value> {
    let pw = secret(data_dir, remote)?;
    let mut cmd = ssh_base(remote, pw.as_deref(), data_dir)?;
    cmd.arg(PROBE_SH);
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
    let pw = secret(data_dir, remote)?;
    let mut cmd = ssh_base(remote, pw.as_deref(), data_dir)?;
    cmd.arg(INSTALL_SH);
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
    if remote.host.trim().is_empty() || remote.user.trim().is_empty() {
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
    const EXACT: [&str; 9] = [
        "RUSTFLAGS",
        "RUSTDOCFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_ENCODED_RUSTDOCFLAGS",
        "CARGO_TARGET_DIR",
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

pub fn sh_quote(s: &str) -> String {
    if s.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_./:=@+".contains(&c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

// ── 遠端工作目錄（issue #141）──────────────────────────────────────────────────────────────
//
// 版面：`<remote_root>/<worktree 路徑的 fnv1a64>/`
//   - `shared/`＋`shared.lock`：同一棵 worktree 共用的原始碼＋`target/`，跨呼叫保留（增量編譯）。
//   - `job-<pid>-<ms>/`＋`.lock`：`shared` 被另一次呼叫佔著、等了 [`SHARED_WAIT_SECS`] 還沒空出來時的退路，冷編譯，結束就刪。
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
// 租約身分（issue #148）是本機每次呼叫新產生的 [`LeaseToken`]（128 位元隨機），不是遠端 shell 的 `$$`：
// PID 會被回收重用，「值一樣」不等於「同一次租約」。守門只把 `$$` 拿來做行程簿記（自己的 process group、
// `/proc/$$/fd`），`.owner`、`.pgid-<token>` 與交給 run 的 token 全都用這個。

/// 沒有鎖、也沒有行程的 cwd 在裡面，放這麼久就當孤兒刪（守門被 kill 在遠端那頭、舊版留下的 `<pid>/`）。
const ORPHAN_IDLE_SECS: u64 = 10 * 60;
/// `shared/` 是快取不是垃圾：閒置這麼久才收（worktree 早就合併刪掉的那些）。
const SHARED_IDLE_SECS: u64 = 3 * 3600;
/// 遠端磁碟剩不到這個比例時，閒置的 `shared/` 也照孤兒的門檻收（#141：一小時塞滿 97G）。
const LOW_DISK_FREE_PCT: u64 = 25;

/// `shared/` 被同一棵 worktree 的另一次呼叫佔著時，最多等這麼久才改用冷編譯的 `job-*`（issue #104）。
/// 等它用完再增量編譯只要十幾秒；搶不到就冷編譯，不但自己要重編幾分鐘，還多佔一份遠端的 RAM 與 2～3G 磁碟——
/// 等得比一次冷編譯還久就不划算了，所以抓一次冷編譯的時間。
const SHARED_WAIT_SECS: u64 = 5 * 60;
/// 遠端名額全滿時最多排這麼久（issue #104）。每個佔著名額的編譯都受 `timeout_secs`（預設 12 分鐘）管，正常兩輪內就排得到；
/// 排更久＝有名額被卡住（例如守門那條連線半開，遠端 sshd 要等 TCP keepalive 約 2 小時才發現），回 [`EXIT_QUEUE_FULL`]，不無限等。
const QUEUE_WAIT_SECS: u64 = 30 * 60;
/// `max_concurrent = 0`（自動）的記憶體帳：留給系統這麼多，每個遠端編譯算 [`AUTO_PER_JOB_KB`]。
/// 2026-09-19 在 32 vCPU／64 GiB 的遠端量：一次冷的 `cargo test -p agents-managerd` 光主 crate 那顆 rustc 就多吃約 4.6 GiB，
/// 兩次同時跑時全機用到 8.2 GiB；再加上測試本身（8 個執行緒、各自起子行程）與 lib／測試 harness 兩顆大 rustc 重疊，一次抓 8 GiB。
const AUTO_RESERVE_KB: u64 = 8 * 1024 * 1024;
const AUTO_PER_JOB_KB: u64 = 8 * 1024 * 1024;

/// 守門的排隊參數（issue #104）。正式呼叫由設定與上面的常數組成（[`Admission::from_cfg`]）；測試用短的等待時間。
#[derive(Debug, Clone, Copy)]
struct Admission {
    /// 同時最多幾個遠端編譯；`0`＝守門依遠端的核數與 RAM 算：
    /// `min(核數 × 1.5 ÷ cargo_jobs, (RAM − 8 GiB) ÷ 8 GiB)`，至少 1。CPU 容許 1.5 倍超賣（一次編譯大多時間只有一顆 rustc 在跑，
    /// `cargo_jobs` 是尖峰），記憶體不超賣——2026-09-19 就是 9 個冷編譯同時跑把遠端壓垮。32 核／64 GiB、`cargo_jobs = 8` → 6。
    max: usize,
    /// 自動算時每個編譯算幾個核（`cargo_jobs`）。
    jobs: usize,
    shared_wait_secs: u64,
    queue_wait_secs: u64,
}

impl Admission {
    fn from_cfg(remote: &BuildRemoteCfg) -> Self {
        Admission {
            max: remote.max_concurrent,
            jobs: remote.cargo_jobs.max(1),
            shared_wait_secs: SHARED_WAIT_SECS,
            queue_wait_secs: QUEUE_WAIT_SECS,
        }
    }
}

/// 一次租約的身分（fencing token）：本機產生的 128 位元隨機數，固定 32 個小寫 hex。
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
        .replace("@RESERVE_KB@", &AUTO_RESERVE_KB.to_string())
        .replace("@PER_JOB_KB@", &AUTO_PER_JOB_KB.to_string())
        .replace("@HEX16@", &"[0-9a-f]".repeat(16))
}

const GUARD_SH: &str = r#"set -u
trap '' HUP PIPE
root=@ROOT@; hash=@HASH@; job=@JOB@; token=@TOKEN@
max=@MAX@; jobs=@JOBS@; swait=@SWAIT@; qwait=@QWAIT@
if ! command -v flock >/dev/null 2>&1 || [ ! -r /proc/self/stat ]; then
  echo 'agents-manager: remote Cargo 的遠端要有 flock（util-linux）與 /proc（Linux）' >&2; exit 2
fi
mkdir -p "$root" && cd "$root" && root=$(pwd -P) || exit 2
# 名額數 0＝依這台的核數與 RAM 算（算式與理由見 helper 的 `Admission::max`）。
if [ "$max" -eq 0 ]; then
  cpu=$(getconf _NPROCESSORS_ONLN 2>/dev/null); case "$cpu" in ''|*[!0-9]*) cpu=1 ;; esac
  mem=$(awk '/^MemTotal:/ { print $2 }' /proc/meminfo 2>/dev/null); case "$mem" in ''|*[!0-9]*) mem=0 ;; esac
  max=$((cpu * 3 / (2 * jobs))); b=$(((mem - @RESERVE_KB@) / @PER_JOB_KB@))
  [ "$b" -lt "$max" ] && max=$b
  [ "$max" -ge 1 ] || max=1
fi
# `$$` 只做行程簿記（自己的 process group 不能被收、`/proc/$$/fd`）；租約身分是本機給的 $token，PID 會被回收重用。
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
  { flock -n 9 && same "$1.lock" 9 && ! cwds | grep -qF "$1/" && rm -rf "$1" && rm -f "$1.owner" "$1".pgid-* "$1.lock"; } 9>>"$1.lock"
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
    flock -n 8 && return 0
    [ $(($(date +%s) - h0)) -lt "$1" ] || return 75
  done
}
# 遠端名額（issue #104）：`$root/.slots/<n>` 的 flock，fd 7，拿著直到守門結束（怎麼死都會放）。
# 同一個 remote_root 不分 worktree 最多 $max 個；全滿就每秒再試，排超過 $qwait 秒回 74。
slot=0
take_slot() {
  mkdir -p "$root/.slots" || return 2
  q0=$(date +%s); said=
  while :; do
    i=1
    while [ "$i" -le "$max" ]; do
      exec 7>>"$root/.slots/$i"
      if flock -n 7; then
        slot=$i
        [ -z "$said" ] || echo "agents-manager: 排到遠端名額 $i/$max（等了 $(($(date +%s) - q0)) 秒）" >&2
        return 0
      fi
      exec 7>&-
      i=$((i + 1))
    done
    [ $(($(date +%s) - q0)) -lt "$qwait" ] || return 74
    [ -n "$said" ] || { echo "agents-manager: 遠端同時編譯已滿（$max 個，[build.remote] max_concurrent），排隊等名額（最多 $qwait 秒）…" >&2; said=1; }
    alive || return 73
  done
}
# 目錄先、名額後：等 shared 的人不佔名額；拿著名額的人不會再等任何鎖（順序一致，不會互等）。
hold() {
  lock_dir "$2" || return $?
  same "$1.lock" 8 || return 76
  take_slot || return $?
  mkdir -p "$1" && touch "$1.lock" && printf '%s\n' "$token" > "$1.owner" || return 2
  printf 'ROOT\t%s\nDIR\t%s\t%s\nSLOT\t%s\t%s\n' "$root" "$1" "$token" "$slot" "$max"
  inventory "$1"
  echo END
  # 本機隨時可能已經斷線：之後再寫 stdout/stderr 會 EPIPE，清理不能因此半途而廢。
  exec >/dev/null 2>&1
  while read -r op p; do [ "$op" = rm ] && gc_rm "$p" "$1"; done
  # 先撤 owner 再收 pgid：晚到的 run 若通過了 owner 檢查，它的 pgid 一定已經寫好了。
  rm -f "$1.owner"
  reap "$1"
  sweep "$1"
  rm -f "$1".pgid-*
  if [ "${1##*/}" = shared ]; then touch "$1.lock"; else rm -rf "$1"; rm -f "$1.lock"; fi
  return 0
}
queue_full() {
  echo "agents-manager: 等遠端名額等了 $qwait 秒還是全滿（$max 個），這次沒跑；稍後再試（結束碼 75）" >&2
  exit 75
}
# 73＝排隊中發現本機已經斷線：沒人要結果了，直接結束（鎖跟著放）。
n=0; s0=$(date +%s)
while :; do
  mkdir -p "$root/$hash" || exit 2
  w=$((swait - ($(date +%s) - s0))); [ "$w" -gt 0 ] || w=0
  hold "$root/$hash/shared" "$w" 8>>"$root/$hash/shared.lock"; rc=$?
  case $rc in
    0|73) exit 0 ;;
    74) queue_full ;;
    75) break ;;
  esac
  n=$((n + 1)); [ $n -lt 5 ] || exit $rc
done
[ "$swait" -eq 0 ] || echo "agents-manager: 等了 $swait 秒共用遠端 target 還沒空出來" >&2
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

/// 讀到 `END` 為止。登入 shell 的雜訊行略過；還沒拿到 `DIR` 就 EOF＝守門失敗（原因在它的 stderr）。
pub fn read_handshake(r: &mut impl BufRead) -> anyhow::Result<Handshake> {
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
            _ => {}
        }
    }
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
    if leaf == "shared" {
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
        let total = hs.dirs.iter().filter(is_shared).count() + usize::from(hs.dir.ends_with("/shared"));
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
    /// 這次租約的身分（本機產生、守門原樣回；[`run_script`] 用它核對 `.owner`）。
    token: LeaseToken,
}

/// 排遠端名額排超過上限，守門沒交目錄就結束了（exit 75）。
#[derive(Debug)]
struct QueueFull;

impl std::fmt::Display for QueueFull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("remote Cargo queue wait exceeded its limit")
    }
}

impl std::error::Error for QueueFull {}

impl Lease {
    /// `cmd` 跑的守門腳本要是用 `token` 產生的；它交回來的 token 對不上就當守門出事、不當成拿到目錄。
    ///
    /// 名額全滿或 shared 被佔著時，守門要排隊，可能很久才交出目錄（issue #104）：交握放到另一條執行緒讀，
    /// 這裡每 20ms 看一次 `ctl`，收到終止訊號就砍掉本機這條 ssh（排隊中的守門靠心跳發現斷線，自己結束、放鎖）。
    fn start(mut cmd: Command, token: LeaseToken, ctl: &Deadline) -> anyhow::Result<Self> {
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());
        let mut child = cmd.spawn().map_err(|e| anyhow::anyhow!("remote workdir guard: {e}"))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or_else(|| anyhow::anyhow!("remote workdir guard: no stdout"))?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let hs = read_handshake(&mut reader);
            let _ = tx.send((reader, hs));
        });
        let (reader, hs) = loop {
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
        let mut lease = Lease { child, stdin, stdout: Some(reader), hs: Handshake::default(), token };
        lease.hs = match hs {
            Ok(hs) => hs,
            Err(e) => {
                // 沒交目錄就結束了：75＝排隊超過上限（原因守門已經印在 stderr），其他是守門出事。
                if lease.finish().and_then(|st| st.code()) == Some(EXIT_QUEUE_FULL) {
                    return Err(anyhow::Error::new(QueueFull));
                }
                return Err(e);
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
    // `~/.cargo/bin` 要自己接：rustup 只改 shell profile，ssh 的非互動 shell 不讀（同 `PROBE_SH`）。
    format!(
        "d={dir}; g=$(ps -o pgid= -p $$ | tr -d ' ') && printf '%s\\n' \"$g\" > \"$d.pgid-{tok}\" \
         && [ \"$(cat \"$d.owner\" 2>/dev/null)\" = {tok} ] \
         || {{ echo 'agents-manager: 遠端工作目錄已經還回去了（helper 中途被砍？），不跑 cargo' >&2; exit 126; }}; \
         cd \"$d\"/{sub} && PATH=\"$HOME/.cargo/bin:$PATH\" CARGO_BUILD_JOBS={jobs}{threads} cargo {argv}",
        dir = sh_quote(dir),
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
        unsafe { libc::signal(sig, on_signal as usize as libc::sighandler_t) };
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
    let mut child = cmd.spawn().map_err(|e| anyhow::anyhow!("{what}: {e}"))?;
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

/// 遠端該同步（也是遠端目錄 hash 的種子）的根，以及 cwd 相對於它的路徑（`""`＝cwd 就是根）。
///
/// 跟 cargo 一樣往上找：最近一層宣告 `[workspace]` 的 `Cargo.toml` 就是工作區根；一路上都沒有就用最近的那個套件根；
/// 完全不在 cargo 專案裡就是 cwd 本身。只同步 cwd（#177）的話，`cd daemon && cargo check` 到了遠端沒有根的
/// `Cargo.lock`、`[profile.*]`、`.cargo/config.toml`——本機用鎖定的依賴，遠端重新解析、驗的不是同一份東西。
fn source_root(cwd: &Path) -> (PathBuf, String) {
    let mut package: Option<&Path> = None;
    let mut root: Option<&Path> = None;
    for dir in cwd.ancestors() {
        let Ok(manifest) = std::fs::read_to_string(dir.join("Cargo.toml")) else { continue };
        package.get_or_insert(dir);
        if manifest.parse::<toml::Table>().is_ok_and(|t| t.contains_key("workspace")) {
            root = Some(dir);
            break;
        }
    }
    let root = root.or(package).unwrap_or(cwd);
    let sub = cwd.strip_prefix(root).map(|r| r.to_string_lossy().into_owned()).unwrap_or_default();
    (root.to_path_buf(), sub)
}

fn open_lease(remote: &BuildRemoteCfg, data_dir: &Path, cwd: &Path, ctl: &Deadline) -> anyhow::Result<Lease> {
    let root = match remote.remote_root.trim().trim_end_matches('/') {
        "" => crate::config::default_remote_build_root(),
        r => r.to_string(),
    };
    let hash = format!("{:016x}", fnv1a64(&cwd.to_string_lossy()));
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let job = format!("job-{}-{millis}", std::process::id());
    let pw = secret(data_dir, remote)?;
    let token = LeaseToken::new();
    let mut cmd = ssh_base(remote, pw.as_deref(), data_dir)?;
    cmd.arg(format!("sh -c {}", sh_quote(&guard_script(&root, &hash, &job, &token, Admission::from_cfg(remote)))));
    Lease::start(cmd, token, ctl)
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

/// 同步原始碼時排除的東西（#328）。`target` 要**錨定在根**：不錨定的 `--exclude target` 會連 `src/target/mod.rs`、`crates/target/`、
/// `tests/fixtures/target/` 這些原始碼一起濾掉，遠端編不過或行為不同。代價：巢狀的獨立 crate 建置輸出（`fuzz/target` 之類）
/// 會被一起送（rsync 3.2 沒有 `--exclude-if-present`，認不出它們）；那是樹裡真實存在的東西，送過去不會錯。
const RSYNC_EXCLUDES: [&str; 4] = ["--exclude", ".git", "--exclude", "/target"];

fn sync_source(remote: &BuildRemoteCfg, data_dir: &Path, cwd: &Path, dir: &str, deadline: &Deadline) -> anyhow::Result<()> {
    if !has_program(&rsync_program()) {
        anyhow::bail!("remote Cargo requires `rsync` on the daemon host");
    }
    check_identity(remote)?;
    let pw = secret(data_dir, remote)?;
    let mode = match pw {
        Some(_) => Some(password_mode(data_dir)?),
        None => None,
    };
    let ssh = rsync_rsh(remote, pw.is_some(), matches!(mode, Some(PwMode::Askpass(_))));
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
    let source = format!("{}/", cwd.to_string_lossy().trim_end_matches('/'));
    let dest = format!("{}@{}:{}/", remote.user, remote.host, dir.trim_end_matches('/'));
    cmd.args(["-az", "--delete"]).args(RSYNC_EXCLUDES).args(["-e", &ssh])
        .arg(source)
        .arg(dest);
    let status = run_status(cmd, "rsync source", deadline)?;
    if !status.success() {
        anyhow::bail!("rsync failed with exit {:?}", status.code());
    }
    Ok(())
}

/// 測試執行緒數：呼叫端自己設了 `RUST_TEST_THREADS`（正整數）就尊重它，沒有才用設定（`0`＝不設）。
fn pick_test_threads(caller: Option<&str>, configured: usize) -> usize {
    caller.and_then(|v| v.trim().parse::<usize>().ok()).filter(|n| *n > 0).unwrap_or(configured)
}

fn run_remote(remote: &BuildRemoteCfg, data_dir: &Path, lease: &Lease, sub: &str, args: &[String], deadline: &Deadline) -> anyhow::Result<i32> {
    let pw = secret(data_dir, remote)?;
    let mut cmd = ssh_base(remote, pw.as_deref(), data_dir)?;
    let threads = pick_test_threads(std::env::var("RUST_TEST_THREADS").ok().as_deref(), remote.test_threads);
    cmd.arg(run_script(&lease.hs.dir, sub, &lease.token, remote.cargo_jobs, threads, args));
    let status = run_status(cmd, "remote cargo", deadline)?;
    Ok(status.code().unwrap_or(1))
}

/// Entry point used by the installed cargo shim. Returns 125 when offload should not happen, so
/// the shim can run the real local Cargo instead. Once a configured remote attempt starts, transport
/// failures are errors (not silent local fallback) because callers must know what was actually verified.
/// `run_cli` 的決定：轉遠端（帶設定），或退回本機（帶要對使用者講的原因；`None`＝本來就沒啟用，不必講）。
pub enum Offload {
    Go(BuildRemoteCfg),
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
    if remote.host.trim().is_empty() || remote.user.trim().is_empty() {
        return Offload::Local(Some("外部編譯啟用了但 host／user 是空的".into()));
    }
    if let Some(var) = env_blocking_offload(env) {
        return Offload::Local(Some(format!("本機設了 {var}，遠端看不到它，結果會跟本機不一致")));
    }
    Offload::Go(remote)
}

pub fn run_cli(config_path: &Path, data_dir: &Path, cwd: &Path, args: &[String]) -> i32 {
    let remote = match decide_offload(config_path, args, std::env::vars()) {
        Offload::Go(r) => r,
        Offload::Local(why) => {
            if let Some(why) = why {
                eprintln!("agents-manager: 這次 cargo 不轉外部編譯，改在本機跑：{why}");
            }
            return 125;
        }
    };
    eprintln!(
        "agents-manager: remote Cargo → {}@{}:{} ({})",
        remote.user, remote.host, remote.ssh_port, args.first().map(String::as_str).unwrap_or("")
    );
    install_signal_handlers();
    run_offload(&remote, data_dir, cwd, args, &Deadline::new(remote.timeout_secs).watching_signals())
}

/// 真的把這次呼叫丟到遠端（已經確定要轉、設定也讀好了）。整體時間受 `remote.timeout_secs` 限制（issue #194）；收到終止訊號也會好好收尾（issue #201）。
fn run_offload(remote: &BuildRemoteCfg, data_dir: &Path, cwd: &Path, args: &[String], ctl: &Deadline) -> i32 {
    let result = (|| -> anyhow::Result<i32> {
        // 從這裡起不管怎麼離開（`?`、panic、被殺、超過上限、收到訊號），守門都會收到 EOF 把目錄還回去、遠端還在跑的 cargo 整組收掉。
        let (root, sub) = source_root(cwd);
        let mut lease = open_lease(remote, data_dir, &root, ctl)?;
        // 整體上限從拿到名額與目錄才開始算（issue #104）。
        ctl.arm();
        lease.collect(&select_gc(&lease.hs, &GcPolicy::from_cfg(remote)));
        sync_source(remote, data_dir, &root, &lease.hs.dir, ctl)?;
        let code = run_remote(remote, data_dir, &lease, &sub, args, ctl)?;
        lease.finish();
        Ok(code)
    })();
    // `lease` 已經在上面那個閉包結束時放掉：守門的清理（收 process group、還目錄與鎖）做完才走到這裡。
    // 收到終止訊號：Ctrl-C 會同時打到本機的 rsync／ssh，它們的結束碼不代表這次的結果——一律照訊號回 128+sig。
    if let Some(Stopped::Interrupted(sig)) = ctl.stop() {
        eprintln!("agents-manager: 收到訊號 {sig}，已中止遠端編譯（遠端那一整組行程已收掉、目錄與鎖已還回）");
        return 128 + sig;
    }
    match result {
        Ok(code) => code,
        // 遠端什麼都還沒跑：原因守門已經印了，回 75 讓呼叫端知道是「稍後再試」，不是驗證失敗。
        Err(e) if e.downcast_ref::<QueueFull>().is_some() => EXIT_QUEUE_FULL,
        Err(e) if matches!(e.downcast_ref::<Stopped>(), Some(Stopped::TimedOut)) => {
            eprintln!(
                "agents-manager: 遠端編譯超過 {} 上限，已中止（遠端那一整組行程已收掉、目錄與鎖已還回；可調 [build.remote] timeout_secs，0＝不設上限）。這次不會退回本機重跑",
                limit_text(remote.timeout_secs)
            );
            EXIT_TIMEOUT
        }
        Err(e) => {
            eprintln!("agents-manager: remote Cargo failed: {e:#}");
            126
        }
    }
}

#[cfg(test)]
mod tests {
    /// #328：同步的排除規則不能濾掉名叫 target 的原始碼目錄，但根目錄的 target 仍不送。
    #[test]
    fn the_rsync_excludes_keep_source_dirs_named_target() {
        if !has_program(&rsync_program()) {
            return;
        }
        let base = std::env::temp_dir().join(format!("am-rsync-{}", std::process::id()));
        let (src, dst) = (base.join("src"), base.join("dst"));
        for d in ["src/target", "crates/target", "target/debug"] {
            std::fs::create_dir_all(src.join(d)).unwrap();
        }
        std::fs::write(src.join("src/target/mod.rs"), "x").unwrap();
        std::fs::write(src.join("crates/target/Cargo.toml"), "x").unwrap();
        std::fs::write(src.join("target/debug/big"), "x").unwrap();
        let st = Command::new(rsync_program())
            .args(["-a", "--delete"])
            .args(RSYNC_EXCLUDES)
            .arg(format!("{}/", src.display()))
            .arg(format!("{}/", dst.display()))
            .output()
            .unwrap();
        assert!(st.status.success(), "{}", String::from_utf8_lossy(&st.stderr));
        assert!(dst.join("src/target/mod.rs").exists(), "src/target 是原始碼，要送");
        assert!(dst.join("crates/target/Cargo.toml").exists(), "crates/target 是 workspace 成員，要送");
        assert!(!dst.join("target").exists(), "根目錄的 target 不送");
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

    /// #324／#325：設定檔壞掉、host 空、或本機設了 RUSTFLAGS 這類遠端看不到的變數——要退回本機也要講出原因，不能靜默；
    /// 正常啟用且沒有這些變數才轉遠端；檔案不存在／沒啟用是本來就不轉，不必吵。
    #[test]
    fn the_offload_decision_never_falls_back_silently_when_something_is_wrong() {
        let dir = std::env::temp_dir().join(format!("am-decide-{}", std::process::id()));
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
        std::fs::remove_dir_all(&dir).ok();
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
        let t = std::env::temp_dir().join(format!("am-r177-{}", crate::db::ulid()));
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
        let dir = std::env::temp_dir().join(format!("am-askpass-{}", crate::db::ulid()));
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

    /// 在 `sh` 裡真的跑遠端的腳本：`HOME` 是空沙盒、PATH 只有 `uname`，`cargo`／`rustup` 用 shell 函式假裝（不寫可執行檔，免得撞 ETXTBSY）。
    fn run_remote_sh(script: &str, fakes: &str) -> String {
        let sandbox = std::env::temp_dir().join(format!("am-probe-{}", crate::db::ulid()));
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

    /// 政策從設定來：閒置小時數至少算 1（0 不能變成「馬上收光」），預設 3 小時、最多 8 份，舊設定沒寫這兩個 key 也是預設。
    #[test]
    fn the_gc_policy_comes_from_the_remote_settings() {
        assert_eq!(GcPolicy::default(), GcPolicy { shared_idle_secs: 3 * 3600, max_shared: 8 });
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
        let cargo = s.find("cargo test").expect(&s);
        assert!(pgid < owner && owner < cargo, "{s}");
        assert!(s.contains("exit 126"), "{s}");
        assert!(s.contains("'x; rm -rf ~'"), "{s}");
        assert!(s.contains("PATH=\"$HOME/.cargo/bin:$PATH\" CARGO_BUILD_JOBS=4 RUST_TEST_THREADS=8 cargo"), "{s}");
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

    /// #148：租約身分固定格式（32 個小寫 hex），嵌進遠端 shell 與檔名都不會變成 injection；
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

    /// 守門交回來的 token 不是我們給的那個＝出事了，不能當成拿到目錄（不然後面 run 會拿錯身分去核 owner）。
    #[test]
    fn a_guard_that_answers_with_another_token_is_not_a_lease() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("printf 'ROOT\\t/r\\nDIR\\t/r/0123456789abcdef/shared\\tfeedfacefeedfacefeedfacefeedface\\nEND\\n'; cat >/dev/null");
        let err = Lease::start(cmd, LeaseToken::new(), &Deadline::new(0)).err().expect("token mismatch must fail").to_string();
        assert!(err.contains("lease token"), "{err}");
    }

    /// 守門腳本裡那條自動上限算式的 Rust 版（測試用的對照；遠端才知道自己的核數與 RAM，正式的只有腳本那份）。
    pub(super) fn auto_max_concurrent(ncpu: u64, mem_kb: u64, jobs: usize) -> usize {
        let by_cpu = ncpu * 3 / (2 * jobs.max(1) as u64);
        let by_mem = mem_kb.saturating_sub(AUTO_RESERVE_KB) / AUTO_PER_JOB_KB;
        by_cpu.min(by_mem).max(1) as usize
    }

    /// issue #104：`max_concurrent = 0`（預設）依遠端核數與 RAM 算——CPU 容許 1.5 倍超賣，記憶體每個編譯 8 GiB、留 8 GiB 給系統。
    /// 現在那台（32 核、MemTotal 65837352 kB）配 `cargo_jobs = 8` 是 6；小機器至少 1；設定寫了數字就用設定、最多 64。
    #[test]
    fn the_remote_cap_defaults_to_what_the_hosts_cores_and_ram_can_take() {
        assert_eq!(BuildRemoteCfg::default().max_concurrent, 0, "預設＝自動");
        let old: BuildRemoteCfg = toml::from_str("host = \"h\"\n").unwrap();
        assert_eq!(old.max_concurrent, 0, "舊設定沒有這個 key：自動");
        assert_eq!(auto_max_concurrent(32, 65_837_352, 8), 6, "現在那台：min(32×1.5÷8, (62.8−8)÷8) = min(6, 6)");
        assert_eq!(auto_max_concurrent(32, 65_837_352, 4), 6, "cargo_jobs 小一點時換記憶體卡住");
        assert_eq!(auto_max_concurrent(8, 16 * 1024 * 1024, 4), 1, "16 GiB 的機器：一次一個");
        assert_eq!(auto_max_concurrent(2, 4 * 1024 * 1024, 8), 1, "再小也至少 1");
        assert_eq!(auto_max_concurrent(64, 256 * 1024 * 1024, 4), 24);
        let set = BuildRemoteCfg { max_concurrent: 3, cargo_jobs: 5, ..Default::default() };
        let adm = Admission::from_cfg(&set);
        assert_eq!((adm.max, adm.jobs, adm.shared_wait_secs, adm.queue_wait_secs), (3, 5, SHARED_WAIT_SECS, QUEUE_WAIT_SECS));
        let script = guard_script("/r", "0123456789abcdef", "job-1-1", &LeaseToken::new(), adm);
        for want in ["max=3; jobs=5; swait=300; qwait=1800", &format!("(mem - {AUTO_RESERVE_KB}) / {AUTO_PER_JOB_KB}")] {
            assert!(script.contains(want), "{want}");
        }
        assert!(!script.contains('@'), "沒替換乾淨");
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
        let dir = std::env::temp_dir().join(format!("am-remote-cargo-{}", crate::db::ulid()));
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
        let store = crate::config::ConfigStore::load(dir.join("config.toml")).await.unwrap();
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
        let store = crate::config::ConfigStore::load(dir.join("config.toml")).await.unwrap();
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
                let store = crate::config::ConfigStore::load(cfg_path.clone()).await.unwrap();
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
                let fresh = crate::config::ConfigStore::load(cfg_path.clone()).await.unwrap().get().await.build.remote;
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
                let store = crate::config::ConfigStore::load(cfg_path.clone()).await.unwrap();
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
    use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
    use std::time::{Duration, Instant};

    const HASH: &str = "0123456789abcdef";

    fn base() -> PathBuf {
        let d = std::env::temp_dir().join(format!("am-r141-{}", crate::db::ulid()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 既有測試用的：名額夠多、shared 被佔著就馬上改用 job 目錄（等待另有測試）。
    const QUICK: Admission = Admission { max: 64, jobs: 1, shared_wait_secs: 0, queue_wait_secs: 30 };

    fn guard(base: &Path, hash: &str, job: &str) -> Lease {
        guard_with(base, hash, job, QUICK)
    }

    fn guard_cmd(base: &Path, hash: &str, job: &str, token: &LeaseToken, adm: Admission) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(guard_script("rc", hash, job, token, adm)).current_dir(base);
        cmd
    }

    fn guard_with(base: &Path, hash: &str, job: &str, adm: Admission) -> Lease {
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
        std::fs::write(&cargo, "#!/bin/sh\ntouch \"$PWD/../cargo-started-$$\"\nexec sleep 60\n").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
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

    /// #148：租約身分不能是遠端 PID。上一代結束、下一代租到同一個 shared 之後，姍姍來遲的舊 run
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
        assert_ne!(stale.as_str(), a.child.id().to_string(), "身分不能是守門 shell 的 PID");
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

    /// 寫一支假腳本並確定它已經可以被 exec：並行的另一條測試 fork 時會短暫繼承寫入 fd，這段時間 exec 會回 ETXTBSY（issue #189）——
    /// 探測用 exec 一次（腳本第一行在 `AM_TEST_EXEC_PROBE` 有設時 `exit 0`），還在被擋就重試，等的是條件不是時間。
    fn write_exec(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::write(path, format!("#!/bin/sh\n[ -z \"${{AM_TEST_EXEC_PROBE:-}}\" ] || exit 0\n{body}")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = Instant::now();
        loop {
            match Command::new(path).env("AM_TEST_EXEC_PROBE", "1").output() {
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && started.elapsed() < Duration::from_secs(30) => std::thread::sleep(Duration::from_millis(2)),
                r => {
                    assert!(r.unwrap().status.success());
                    return;
                }
            }
        }
    }

    /// 假遠端：`ssh` 把最後一個參數（要在遠端跑的指令字串）在**本機**用 `sh -c` 跑，跟 sshd 一樣每條連線自成一個 session
    /// （`setsid`；收 process group 才不會打到測試本身）；`rsync` 把來源目錄複製到 `user@host:` 後面那個路徑。
    /// `$HOME` 指到沙盒，裡面的假 cargo 就是「遠端的 cargo」。回傳 (設定, cwd, data_dir)。
    fn fake_remote(base: &Path, timeout_secs: u64, cargo_body: &str) -> (BuildRemoteCfg, PathBuf, PathBuf) {
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

    /// 在這條執行緒上把 `ssh`／`rsync` 換成 `fake_remote` 寫的假腳本再跑 `f`。
    fn with_fake_transport<T>(base: &Path, f: impl FnOnce() -> T) -> T {
        let bin = base.join("fakebin");
        TEST_PROGRAMS.with(|p| *p.borrow_mut() = Some((bin.join("ssh").display().to_string(), bin.join("rsync").display().to_string())));
        let out = f();
        TEST_PROGRAMS.with(|p| *p.borrow_mut() = None);
        out
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
        let adm = Admission { max: 2, jobs: 1, shared_wait_secs: 0, queue_wait_secs: 60 };
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
                let (remote, data, base) = (remote.clone(), data.clone(), base.clone());
                std::thread::spawn(move || {
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
    #[test]
    fn time_spent_queueing_does_not_count_toward_the_overall_limit() {
        let base = base();
        let (mut remote, cwd, data) = fake_remote(&base, 3, "sleep 1\nexit 0\n");
        remote.max_concurrent = 1;
        let mut holder = guard_with(&base, "00000000000000a1", "job-1-1", Admission { max: 1, ..QUICK });
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
        assert!(t0.elapsed() >= Duration::from_secs(2), "{:?}", t0.elapsed());
        c.finish();
        b.finish();
        let _ = std::fs::remove_dir_all(base);
    }

    /// 名額一直全滿：排到上限就放棄——不交目錄、exit 75（[`QueueFull`]），自己佔的 shared 鎖也放掉。
    #[test]
    fn a_queue_that_never_moves_gives_up_with_exit_75() {
        let base = base();
        let mut holder = guard_with(&base, "00000000000000a1", "job-1-1", Admission { max: 1, ..QUICK });
        let token = LeaseToken::new();
        let cmd = guard_cmd(&base, HASH, "job-2-2", &token, Admission { max: 1, queue_wait_secs: 2, ..QUICK });
        let t0 = Instant::now();
        let err = Lease::start(cmd, token, &Deadline::new(0)).err().expect("queue full");
        assert!(err.downcast_ref::<QueueFull>().is_some(), "{err:#}");
        assert!(t0.elapsed() >= Duration::from_secs(2) && t0.elapsed() < Duration::from_secs(60), "{:?}", t0.elapsed());
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
}
