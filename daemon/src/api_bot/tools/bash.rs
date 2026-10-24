use std::{path::Path, process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy)]
pub struct Limits {
    pub timeout: Duration,
    pub output_bytes: usize,
}
impl Limits {
    pub fn validate(self) -> Result<(), String> {
        if self.timeout.is_zero()
            || self.timeout > Duration::from_secs(300)
            || !(1..=1024 * 1024).contains(&self.output_bytes)
        {
            return Err("invalid command limits".into());
        }
        Ok(())
    }
}

pub async fn execute(
    root: &Path,
    command: &str,
    limits: Limits,
    cancel: &CancellationToken,
) -> Result<String, String> {
    limits.validate()?;
    // Use exactly the same #1884 static policy as the existing CLI PreToolUse hook.
    if let am_base::kill_guard::Verdict::Deny(reason) = am_base::kill_guard::check_command(command)
    {
        return Err(reason);
    }
    if command.len() > 64 * 1024 {
        return Err("command is too large".into());
    }
    if cancel.is_cancelled() {
        return Err("cancelled".into());
    }
    #[cfg(target_os = "linux")]
    {
        let bwrap = Path::new("/usr/bin/bwrap");
        // No fallback to a host shell. A namespace/mount failure also fails closed: bubblewrap
        // won't reach the shell unless every isolation operation has succeeded.
        if !bwrap.is_file() {
            return Err("bash isolation unavailable".into());
        }
        let mut cmd = Command::new(bwrap);
        cmd.args([
            "--unshare-all",
            "--die-with-parent",
            "--new-session",
            "--clearenv",
        ]);
        for path in ["/usr", "/bin", "/lib", "/lib64"] {
            if Path::new(path).exists() {
                cmd.args(["--ro-bind", path, path]);
            }
        }
        cmd.args([
            "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--bind",
        ])
        .arg(root)
        .arg("/workspace")
        .args([
            "--chdir",
            "/workspace",
            "--setenv",
            "PATH",
            "/usr/bin:/bin",
            "--setenv",
            "HOME",
            "/tmp",
            "--setenv",
            "LANG",
            "C.UTF-8",
            "--",
            "/bin/bash",
            "--noprofile",
            "--norc",
            "-c",
            command,
        ]);
        supervise(cmd, root, limits, cancel).await
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = root;
        Err("bash isolation unsupported on this platform".into())
    }
}

/// A PID owned by us, never looked up by process-name matching. Linux's PID namespace makes
/// its init's death terminate all descendants (including ones that called setsid).
struct OwnedGroup(u32);
impl Drop for OwnedGroup {
    fn drop(&mut self) {
        // The leader has not been waited/reaped yet when the guard is dropped, so its PID/PGID
        // cannot be reused. Only this command's freshly created process group is signalled.
        unsafe {
            libc::kill(-(self.0 as i32), libc::SIGKILL);
        }
    }
}

