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

fn ssh_base(remote: &BuildRemoteCfg, password: Option<&str>) -> anyhow::Result<Command> {
    if password.is_some() && !has_program("sshpass") {
        anyhow::bail!("password auth requires `sshpass` on the daemon host; install it or clear the password and use an SSH key/agent");
    }
    let mut cmd = if password.is_some() {
        let mut c = Command::new("sshpass");
        c.arg("-e").arg("ssh");
        c
    } else {
        Command::new("ssh")
    };
    if let Some(pw) = password {
        cmd.env("SSHPASS", pw);
    }
    cmd.arg("-p")
        .arg(remote.ssh_port.to_string())
        .arg("-o")
        .arg("StrictHostKeyChecking=accept-new")
        .arg("-o")
        .arg(if password.is_some() { "BatchMode=no" } else { "BatchMode=yes" })
        .arg(format!("{}@{}", remote.user, remote.host));
    Ok(cmd)
}

fn probe(remote: &BuildRemoteCfg, data_dir: &Path) -> anyhow::Result<Value> {
    let pw = secret(data_dir)?;
    let mut cmd = ssh_base(remote, pw.as_deref())?;
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
    matches!(args.first().map(String::as_str), Some("check" | "test" | "clippy"))
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
    format!("{root}/{:016x}", fnv1a64(&cwd.to_string_lossy()))
}

fn run_status(mut cmd: Command, what: &str) -> anyhow::Result<ExitStatus> {
    cmd.stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
    cmd.status().map_err(|e| anyhow::anyhow!("{what}: {e}"))
}

fn ensure_remote_dir(remote: &BuildRemoteCfg, data_dir: &Path, dir: &str) -> anyhow::Result<()> {
    let pw = secret(data_dir)?;
    let mut cmd = ssh_base(remote, pw.as_deref())?;
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
    if pw.is_some() && !has_program("sshpass") {
        anyhow::bail!("password auth requires `sshpass` on the daemon host");
    }
    let ssh = format!(
        "ssh -p {} -o StrictHostKeyChecking=accept-new -o {}",
        remote.ssh_port,
        if pw.is_some() { "BatchMode=no" } else { "BatchMode=yes" }
    );
    let mut cmd = if pw.is_some() {
        let mut c = Command::new("sshpass");
        c.arg("-e").arg("rsync");
        c
    } else {
        Command::new("rsync")
    };
    if let Some(pw) = pw.as_deref() {
        cmd.env("SSHPASS", pw);
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
    let mut cmd = ssh_base(remote, pw.as_deref())?;
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

    #[test]
    fn only_cross_platform_verification_commands_are_offloaded() {
        for cmd in ["check", "test", "clippy"] {
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
