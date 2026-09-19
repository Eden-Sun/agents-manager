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

/// 超過整體上限時 helper 的結束碼（跟 GNU `timeout` 一樣是 124）。shim 只把 125 當成「退回本機」，所以不會在本機重跑一次。
pub const EXIT_TIMEOUT: i32 = 124;

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
    /// None = preserve the currently stored password; Some("") = delete it (key/agent auth).
    #[serde(default)]
    pub password: Option<String>,
}

fn default_port() -> u16 {
    22
}

fn password_path(data_dir: &Path) -> PathBuf {
    data_dir.join(PASSWORD_FILE)
}

pub fn password_is_set(data_dir: &Path) -> bool {
    std::fs::metadata(password_path(data_dir)).map(|m| m.is_file() && m.len() > 0).unwrap_or(false)
}

fn write_password(data_dir: &Path, password: &str) -> anyhow::Result<()> {
    let path = password_path(data_dir);
    if password.is_empty() {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        return Ok(());
    }
    std::fs::write(&path, password)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
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
        "password_set": password_is_set(data_dir),
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
    if input.timeout_secs.is_some_and(|t| t > MAX_TIMEOUT_SECS) {
        return Err(LcError::Bad(format!("timeout_secs 最多 {MAX_TIMEOUT_SECS} 秒（0＝不設上限）")));
    }
    app.cfg
        .update(|cfg| {
            let timeout_secs = input.timeout_secs.unwrap_or(cfg.build.remote.timeout_secs);
            let shared_idle_hours = input.shared_idle_hours.unwrap_or(cfg.build.remote.shared_idle_hours);
            let max_shared_dirs = input.max_shared_dirs.unwrap_or(cfg.build.remote.max_shared_dirs);
            let test_threads = input.test_threads.unwrap_or(cfg.build.remote.test_threads).min(256);
            cfg.build.remote = BuildRemoteCfg {
                enabled: input.enabled,
                host: host.clone(),
                user: user.clone(),
                ssh_port: input.ssh_port,
                remote_root: if remote_root.is_empty() {
                    crate::config::default_remote_build_root()
                } else {
                    remote_root.clone()
                },
                cargo_jobs,
                test_threads,
                timeout_secs,
                shared_idle_hours,
                max_shared_dirs,
            };
            Ok(())
        })
        .await
        .map_err(|e| LcError::Upstream(e.to_string()))?;

    // Secret is intentionally outside config.toml. If this write fails, report failure instead of
    // pretending the setting is usable; the non-secret desired state is still visible for repair.
    if let Some(password) = input.password.as_deref() {
        write_password(&app.data_dir, password).map_err(|e| LcError::Upstream(format!("store remote Cargo password: {e}")))?;
    }
    let cfg = app.cfg.get().await;
    Ok(Json(sanitized(&cfg.build.remote, &app.data_dir)))
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

fn secret(data_dir: &Path) -> anyhow::Result<Option<String>> {
    match std::fs::read_to_string(password_path(data_dir)) {
        Ok(s) => {
            let s = s.trim_end_matches(|c| c == '\r' || c == '\n').to_string();
            Ok((!s.is_empty()).then_some(s))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// 連線存活選項（#174），ssh 與 rsync 的 `-e` 共用：對端連上之後靜默消失（Wi-Fi 換 AP、Mac 睡著醒來 IP 變了、遠端掉電）時，
/// 沒有這些 ssh 只能等 TCP keepalive（預設 2 小時），helper 與 agent 的 cargo 一路卡著，遠端的鎖與目錄也一直被佔。
/// `ConnectTimeout` 連握手一起算（連上就不說話的對端）；`ServerAlive*` 管連上之後——15 秒問一次、連 3 次沒回（約 45 秒）就斷。
/// 跟 `hosts.rs::ssh_args` 的節奏一致。
const SSH_LIVENESS_OPTS: [&str; 6] = ["-o", "ConnectTimeout=15", "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=3"];

fn ssh_base(remote: &BuildRemoteCfg, password: Option<&str>, data_dir: &Path) -> anyhow::Result<Command> {
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
        .arg(if password.is_some() { "BatchMode=no" } else { "BatchMode=yes" })
        .arg(format!("{}@{}", remote.user, remote.host));
    cmd
}

/// probe 在遠端跑的那一行。`~/.cargo/bin` 一併看：rustup 裝好之後**只**改 shell profile，
/// 而 ssh 的非互動 shell 不讀 profile，`command -v cargo` 會說沒有（2026-09-18）。
const PROBE_SH: &str = "printf 'OS='; uname -s; printf 'ARCH='; uname -m; \
PATH=\"$HOME/.cargo/bin:$PATH\"; printf 'CARGO='; command -v cargo || true; cargo --version 2>/dev/null || true";

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

fn probe(remote: &BuildRemoteCfg, data_dir: &Path) -> anyhow::Result<Value> {
    let pw = secret(data_dir)?;
    let mut cmd = ssh_base(remote, pw.as_deref(), data_dir)?;
    cmd.arg(PROBE_SH);
    let out = cmd.output()?;
    if !out.status.success() {
        anyhow::bail!("ssh probe failed (exit {:?}): {}", out.status.code(), String::from_utf8_lossy(&out.stderr).trim());
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let (os, arch, cargo, version) = parse_probe(&stdout);
    Ok(json!({
        "ok": true,
        "output": stdout.trim(),
        "password_auth": pw.is_some(),
        "os": os,
        "arch": arch,
        "cargo_path": cargo,
        "cargo_version": version,
        "cargo_missing": cargo.is_none(),
    }))
}

/// 在遠端裝 Rust 工具鏈。已經有了就什麼都不做（回報現有版本）。
///
/// `--profile minimal --no-modify-path`：我們只需要 cargo／rustc 跑 check/test/clippy，而且不要
/// 去動使用者的 shell profile——probe 與 `run_remote` 都自己把 `~/.cargo/bin` 接到 PATH 前面。
const INSTALL_SH: &str = "set -e\n\
PATH=\"$HOME/.cargo/bin:$PATH\"\n\
if command -v cargo >/dev/null 2>&1; then printf 'ALREADY='; cargo --version; exit 0; fi\n\
if ! command -v curl >/dev/null 2>&1; then echo 'remote host has no curl; install curl (or rustup) there first' >&2; exit 2; fi\n\
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --no-modify-path\n\
printf 'INSTALLED='; \"$HOME/.cargo/bin/cargo\" --version\n\
command -v cc >/dev/null 2>&1 || command -v gcc >/dev/null 2>&1 || echo 'CC_MISSING=1'\n";

fn install_toolchain(remote: &BuildRemoteCfg, data_dir: &Path) -> anyhow::Result<Value> {
    let pw = secret(data_dir)?;
    let mut cmd = ssh_base(remote, pw.as_deref(), data_dir)?;
    cmd.arg(INSTALL_SH);
    let out = cmd.output()?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !out.status.success() {
        anyhow::bail!("rustup install failed (exit {:?}): {}", out.status.code(), if stderr.is_empty() { stdout } else { stderr });
    }
    let already = stdout.contains("ALREADY=");
    // rustup 只裝 rust，不裝 linker。沒有 `cc` 的話 cargo test 會在連結那一步才爆，訊息很難懂
    // （2026-09-18 實測這台就是這樣），所以當場講出來。
    let cc_missing = stdout.lines().any(|l| l.trim() == "CC_MISSING=1");
    let version = stdout
        .lines()
        .find_map(|l| l.strip_prefix("ALREADY=").or_else(|| l.strip_prefix("INSTALLED=")))
        .map(|v| v.trim().to_string());
    Ok(json!({
        "ok": true,
        "already_installed": already,
        "cargo_version": version,
        "cc_missing": cc_missing,
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
pub fn eligible(args: &[String]) -> bool {
    matches!(args.first().map(String::as_str), Some("c" | "check" | "t" | "test" | "clippy"))
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
//   - `job-<pid>-<ms>/`＋`.lock`：`shared` 正被另一次呼叫用著時的退路，冷編譯，結束就刪。
//   - 純數字的 `<pid>/`：#141 之前的版本留下的，沒有鎖，只能靠「沒行程在用＋放很久」回收。
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
/// stdout 協定：`ROOT\t<絕對路徑>`、`DIR\t<這次的工作目錄>\t<token，原樣回>`、每個候選目錄一行
/// `D\t<路徑>\t<鎖被持有>\t<有行程 cwd 在裡面>\t<閒置秒數>`、`F\t<剩餘 KB>\t<總 KB>`，最後 `END`。
/// 之後 stdin 每行 `rm <路徑>` 是要它回收的孤兒（它自己拿鎖、重看一次有沒有人在用才刪），EOF＝這次結束。
fn guard_script(root: &str, hash: &str, job: &str, token: &LeaseToken) -> String {
    GUARD_SH
        .replace("@ROOT@", &sh_quote(root))
        .replace("@HASH@", &sh_quote(hash))
        .replace("@JOB@", &sh_quote(job))
        .replace("@TOKEN@", &sh_quote(token.as_str()))
        .replace("@HEX16@", &"[0-9a-f]".repeat(16))
}

const GUARD_SH: &str = r#"set -u
trap '' HUP PIPE
root=@ROOT@; hash=@HASH@; job=@JOB@; token=@TOKEN@
if ! command -v flock >/dev/null 2>&1 || [ ! -r /proc/self/stat ]; then
  echo 'agents-manager: remote Cargo 的遠端要有 flock（util-linux）與 /proc（Linux）' >&2; exit 2
fi
mkdir -p "$root" && cd "$root" && root=$(pwd -P) || exit 2
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
hold() {
  flock -n 8 || return 75
  same "$1.lock" 8 || return 76
  mkdir -p "$1" && touch "$1.lock" && printf '%s\n' "$token" > "$1.owner" || return 2
  printf 'ROOT\t%s\nDIR\t%s\t%s\n' "$root" "$1" "$token"
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
n=0
while :; do
  mkdir -p "$root/$hash" || exit 2
  hold "$root/$hash/shared" 8>>"$root/$hash/shared.lock"; rc=$?
  case $rc in
    0) exit 0 ;;
    75) break ;;
  esac
  n=$((n + 1)); [ $n -lt 5 ] || exit $rc
done
echo 'agents-manager: 這棵 worktree 的共用遠端 target 正被另一次呼叫使用，這次改用獨立目錄（冷編譯，結束就刪）' >&2
hold "$root/$hash/$job" 8>>"$root/$hash/$job.lock"
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

impl Lease {
    /// `cmd` 跑的守門腳本要是用 `token` 產生的；它交回來的 token 對不上就當守門出事、不當成拿到目錄。
    fn start(mut cmd: Command, token: LeaseToken) -> anyhow::Result<Self> {
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());
        let mut child = cmd.spawn().map_err(|e| anyhow::anyhow!("remote workdir guard: {e}"))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().map(BufReader::new);
        let mut lease = Lease { child, stdin, stdout, hs: Handshake::default(), token };
        let reader = lease.stdout.as_mut().ok_or_else(|| anyhow::anyhow!("remote workdir guard: no stdout"))?;
        lease.hs = read_handshake(reader)?;
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
    at: Option<std::time::Instant>,
    /// 測試用：模擬「收到終止訊號」。
    abort: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// 只有正式的 helper 看真的訊號；測試在同一個行程裡並行跑，不能共用那個全域。
    watch_signals: bool,
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
    /// `limit_secs == 0`＝不設上限。
    fn new(limit_secs: u64) -> Self {
        Deadline {
            at: (limit_secs > 0).then(|| std::time::Instant::now() + std::time::Duration::from_secs(limit_secs)),
            abort: Default::default(),
            watch_signals: false,
        }
    }

    /// 正式 helper：也看真的終止訊號。
    fn watching_signals(mut self) -> Self {
        self.watch_signals = true;
        self
    }

    fn stop(&self) -> Option<Stopped> {
        if self.abort.load(std::sync::atomic::Ordering::SeqCst) {
            return Some(Stopped::Interrupted(libc::SIGTERM));
        }
        if self.watch_signals {
            let sig = SIGNALLED.load(std::sync::atomic::Ordering::SeqCst);
            if sig != 0 {
                return Some(Stopped::Interrupted(sig));
            }
        }
        self.at.filter(|at| std::time::Instant::now() >= *at).map(|_| Stopped::TimedOut)
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

fn open_lease(remote: &BuildRemoteCfg, data_dir: &Path, cwd: &Path) -> anyhow::Result<Lease> {
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
    let pw = secret(data_dir)?;
    let token = LeaseToken::new();
    let mut cmd = ssh_base(remote, pw.as_deref(), data_dir)?;
    cmd.arg(format!("sh -c {}", sh_quote(&guard_script(&root, &hash, &job, &token))));
    Lease::start(cmd, token)
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
    ssh
}

fn sync_source(remote: &BuildRemoteCfg, data_dir: &Path, cwd: &Path, dir: &str, deadline: &Deadline) -> anyhow::Result<()> {
    if !has_program(&rsync_program()) {
        anyhow::bail!("remote Cargo requires `rsync` on the daemon host");
    }
    let pw = secret(data_dir)?;
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
    cmd.args(["-az", "--delete", "--exclude", ".git", "--exclude", "target", "-e", &ssh])
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
    let pw = secret(data_dir)?;
    let mut cmd = ssh_base(remote, pw.as_deref(), data_dir)?;
    let threads = pick_test_threads(std::env::var("RUST_TEST_THREADS").ok().as_deref(), remote.test_threads);
    cmd.arg(run_script(&lease.hs.dir, sub, &lease.token, remote.cargo_jobs, threads, args));
    let status = run_status(cmd, "remote cargo", deadline)?;
    Ok(status.code().unwrap_or(1))
}

/// Entry point used by the installed cargo shim. Returns 125 when offload should not happen, so
/// the shim can run the real local Cargo instead. Once a configured remote attempt starts, transport
/// failures are errors (not silent local fallback) because callers must know what was actually verified.
pub fn run_cli(config_path: &Path, data_dir: &Path, cwd: &Path, args: &[String]) -> i32 {
    if !eligible(args) {
        return 125;
    }
    let cfg: ConfigFile = match std::fs::read_to_string(config_path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
    {
        Some(c) => c,
        None => return 125,
    };
    let remote = cfg.build.remote;
    if !remote.enabled || remote.host.trim().is_empty() || remote.user.trim().is_empty() {
        return 125;
    }
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
        let mut lease = open_lease(remote, data_dir, &root)?;
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
        assert!(Deadline::new(0).stop().is_none(), "0＝不設上限");
        assert!(Deadline::new(1).at.is_some());
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
        // 安裝是冪等的：已經有就只回版本，不會再跑一次 rustup。
        assert!(INSTALL_SH.contains("if command -v cargo >/dev/null 2>&1; then printf 'ALREADY='"), "{INSTALL_SH}");
        // minimal，而且不去動使用者的 shell profile。
        assert!(INSTALL_SH.contains("--profile minimal"), "{INSTALL_SH}");
        assert!(INSTALL_SH.contains("--no-modify-path"), "{INSTALL_SH}");
        // rustup 不裝 linker：沒有 cc 的機器要當場講，不要等到 cargo test 連結失敗才看到天書。
        assert!(INSTALL_SH.contains("CC_MISSING=1"), "{INSTALL_SH}");
    }

    #[test]
    fn only_cross_platform_verification_commands_are_offloaded() {
        for cmd in ["c", "check", "t", "test", "clippy"] {
            assert!(eligible(&[cmd.into()]), "{cmd}");
        }
        for cmd in ["build", "run", "bench", "doc", "install", "metadata"] {
            assert!(!eligible(&[cmd.into()]), "{cmd}");
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
        let out = "Welcome!\nROOT\t/r\nDIR\t/r/0123456789abcdef/shared\tfeedfacefeedfacefeedfacefeedface\n\
                   D\t/r/00000000000000aa/111\t0\t1\t-3\nD\t/r/00000000000000aa/222\t1\t0\t900\n\
                   F\t500\t1000\nEND\nD\t/late\t0\t0\t9\n";
        let h = read_handshake(&mut std::io::Cursor::new(out)).unwrap();
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
        let script = guard_script("/r", "0123456789abcdef", "job-1-1", &a);
        assert!(script.contains(&format!("token={}", a.as_str())), "token 要原樣、不帶引號地進腳本");
        assert!(!script.contains("@TOKEN@"), "沒替換乾淨");
    }

    /// 守門交回來的 token 不是我們給的那個＝出事了，不能當成拿到目錄（不然後面 run 會拿錯身分去核 owner）。
    #[test]
    fn a_guard_that_answers_with_another_token_is_not_a_lease() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("printf 'ROOT\\t/r\\nDIR\\t/r/0123456789abcdef/shared\\tfeedfacefeedfacefeedfacefeedface\\nEND\\n'; cat >/dev/null");
        let err = Lease::start(cmd, LeaseToken::new()).err().expect("token mismatch must fail").to_string();
        assert!(err.contains("lease token"), "{err}");
    }

    #[test]
    fn password_file_is_private_and_metadata_does_not_echo_the_secret() {
        let dir = std::env::temp_dir().join(format!("am-remote-cargo-{}", crate::db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        write_password(&dir, "super-secret").unwrap();
        assert!(password_is_set(&dir));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(password_path(&dir)).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let v = sanitized(&BuildRemoteCfg::default(), &dir);
        assert_eq!(v["password_set"], true);
        assert!(!v.to_string().contains("super-secret"));
        let _ = std::fs::remove_dir_all(dir);
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

    fn guard(base: &Path, hash: &str, job: &str) -> Lease {
        let token = LeaseToken::new();
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(guard_script("rc", hash, job, &token)).current_dir(base);
        Lease::start(cmd, token).expect("guard should hand out a directory")
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
}