async fn read_capped(
    mut stream: impl tokio::io::AsyncRead + Unpin,
    cap: usize,
) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let count = stream
            .read(&mut chunk)
            .await
            .map_err(|_| "output read failed")?;
        if count == 0 {
            return Ok(bytes);
        }
        if bytes.len().saturating_add(count) > cap {
            return Err("command output limit exceeded".into());
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
}
async fn supervise(
    mut cmd: Command,
    root: &Path,
    limits: Limits,
    cancel: &CancellationToken,
) -> Result<String, String> {
    // Nothing from daemon's environment, including AM_*, API keys, BASH_ENV, or key_file paths.
    cmd.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C.UTF-8")
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .map_err(|_| "isolated command could not start")?;
    let pid = child.id().ok_or("missing command pid")?;
    let group = OwnedGroup(pid);
    let stdout = child.stdout.take().ok_or("missing stdout")?;
    let stderr = child.stderr.take().ok_or("missing stderr")?;
    // The two pipes share a combined budget: split it in half; neither side can allocate unboundedly.
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    let waiter = tokio::task::spawn_blocking(move || {
        // Observe exit WITHOUT reaping: keep PID ownership until group cleanup is complete.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = loop {
            let rc =
                unsafe { libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOWAIT) };
            if rc == 0 {
                break Ok(());
            }
            if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                break Err("command exit observation failed".to_string());
            }
        };
        let _ = exit_tx.send(result);
    });
    let collect = async {
        let (out, err) = tokio::try_join!(
            read_capped(stdout, limits.output_bytes / 2),
            read_capped(stderr, limits.output_bytes - limits.output_bytes / 2)
        )?;
        exit_rx.await.map_err(|_| "command observer lost")??;
        Ok::<_, String>((out, err))
    };
    let output = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err("cancelled".into()),
        result = tokio::time::timeout(limits.timeout, collect) => result.unwrap_or_else(|_| Err("command timed out".into())),
    };
    // Kill/reap on every path, also after normal output EOF (background tasks may have closed pipes).
    drop(group);
    let _ = child.start_kill();
    let _ = waiter.await;
    let status = child.wait().await.map_err(|_| "command reap failed")?;
    let (out, err) = output?;
    if !status.success() {
        return Err(format!(
            "command failed: {}{}",
            String::from_utf8_lossy(&out),
            String::from_utf8_lossy(&err)
        ));
    }
    Ok(format!(
        "{}{}",
        String::from_utf8_lossy(&out),
        String::from_utf8_lossy(&err)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn kill_guard_runs_before_isolation_or_spawn() {
        let result = execute(
            Path::new("/not-a-worktree"),
            "pkill -f node",
            Limits {
                timeout: Duration::from_secs(1),
                output_bytes: 1024,
            },
            &CancellationToken::new(),
        )
        .await;
        assert!(result.unwrap_err().contains("node"));
    }
}

#[cfg(test)]
mod process_tests {
    use super::*;
    fn limits() -> Limits {
        Limits {
            timeout: Duration::from_secs(1),
            output_bytes: 1024,
        }
    }
    #[tokio::test]
    async fn macos_local_command_has_only_whitelisted_environment_and_returns_success() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "env"]);
        let output = supervise(cmd, Path::new("/tmp"), limits(), &CancellationToken::new())
            .await
            .unwrap();
        assert!(output.lines().any(|l| l == "PATH=/usr/bin:/bin"));
        for line in output.lines() {
            let key = line.split('=').next().unwrap();
            assert!(
                matches!(key, "PATH" | "LANG" | "PWD" | "SHLVL" | "_"),
                "unexpected environment key: {key}"
            );
        }
    }
    #[tokio::test]
    async fn macos_local_command_caps_output_and_times_out_then_reaps_its_pid() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args([
            "-c",
            "while :; do printf xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; done",
        ]);
        assert!(
            supervise(cmd, Path::new("/tmp"), limits(), &CancellationToken::new())
                .await
                .unwrap_err()
                .contains("output limit")
        );
        let tree = crate::testing::track(
            std::env::temp_dir().join(format!("am-api-command-{}", crate::db::ulid())),
        );
        std::fs::create_dir(&tree).unwrap();
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "echo $$ > pid; exec /bin/sleep 60"]);
        let result = supervise(cmd, &tree, limits(), &CancellationToken::new()).await;
        let pid: i32 = std::fs::read_to_string(tree.join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        std::fs::remove_dir_all(tree).unwrap();
        assert!(result.unwrap_err().contains("timed out"));
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
    #[tokio::test]
    async fn macos_local_command_cancellation_reaps_recorded_pid() {
        let tree = crate::testing::track(
            std::env::temp_dir().join(format!("am-api-cancel-{}", crate::db::ulid())),
        );
        std::fs::create_dir(&tree).unwrap();
        let cancel = CancellationToken::new();
        let signal = cancel.clone();
        let root = tree.clone();
        let task = tokio::spawn(async move {
            let mut cmd = Command::new("/bin/sh");
            cmd.args(["-c", "echo $$ > pid; exec /bin/sleep 60"]);
            supervise(
                cmd,
                &root,
                Limits {
                    timeout: Duration::from_secs(10),
                    ..limits()
                },
                &signal,
            )
            .await
        });
        let pid: i32 = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(s) = tokio::fs::read_to_string(tree.join("pid")).await {
                    if let Ok(pid) = s.trim().parse() {
                        break pid;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        cancel.cancel();
        assert_eq!(task.await.unwrap().unwrap_err(), "cancelled");
        std::fs::remove_dir_all(tree).unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
}
