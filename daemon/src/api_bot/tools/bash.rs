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
    editor: &super::editor::Editor,
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
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        let directory = editor.directory();
        let metadata = directory.metadata().map_err(|_| "worktree metadata failed")?;
        let fd = directory.as_raw_fd();
        if fd < 3 { return Err("worktree descriptor unavailable".into()); }
        let expected = format!("{}:{}", metadata.dev(), metadata.ino());
        let check = format!("test \"$(/usr/bin/stat -Lc '%d:%i' /workspace)\" = \"$1\" || exit 125; exec {fd}<&-; exec /bin/bash --noprofile --norc -c \"$2\"");
        let mut cmd = Command::new(bwrap);
        // In the child only, mark every nonstandard descriptor close-on-exec, then
        // allow exactly the pinned directory descriptor through the bwrap exec.
        // An unsupported close_range or bind-fd fails closed before the command.
        unsafe { cmd.pre_exec(move || {
            if libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 4u32) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        }); }
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
            "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--bind-fd",
        ])
        .arg(fd.to_string())
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
        ]).arg(check).arg("sandbox-check").arg(expected).arg(command);
        supervise(cmd, editor.root(), limits, cancel).await
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = editor;
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
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires approved real bubblewrap validation window"]
    async fn real_bwrap_isolation() {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::{MetadataExt, symlink};
        { use std::os::unix::fs::PermissionsExt; let m = std::fs::metadata("/usr/bin/bwrap").unwrap(); assert!(m.is_file() && m.permissions().mode() & 0o111 != 0); }
        assert_eq!(std::env::var("KX090P_FAKE_SENTINEL").as_deref(), Ok("synthetic"));
        let root = crate::testing::track(std::env::temp_dir().join(format!("native-bwrap-{}", crate::db::ulid())));
        let workspace = root.join("workspace"); let outside = root.join("outside");
        std::fs::create_dir_all(&workspace).unwrap(); std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("sentinel"), "synthetic").unwrap();
        symlink(outside.join("sentinel"), workspace.join("escape")).unwrap();
        let file = std::fs::File::create(workspace.join("fake-fd")).unwrap();
        let fd = file.as_raw_fd(); let meta = file.metadata().unwrap();
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFD, 0) }, 0);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let names = ["user", "mnt", "pid", "net"];
        let host_inodes: Vec<u64> = names.iter().map(|name| std::fs::metadata(format!("/proc/self/ns/{name}")).unwrap().ino()).collect();
        let python = format!("import os,socket; print(' '.join(str(os.stat('/proc/self/ns/'+n).st_ino) for n in ('user','mnt','pid','net'))); assert os.environ.get('KX090P_FAKE_SENTINEL') is None; assert set(os.environ)<=set(('PATH','HOME','LANG','PWD','SHLVL','_'));\ntry:\n s=os.fstat({fd}); assert (s.st_dev,s.st_ino)!=({dev},{ino})\nexcept OSError: pass\ns=socket.socket(); s.settimeout(1); assert s.connect_ex(('127.0.0.1',{port}))!=0", dev=meta.dev(), ino=meta.ino());
        let command = format!("test ! -e /workspace/escape && test ! -e /home && printf synthetic > /workspace/writable && /usr/bin/python3 -c \"{}\"", python.replace('\"', "\\\""));
        let command = format!("! /bin/ln -L /workspace/escape /workspace/hardlink && /bin/ln -s /workspace/escape /workspace/symlink && test ! -e /workspace/symlink && {command}");
        let editor = super::super::editor::Editor::new(&workspace).unwrap();
        let output = execute(&editor, &command, Limits { timeout: Duration::from_secs(5), output_bytes: 4096 }, &CancellationToken::new()).await;
        drop(file);
        let sandbox_inodes: Vec<u64> = output.as_deref().unwrap_or("").split_whitespace().filter_map(|s| s.parse().ok()).collect();
        let listener = listener.into_std().unwrap();
        let no_connection = matches!(listener.accept(), Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock);
        let writable = std::fs::read_to_string(workspace.join("writable")).ok().as_deref() == Some("synthetic");
        drop(listener); std::fs::remove_dir_all(&root).unwrap();
        assert!(output.is_ok() && no_connection && writable);
        assert_eq!(sandbox_inodes.len(), host_inodes.len());
        assert!(sandbox_inodes.iter().zip(host_inodes).all(|(inside, outside)| *inside != outside));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn editor_fd_pins_identity_across_workspace_replacement() {
        use std::os::unix::fs::MetadataExt;
        let root = crate::testing::track(std::env::temp_dir().join(format!("native-pin-unit-{}", crate::db::ulid())));
        let workspace = root.join("workspace"); let original = root.join("original");
        std::fs::create_dir_all(&workspace).unwrap();
        let editor = super::super::editor::Editor::new(&workspace).unwrap();
        let before = editor.directory().metadata().unwrap();
        std::fs::rename(&workspace, &original).unwrap(); std::fs::create_dir(&workspace).unwrap();
        let after = editor.directory().metadata().unwrap(); let replacement = std::fs::metadata(&workspace).unwrap();
        assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
        assert_ne!((after.dev(), after.ino()), (replacement.dev(), replacement.ino()));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires approved real bubblewrap validation window"]
    async fn real_bwrap_pinned_workspace_survives_rename_and_replacement() {
        let root = crate::testing::track(std::env::temp_dir().join(format!("native-pin-{}", crate::db::ulid())));
        let workspace = root.join("workspace"); let original = root.join("original");
        std::fs::create_dir_all(&workspace).unwrap();
        let editor = super::super::editor::Editor::new(&workspace).unwrap();
        let ready = execute(&editor, "true", Limits { timeout: Duration::from_secs(5), output_bytes: 1024 }, &CancellationToken::new()).await;
        assert!(ready.is_ok(), "bubblewrap setup or fd binding unavailable");
        std::fs::rename(&workspace, &original).unwrap(); std::fs::create_dir(&workspace).unwrap();
        let result = execute(&editor, "printf synthetic > marker", Limits { timeout: Duration::from_secs(5), output_bytes: 1024 }, &CancellationToken::new()).await;
        let pinned = original.join("marker").exists(); let replacement = workspace.join("marker").exists();
        std::fs::remove_dir_all(&root).unwrap();
        assert!(result.is_err() || pinned, "command must be refused or run in the pinned directory");
        assert!(!replacement, "replacement directory must never receive the write");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires approved real bubblewrap validation window"]
    async fn real_bwrap_timeout_cancellation_descendants_and_output() {
        { use std::os::unix::fs::PermissionsExt; let m = std::fs::metadata("/usr/bin/bwrap").unwrap(); assert!(m.is_file() && m.permissions().mode() & 0o111 != 0); }
        let root = crate::testing::track(std::env::temp_dir().join(format!("native-bwrap-{}", crate::db::ulid())));
        std::fs::create_dir(&root).unwrap();
        let limits = Limits { timeout: Duration::from_secs(2), output_bytes: 1024 };
        let editor = super::super::editor::Editor::new(&root).unwrap();
        let capped = execute(&editor, "/usr/bin/yes x", limits, &CancellationToken::new()).await;
        fn start_time(pid: u32) -> Option<u64> {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            stat.rsplit_once(')')?.1.split_whitespace().nth(19)?.parse().ok()
        }
        fn children(pid: u32) -> Vec<u32> {
            let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else { return vec![]; };
            tasks.flatten().flat_map(|entry| std::fs::read_to_string(entry.path().join("children")).unwrap_or_default().split_whitespace().filter_map(|s| s.parse().ok()).collect::<Vec<u32>>()).collect()
        }
        fn descendants(root: u32, out: &mut Vec<(u32,u64)>) {
            if let Some(start) = start_time(root) { out.push((root,start)); }
            for child in children(root) { descendants(child, out); }
        }
        for cancel_mode in [false, true] {
            let me = std::process::id();
            let before = children(me);
            let token = CancellationToken::new(); let worker_token = token.clone(); let worker_root = root.clone();
            let task = tokio::spawn(async move { let editor = super::super::editor::Editor::new(&worker_root).unwrap(); execute(&editor, "/usr/bin/setsid /bin/bash -c 'while :; do /bin/sleep 1; done' >/dev/null 2>&1 & printf ready > ready; wait", Limits { timeout: Duration::from_secs(if cancel_mode { 8 } else { 2 }), output_bytes: 1024 }, &worker_token).await });
            tokio::time::timeout(Duration::from_secs(5), async { while !root.join("ready").exists() { tokio::task::yield_now().await; } }).await.unwrap();
            let roots: Vec<u32> = children(me).into_iter().filter(|pid| !before.contains(pid)).collect();
            let mut owned = Vec::new();
            for pid in roots { descendants(pid, &mut owned); }
            if cancel_mode { token.cancel(); }
            let result = tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                while owned.iter().any(|(pid,start)| start_time(*pid) == Some(*start)) { tokio::task::yield_now().await; }
            }).await.unwrap();
            assert!(owned.len() >= 2, "the test must observe its own sandbox and descendant PIDs");
            assert!(matches!(result, Err(ref e) if e == if cancel_mode { "cancelled" } else { "command timed out" }));
            std::fs::remove_file(root.join("ready")).unwrap();
        }
        std::fs::remove_dir_all(&root).unwrap();
        assert!(matches!(capped, Err(ref e) if e == "command output limit exceeded"));
    }

    #[tokio::test]
    async fn kill_guard_runs_before_isolation_or_spawn() {
        let result = execute(
            &super::super::editor::Editor::new(Path::new("/tmp")).unwrap(),
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
