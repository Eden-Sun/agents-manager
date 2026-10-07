//! Running `git` (and any `sh` script) on a project's host: a local process, or `ssh_exec_path`
//! with the exit status carried back through stdout.

use crate::config::LOCAL_HOST;
use crate::hosts::sh_quote;
use crate::hosts::HostsAccess;
use anyhow::{anyhow, Result};
use std::time::Duration;

pub const GIT_TIMEOUT: Duration = Duration::from_secs(60);
pub const PUSH_TIMEOUT: Duration = Duration::from_secs(180);

/// Marker `sh` appends so a remote shell can report the exit status through stdout.
const RC: &str = "__am_rc=";

pub fn wrap_remote_script(full: &str) -> String {
    format!("( {full}\n) 2>&1; printf '\\n{RC}%s\\n' \"$?\"")
}

pub fn parse_remote_output(raw: String) -> (String, i32) {
    match raw.rsplit_once(RC) {
        Some((body, code)) => (body.to_string(), code.trim().parse::<i32>().unwrap_or(-1)),
        None => (raw, -1),
    }
}

#[derive(Debug, Clone)]
pub struct Out {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Out {
    pub fn ok(&self) -> bool {
        self.code == 0
    }
    pub fn trimmed(&self) -> String {
        self.stdout.trim().to_string()
    }
    /// stderr when there is any, else stdout — what a human wants to read about a failure.
    pub fn message(&self) -> String {
        let e = self.stderr.trim();
        if e.is_empty() {
            self.stdout.trim().to_string()
        } else {
            e.to_string()
        }
    }
}

/// PATH prefix so git / gh from Homebrew are found even under launchd, colour forcing off
/// (the same fix `github.rs` needs for `gh --json`).
const PATH_FIX: &str = "export PATH=\"/opt/homebrew/bin:/usr/local/bin:$HOME/.local/bin:$PATH\"\n\
export NO_COLOR=1\nunset CLICOLOR_FORCE FORCE_COLOR CLICOLOR 2>/dev/null\n";

/// Run a POSIX `sh` script on `host` and return its exit code as data rather than an error:
/// a failed git command is a normal outcome for the caller to report, not a transport error.
pub async fn sh(app: &impl HostsAccess, host: &str, script: &str, timeout: Duration) -> Result<Out> {
    let full = format!("{PATH_FIX}{script}");
    if host == LOCAL_HOST {
        // `sh_local` kills the whole process group on timeout, so a hung git does not keep
        // holding `index.lock` / `MERGE_HEAD` after we have reported it as failed.
        let o = crate::hosts::sh_local(&full, timeout)
            .await?
            .ok_or_else(|| anyhow!("git command timed out after {}s", timeout.as_secs()))?;
        return Ok(Out {
            code: o.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&o.stdout).to_string(),
            stderr: String::from_utf8_lossy(&o.stderr).to_string(),
        });
    }
    // Remote: ssh_exec_path already fails the whole call on a non-zero status, so the status
    // is carried back in stdout instead.
    let conn = app.hosts().get(host).await.ok_or_else(|| anyhow!("unknown host `{host}`"))?;
    let wrapped = wrap_remote_script(&full);
    let raw = conn.ssh_exec_path_timeout(&wrapped, timeout).await?;
    let (body, code) = parse_remote_output(raw);
    Ok(Out { code, stdout: body, stderr: String::new() })
}

/// One `git -C <dir> …` invocation. `args` are shell-quoted here, never by the caller.
pub async fn git(app: &impl HostsAccess, host: &str, dir: &str, args: &[&str], timeout: Duration) -> Result<Out> {
    let mut script = format!("git -C {}", sh_quote(dir));
    for a in args {
        script.push(' ');
        script.push_str(&sh_quote(a));
    }
    sh(app, host, &script, timeout).await
}
