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
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
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

fn probe(remote: &BuildRemoteCfg, data_dir: &Path) -> anyhow::Result<Value> {
    let pw = secret(data_dir)?;
    let mut cmd = ssh_base(remote, pw.as_deref(), data_dir)?;
    cmd.arg("printf 'OS='; uname -s; printf 'ARCH='; uname -m; printf 'CARGO='; command -v cargo || true; cargo --version 2>/dev/null || true");
    let out = cmd.output()?;
    if !out.status.success() {
        anyhow::bail!("ssh probe failed (exit {:?}): {}", out.status.code(), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(json!({
        "ok": true,
        "output": String::from_utf8_lossy(&out.stdout).trim(),
        "password_auth": pw.is_some(),
    }))
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

fn remote_dir(cfg: &BuildRemoteCfg, cwd: &Path) -> String {
    let root = cfg.remote_root.trim_end_matches('/');
    // Job-specific leaf prevents two agents verifying the same worktree from racing the same rsync/target tree.
    // Cache reuse is intentionally delegated to sccache (#91) rather than sharing a mutable target directory.
    format!("{root}/{:016x}/{}", fnv1a64(&cwd.to_string_lossy()), std::process::id())
}

fn run_status(mut cmd: Command, what: &str) -> anyhow::Result<ExitStatus> {
    cmd.stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
    cmd.status().map_err(|e| anyhow::anyhow!("{what}: {e}"))
}

fn ensure_remote_dir(remote: &BuildRemoteCfg, data_dir: &Path, dir: &str) -> anyhow::Result<()> {
    let pw = secret(data_dir)?;
    let mut cmd = ssh_base(remote, pw.as_deref(), data_dir)?;
    cmd.arg(format!("mkdir -p {}", sh_quote(dir)));
    let status = run_status(cmd, "ssh mkdir")?;
    if !status.success() {
        anyhow::bail!("remote mkdir failed with exit {:?}", status.code());
    }
    Ok(())
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

fn run_remote(remote: &BuildRemoteCfg, data_dir: &Path, dir: &str, args: &[String]) -> anyhow::Result<i32> {
    let pw = secret(data_dir)?;
    let mut cmd = ssh_base(remote, pw.as_deref(), data_dir)?;
    let argv = args.iter().map(|a| sh_quote(a)).collect::<Vec<_>>().join(" ");
    let remote_cmd = format!(
        "cd {} && CARGO_BUILD_JOBS={} cargo {}",
        sh_quote(dir),
        remote.cargo_jobs.max(1),
        argv
    );
    cmd.arg(remote_cmd);
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
    let dir = remote_dir(&remote, cwd);
    let result = (|| -> anyhow::Result<i32> {
        ensure_remote_dir(&remote, data_dir, &dir)?;
        sync_source(&remote, data_dir, cwd, &dir)?;
        run_remote(&remote, data_dir, &dir, args)
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
