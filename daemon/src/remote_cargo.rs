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
    app.cfg
        .update(|cfg| {
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
        _ => Command::new("ssh"),
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

/// 這次要請守門回收哪些目錄。有鎖被持有、有行程 cwd 在裡面、或就是自己這次的目錄，一律不動；
/// 其餘照種類看閒置多久。守門刪之前會自己再拿鎖、再看一次行程，這裡是第一道篩選。
pub fn select_gc(hs: &Handshake) -> Vec<String> {
    let low_disk = matches!(hs.disk, Some((free, total)) if total > 0 && free * 100 < total * LOW_DISK_FREE_PCT);
    hs.dirs
        .iter()
        .filter(|d| !d.locked && !d.busy && d.path != hs.dir)
        .filter(|d| match leaf_kind(&hs.root, &d.path) {
            Some(LeafKind::Shared) if !low_disk => d.idle_secs >= SHARED_IDLE_SECS,
            Some(_) => d.idle_secs >= ORPHAN_IDLE_SECS,
            None => false,
        })
        .map(|d| d.path.clone())
        .collect()
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
fn run_script(dir: &str, token: &LeaseToken, jobs: usize, args: &[String]) -> String {
    let argv = args.iter().map(|a| sh_quote(a)).collect::<Vec<_>>().join(" ");
    let tok = token.as_str();
    // `~/.cargo/bin` 要自己接：rustup 只改 shell profile，ssh 的非互動 shell 不讀（同 `PROBE_SH`）。
    format!(
        "d={dir}; g=$(ps -o pgid= -p $$ | tr -d ' ') && printf '%s\\n' \"$g\" > \"$d.pgid-{tok}\" \
         && [ \"$(cat \"$d.owner\" 2>/dev/null)\" = {tok} ] \
         || {{ echo 'agents-manager: 遠端工作目錄已經還回去了（helper 中途被砍？），不跑 cargo' >&2; exit 126; }}; \
         cd \"$d\" && PATH=\"$HOME/.cargo/bin:$PATH\" CARGO_BUILD_JOBS={jobs} cargo {argv}",
        dir = sh_quote(dir),
        jobs = jobs.max(1),
    )
}

fn run_status(mut cmd: Command, what: &str) -> anyhow::Result<ExitStatus> {
    cmd.stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
    cmd.status().map_err(|e| anyhow::anyhow!("{what}: {e}"))
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

fn sync_source(remote: &BuildRemoteCfg, data_dir: &Path, cwd: &Path, dir: &str) -> anyhow::Result<()> {
    if !has_program("rsync") {
        anyhow::bail!("remote Cargo requires `rsync` on the daemon host");
    }
    let pw = secret(data_dir)?;
    let mode = match pw {
        Some(_) => Some(password_mode(data_dir)?),
        None => None,
    };
    let mut ssh = format!(
        "ssh -p {} -o StrictHostKeyChecking=accept-new -o {}",
        remote.ssh_port,
        if pw.is_some() { "BatchMode=no" } else { "BatchMode=yes" }
    );
    if matches!(mode, Some(PwMode::Askpass(_))) {
        ssh.push_str(" -o NumberOfPasswordPrompts=1");
    }
    let mut cmd = match &mode {
        Some(PwMode::Sshpass) => {
            let mut c = Command::new("sshpass");
            c.arg("-e").arg("rsync");
            c
        }
        _ => Command::new("rsync"),
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
    let status = run_status(cmd, "rsync source")?;
    if !status.success() {
        anyhow::bail!("rsync failed with exit {:?}", status.code());
    }
    Ok(())
}

fn run_remote(remote: &BuildRemoteCfg, data_dir: &Path, lease: &Lease, args: &[String]) -> anyhow::Result<i32> {
    let pw = secret(data_dir)?;
    let mut cmd = ssh_base(remote, pw.as_deref(), data_dir)?;
    cmd.arg(run_script(&lease.hs.dir, &lease.token, remote.cargo_jobs, args));
    let status = run_status(cmd, "remote cargo")?;
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
    let result = (|| -> anyhow::Result<i32> {
        // 從這裡起不管怎麼離開（`?`、panic、被殺），守門都會收到 EOF 把目錄還回去。
        let mut lease = open_lease(&remote, data_dir, cwd)?;
        lease.collect(&select_gc(&lease.hs));
        sync_source(&remote, data_dir, cwd, &lease.hs.dir)?;
        let code = run_remote(&remote, data_dir, &lease, args)?;
        lease.finish();
        Ok(code)
    })();
    match result {
        Ok(code) => code,
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
            select_gc(&h),
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
        let roomy = select_gc(&hs(dirs.clone(), Some((50, 100))));
        assert_eq!(roomy, vec![format!("{ROOT}/00000000000000bb/shared")]);
        let full = select_gc(&hs(dirs, Some((10, 100))));
        assert_eq!(
            full,
            vec![format!("{ROOT}/00000000000000aa/shared"), format!("{ROOT}/00000000000000bb/shared")],
            "磁碟緊時，被持有的 shared 還是不能收"
        );
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
        assert!(select_gc(&hs(dirs, Some((1, 100)))).is_empty());
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
        let s = run_script("/r/0123456789abcdef/shared", &tok, 4, &["test".into(), "-p".into(), "x; rm -rf ~".into()]);
        let pgid = s.find("> \"$d.pgid-0123456789abcdef0123456789abcdef\"").expect(&s);
        let owner = s.find("\"$d.owner\"").expect(&s);
        let cargo = s.find("cargo test").expect(&s);
        assert!(pgid < owner && owner < cargo, "{s}");
        assert!(s.contains("exit 126"), "{s}");
        assert!(s.contains("'x; rm -rf ~'"), "{s}");
        assert!(s.contains("PATH=\"$HOME/.cargo/bin:$PATH\" CARGO_BUILD_JOBS=4"), "{s}");
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
            .arg(run_script(dir, token, 1, &["test".into()]))
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
        assert!(select_gc(&g.hs).is_empty(), "剛建的目錄還沒到閒置門檻：{:?}", g.hs.dirs);

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
}
