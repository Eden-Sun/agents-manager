//! Remote hosts over SSH (SPEC §11.3).
//!
//! One `ssh -N -M` master per host forwards the remote herdr socket to a **short** local path
//! (macOS AF_UNIX caps paths at 104 bytes). That single `-L` is the whole tunnel: hooks report
//! via the remote herdr + a spool file read over ssh (SPEC §11.4), so no `-R` / `hook_port`.

use crate::config::{HostCfg, LOCAL_HOST};
use crate::herdr::HerdrClient;
use crate::state::App;
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// SPEC §11.3.4.
const PING_INTERVAL: Duration = Duration::from_secs(10);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
const MASTER_UP_TIMEOUT: Duration = Duration::from_secs(20);
const SSH_EXEC_TIMEOUT: Duration = Duration::from_secs(30);
const SSH_PUT_TIMEOUT: Duration = Duration::from_secs(120);

/// Short on purpose: AF_UNIX path limit. Namespaced by daemon instance (`startup::instance_slug`)
/// so an isolated/alt-data-dir daemon managing the same remote host name never shares this
/// production daemon's SSH master control socket or forwarded herdr socket (issue #85) —
/// otherwise one instance's explicit reconnect or shutdown kills the other's tunnel.
pub fn short_dir(instance: Option<&str>) -> PathBuf {
    use std::os::unix::fs::MetadataExt;
    let uid = dirs::home_dir().and_then(|h| std::fs::metadata(h).ok()).map(|m| m.uid()).unwrap_or(0);
    match instance {
        Some(slug) => PathBuf::from(format!("/tmp/agents-manager-{uid}-{slug}")),
        None => PathBuf::from(format!("/tmp/agents-manager-{uid}")),
    }
}

pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// `None` = timed out. The whole process group is killed so a hung `git merge` (pinentry,
/// stuck remote) cannot keep holding `index.lock` after the caller gave up.
pub async fn sh_local(script: &str, timeout: Duration) -> Result<Option<std::process::Output>> {
    let mut cmd = tokio::process::Command::new("/bin/sh");
    cmd.arg("-c").arg(script).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.process_group(0);
    cmd.kill_on_drop(true);
    let mut child = cmd.spawn().context("spawn /bin/sh")?;
    let pid = child.id();
    let out = child.stdout.take();
    let err = child.stderr.take();
    let gather = async move {
        use tokio::io::AsyncReadExt;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        if let Some(mut o) = out {
            o.read_to_end(&mut stdout).await.ok();
        }
        if let Some(mut e) = err {
            e.read_to_end(&mut stderr).await.ok();
        }
        (stdout, stderr)
    };
    let (status, (stdout, stderr)) = tokio::select! {
        r = async { tokio::join!(child.wait(), gather) } => (r.0.context("wait /bin/sh")?, r.1),
        _ = tokio::time::sleep(timeout) => {
            if let Some(pid) = pid {
                let _ = std::process::Command::new("/bin/kill").args(["-9", "--", &format!("-{pid}")]).status();
            }
            let _ = child.kill().await;
            return Ok(None);
        }
    };
    Ok(Some(std::process::Output { status, stdout, stderr }))
}

/// PATH 以 `:` 分項，每項各自 quote，所以空白、`;`、`$(…)` 是資料不是指令；只有項目**開頭**的
/// `$HOME`／`${HOME}`／`~` 展開成遠端 home（API.md 的範例就是 `/opt/homebrew/bin:$HOME/.local/bin`，
/// 整串包單引號會讓 `$HOME` 留成字面，遠端找不到 herdr，#241）。
pub fn remote_path_prefix(remote_path: &str) -> String {
    let p = remote_path.trim();
    if p.is_empty() {
        return String::new();
    }
    let entries: Vec<String> = p
        .split(':')
        .filter(|e| !e.is_empty())
        .map(|e| {
            for home in ["${HOME}", "$HOME", "~"] {
                if let Some(rest) = e.strip_prefix(home) {
                    if rest.is_empty() || rest.starts_with('/') {
                        return if rest.is_empty() { "\"$HOME\"".to_string() } else { format!("\"$HOME\"{}", sh_quote(rest)) };
                    }
                }
            }
            sh_quote(e)
        })
        .collect();
    if entries.is_empty() {
        return String::new();
    }
    format!("export PATH={}:\"$PATH\"\n", entries.join(":"))
}

/// `remote_path` 拆成 PATH 片段給 pane env 用（herdr 照字面設 env、不經 shell）：規則同
/// [`remote_path_prefix`]——只有項目**開頭**的 `$HOME`／`${HOME}`／`~` 換成那台的 home，其餘照字面。
pub fn remote_path_dirs(remote_path: &str, home: &str) -> Vec<String> {
    remote_path
        .trim()
        .split(':')
        .filter(|e| !e.is_empty())
        .map(|e| {
            for h in ["${HOME}", "$HOME", "~"] {
                if let Some(rest) = e.strip_prefix(h) {
                    if rest.is_empty() || rest.starts_with('/') {
                        return format!("{home}{rest}");
                    }
                }
            }
            e.to_string()
        })
        .collect()
}

pub struct HostConn {
    pub name: String,
    /// `None` for `local`.
    pub cfg: Option<HostCfg>,
    pub client: HerdrClient,
    pub connected: AtomicBool,
    pub error: Mutex<Option<String>>,
    pub remote_home: Mutex<Option<String>>,
    master: Mutex<Option<tokio::process::Child>>,
    supervisor: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Bumped on every explicit reconnect so a stale supervisor exits.
    generation: std::sync::atomic::AtomicU64,
    /// [`HostFence`] tickets handed out / the newest ticket whose result was published (issue #347).
    fence_tickets: std::sync::atomic::AtomicU64,
    fence_published: std::sync::atomic::AtomicU64,
    /// This daemon's instance slug at the time this host was (re)configured — namespaces the
    /// ctl/sock paths (issue #85). Unused for `local` (never spawns an SSH master).
    instance: Option<String>,
}

impl HostConn {
    fn local(client: HerdrClient) -> Arc<Self> {
        Arc::new(Self {
            name: LOCAL_HOST.to_string(),
            cfg: None,
            client,
            connected: AtomicBool::new(false),
            error: Mutex::new(None),
            remote_home: Mutex::new(dirs::home_dir().map(|p| p.to_string_lossy().to_string())),
            master: Mutex::new(None),
            supervisor: Mutex::new(None),
            generation: std::sync::atomic::AtomicU64::new(0),
            fence_tickets: std::sync::atomic::AtomicU64::new(0),
            fence_published: std::sync::atomic::AtomicU64::new(0),
            instance: None,
        })
    }

    fn remote(cfg: HostCfg, instance: Option<String>) -> Arc<Self> {
        let sock = short_dir(instance.as_deref()).join(format!("{}.sock", cfg.name));
        Arc::new(Self {
            name: cfg.name.clone(),
            client: HerdrClient::new(sock),
            cfg: Some(cfg),
            connected: AtomicBool::new(false),
            error: Mutex::new(None),
            remote_home: Mutex::new(None),
            master: Mutex::new(None),
            supervisor: Mutex::new(None),
            generation: std::sync::atomic::AtomicU64::new(0),
            fence_tickets: std::sync::atomic::AtomicU64::new(0),
            fence_published: std::sync::atomic::AtomicU64::new(0),
            instance,
        })
    }

    pub fn is_local(&self) -> bool {
        self.cfg.is_none()
    }

    /// Test seam: what an explicit reconnect does to superseded observations.
    #[cfg(test)]
    pub(crate) fn bump_generation_for_test(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    pub async fn error_string(&self) -> Option<String> {
        self.error.lock().await.clone()
    }

    fn ctl_path(&self) -> PathBuf {
        short_dir(self.instance.as_deref()).join(format!("{}.ctl", self.name))
    }

    fn ssh_args(&self) -> Vec<String> {
        let Some(cfg) = &self.cfg else { return vec![] };
        let mut v: Vec<String> = vec![
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "ServerAliveInterval=15".into(),
            "-o".into(),
            "ServerAliveCountMax=3".into(),
        ];
        // Only override the port when it is not the default, so ssh_config aliases keep theirs.
        if cfg.ssh_port != 22 {
            v.push("-p".into());
            v.push(cfg.ssh_port.to_string());
        }
        v.extend(cfg.ssh_opts.iter().cloned());
        v
    }

    /// Script is piped into `/bin/sh -s`, not argv: the remote login shell may be zsh/fish,
    /// and this avoids a second round of quoting.
    pub async fn ssh_exec(&self, script: &str) -> Result<String> {
        self.ssh_exec_timeout(script, SSH_EXEC_TIMEOUT).await
    }

    pub async fn ssh_exec_timeout(&self, script: &str, timeout: Duration) -> Result<String> {
        // 假貨是同步的，永遠不會把控制權交回 executor，所以「主機收下 TCP 但不回話」這種**逾時**光靠假貨測不到
        // （`tokio::time::timeout` 是合作式的）。這個延遲是真的 await point，讓呼叫端的上限測得出來（#407 review）。
        #[cfg(test)]
        if let Some(d) = ssh_delay_for(&self.name) {
            tokio::time::sleep(d).await;
        }
        #[cfg(test)]
        if let Some(f) = ssh_fake_for(&self.name) {
            return f(script);
        }
        let Some(cfg) = &self.cfg else { bail!("ssh_exec called on the local host") };
        let mut cmd = tokio::process::Command::new("ssh");
        cmd.args(self.ssh_args()).arg(&cfg.ssh).arg("/bin/sh").arg("-s");
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd.kill_on_drop(true);
        let mut child = cmd.spawn().with_context(|| format!("spawn ssh {}", cfg.ssh))?;
        if let Some(mut sin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let script = script.to_string();
            sin.write_all(script.as_bytes()).await.ok();
            sin.shutdown().await.ok();
        }
        let out = tokio::time::timeout(timeout, child.wait_with_output())
            .await
            .map_err(|_| anyhow::anyhow!("ssh to {} timed out after {}s", cfg.ssh, timeout.as_secs()))?
            .with_context(|| format!("run ssh {}", cfg.ssh))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            bail!("ssh {} failed ({}): {}", cfg.ssh, out.status, if err.is_empty() { "no stderr".into() } else { err });
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// Script in argv, stdin carries the file (`ssh_exec` already uses stdin for the script).
    pub async fn ssh_put(&self, path: &str, data: &[u8]) -> Result<()> {
        let Some(cfg) = &self.cfg else { bail!("ssh_put called on the local host") };
        let dir = match path.rsplit_once('/') {
            Some((d, _)) if !d.is_empty() => d,
            _ => ".",
        };
        let script = format!("mkdir -p {} && cat > {}", sh_quote(dir), sh_quote(path));
        // ssh joins argv and hands it to the remote login shell: quote once more for that split.
        let remote = format!("/bin/sh -c {}", sh_quote(&script));
        let mut cmd = tokio::process::Command::new("ssh");
        cmd.args(self.ssh_args()).arg(&cfg.ssh).arg(&remote);
        let (out, wrote) = run_with_stdin(cmd, data, SSH_PUT_TIMEOUT)
            .await
            .with_context(|| format!("run ssh {}", cfg.ssh))?
            .ok_or_else(|| anyhow::anyhow!("ssh to {} timed out writing {}", cfg.ssh, path))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            bail!("ssh {} could not write {} ({}): {}", cfg.ssh, path, out.status, if err.is_empty() { "no stderr".into() } else { err });
        }
        wrote.with_context(|| format!("write {} to {}", path, cfg.ssh))?;
        Ok(())
    }

    /// For payloads like a gh token: script in argv, bytes on stdin (same quoting as `ssh_put`).
    /// Failures report stderr only — stdout may be sensitive.
    pub async fn ssh_exec_stdin(&self, script: &str, data: &[u8], timeout: Duration) -> Result<String> {
        let Some(cfg) = &self.cfg else { bail!("ssh_exec_stdin called on the local host") };
        let remote = format!("/bin/sh -c {}", sh_quote(script));
        let mut cmd = tokio::process::Command::new("ssh");
        cmd.args(self.ssh_args()).arg(&cfg.ssh).arg(&remote);
        let (out, wrote) = run_with_stdin(cmd, data, timeout)
            .await
            .with_context(|| format!("run ssh {}", cfg.ssh))?
            .ok_or_else(|| anyhow::anyhow!("ssh to {} timed out", cfg.ssh))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            bail!("ssh {} failed ({}): {}", cfg.ssh, out.status, if err.is_empty() { "no stderr".into() } else { err });
        }
        wrote.with_context(|| format!("write stdin to {}", cfg.ssh))?;
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// SPEC §11.2 `remote_path`.
    fn path_prefix(&self) -> String {
        remote_path_prefix(self.cfg.as_ref().map(|c| c.remote_path.as_str()).unwrap_or_default())
    }

    pub async fn ssh_exec_path_stdin(&self, script: &str, data: &[u8], timeout: Duration) -> Result<String> {
        self.ssh_exec_stdin(&format!("{}{script}", self.path_prefix()), data, timeout).await
    }

    pub async fn ssh_exec_path(&self, script: &str) -> Result<String> {
        self.ssh_exec_path_timeout(script, SSH_EXEC_TIMEOUT).await
    }

    pub async fn ssh_exec_path_timeout(&self, script: &str, timeout: Duration) -> Result<String> {
        self.ssh_exec_timeout(&format!("{}{script}", self.path_prefix()), timeout).await
    }

    pub async fn home(&self) -> Result<String> {
        if let Some(h) = self.remote_home.lock().await.clone() {
            return Ok(h);
        }
        let h = self.ssh_exec("printf '%s' \"$HOME\"").await?.trim().to_string();
        if h.is_empty() {
            bail!("remote $HOME is empty");
        }
        *self.remote_home.lock().await = Some(h.clone());
        Ok(h)
    }

    /// SPEC §11.3.1. On macOS herdr runs as a launchd GUI-domain agent, not `nohup` from ssh:
    /// an ssh-spawned process can't read the login Keychain, so Claude Code inside reports
    /// "Not logged in". Falls back to `nohup` when not macOS / no console owner / launchctl refuses.
    async fn ensure_remote_session(&self) -> Result<String> {
        let cfg = self.cfg.as_ref().unwrap();
        let sess = &cfg.herdr_session;
        let q = sh_quote(sess);
        // The plist is XML: `&`, `<`, `>` in a path would break the whole file, not just PATH.
        let path_prefix = if cfg.remote_path.trim().is_empty() {
            String::new()
        } else {
            format!("{}:", cfg.remote_path.trim().replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;"))
        };
        let script = format!(
            r#"printf 'AM_HOME=%s\n' "$HOME"
S={q}
SOCK="$HOME/.config/herdr/sessions/$S/herdr.sock"
LABEL="dev.agents-manager.herdr-$S"
running() {{ herdr session list 2>/dev/null | grep -q "^$S[[:space:]].*running"; }}
HERDR_BIN=$(command -v herdr 2>/dev/null)
CONSOLE_USER=$(stat -f %Su /dev/console 2>/dev/null || true)
MODE=nohup
if [ "$(uname -s)" = Darwin ] && [ -n "$HERDR_BIN" ] && [ "$CONSOLE_USER" = "$(id -un)" ] && command -v launchctl >/dev/null 2>&1; then
  MODE=launchd
fi
if [ "$MODE" = launchd ]; then
  PL="$HOME/Library/LaunchAgents/$LABEL.plist"
  mkdir -p "$HOME/Library/LaunchAgents"
  if launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1; then
    printf 'AM_MODE=launchd-existing\n'
  else
    cat > "$PL" <<AM_PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>$LABEL</string>
  <key>ProgramArguments</key><array><string>$HERDR_BIN</string><string>--session</string><string>$S</string><string>server</string></array>
  <key>EnvironmentVariables</key><dict><key>PATH</key><string>{path_prefix}$PATH</string></dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardOutPath</key><string>/tmp/herdr-$S.log</string>
  <key>StandardErrorPath</key><string>/tmp/herdr-$S.log</string>
</dict></plist>
AM_PLIST
    if running; then
      # A server started the old way (nohup) is still up: stop it so launchd owns the next one.
      herdr --session "$S" server stop >/dev/null 2>&1 || true
      sleep 1
    fi
    if launchctl bootstrap "gui/$(id -u)" "$PL" 2>/tmp/am-launchctl.err; then
      printf 'AM_MODE=launchd\n'
    else
      printf 'AM_MODE=nohup-fallback launchctl: %s\n' "$(tr '\n' ' ' </tmp/am-launchctl.err)"
      MODE=nohup
    fi
  fi
fi
if [ "$MODE" = nohup ] && ! running; then
  # `nohup` refuses to detach when stderr is not a console on some macOS builds,
  # so ignore SIGHUP in a subshell instead.
  ( trap '' HUP; herdr --session "$S" server </dev/null >"/tmp/herdr-$S.log" 2>&1 & )
  printf 'AM_MODE=nohup\n'
  sleep 1
fi
i=0
while [ $i -lt 20 ]; do
  if [ -S "$SOCK" ] && running; then
    printf 'AM_OK=1\n'; break
  fi
  i=$((i+1)); sleep 1
done
printf 'AM_SOCK=%s\n' "$SOCK"
herdr session list 2>&1 | sed 's/^/AM_LIST /'
"#
        );
        let out = self.ssh_exec_path(&script).await?;
        let mut home = None;
        let mut sock = None;
        let mut ok = false;
        let mut mode = String::from("?");
        for line in out.lines() {
            if let Some(v) = line.strip_prefix("AM_HOME=") {
                home = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("AM_SOCK=") {
                sock = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("AM_MODE=") {
                mode = v.trim().to_string();
            } else if line.starts_with("AM_OK=1") {
                ok = true;
            }
        }
        if let Some(h) = home {
            *self.remote_home.lock().await = Some(h);
        }
        let sock = sock.filter(|s| !s.is_empty()).ok_or_else(|| anyhow::anyhow!("could not determine remote herdr socket path; is herdr on the remote PATH? (remote_path)\n{out}"))?;
        if !ok {
            bail!("remote herdr session `{sess}` did not come up (mode {mode}):\n{}", out.trim());
        }
        tracing::info!(host = %self.name, session = %sess, socket = %sock, mode = %mode, "remote session ensured");
        Ok(sock)
    }

    /// SPEC §11.3.2.
    async fn start_master(&self, remote_sock: &str) -> Result<()> {
        let cfg = self.cfg.as_ref().unwrap();
        let dir = short_dir(self.instance.as_deref());
        std::fs::create_dir_all(&dir).ok();
        let ctl = self.ctl_path();
        let local_sock = short_dir(self.instance.as_deref()).join(format!("{}.sock", self.name));
        if local_sock.to_string_lossy().len() > 100 {
            bail!("forwarded socket path is too long for AF_UNIX: {}", local_sock.display());
        }

        self.kill_master().await;

        let mut cmd = tokio::process::Command::new("ssh");
        cmd.arg("-N")
            .arg("-M")
            .arg("-S")
            .arg(&ctl)
            .args(self.ssh_args())
            .arg("-o")
            .arg("ExitOnForwardFailure=yes")
            .arg("-o")
            .arg("StreamLocalBindUnlink=yes")
            .arg("-L")
            .arg(format!("{}:{}", local_sock.display(), remote_sock));
        cmd.arg(&cfg.ssh);
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
        cmd.kill_on_drop(false);
        let mut child = cmd.spawn().context("spawn ssh master")?;
        let stderr = child.stderr.take();
        let name = self.name.clone();
        if let Some(mut e) = stderr {
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut buf = Vec::new();
                let _ = e.read_to_end(&mut buf).await;
                let s = String::from_utf8_lossy(&buf);
                for line in s.lines().filter(|l| !l.trim().is_empty()) {
                    tracing::warn!(host = %name, "ssh master: {line}");
                }
            });
        }
        *self.master.lock().await = Some(child);

        let deadline = std::time::Instant::now() + MASTER_UP_TIMEOUT;
        loop {
            if let Some(m) = self.master.lock().await.as_mut() {
                if let Ok(Some(st)) = m.try_wait() {
                    bail!("ssh master exited immediately ({st}); check credentials and the ssh target");
                }
            }
            if self.client.ping().await.is_ok() {
                tracing::info!(host = %self.name, socket = %local_sock.display(), "ssh master up; herdr ping ok");
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                self.kill_master().await;
                bail!("herdr did not answer over the forwarded socket within {MASTER_UP_TIMEOUT:?}");
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    /// SPEC §11.3.5 — close the master; the remote herdr server and its agents stay alive.
    pub async fn kill_master(&self) {
        if self.is_local() {
            return;
        }
        let cfg = self.cfg.as_ref().unwrap();
        let ctl = self.ctl_path();
        if ctl.exists() {
            let mut cmd = tokio::process::Command::new("ssh");
            cmd.arg("-S").arg(&ctl).arg("-O").arg("exit").args(self.ssh_args()).arg(&cfg.ssh);
            cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
            let _ = tokio::time::timeout(Duration::from_secs(5), cmd.status()).await;
        }
        if let Some(mut child) = self.master.lock().await.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        let _ = std::fs::remove_file(&ctl);
        let _ = std::fs::remove_file(short_dir(self.instance.as_deref()).join(format!("{}.sock", self.name)));
    }
}

/// 把 `data` 餵進子行程的 stdin 並等它結束，**寫入與等待共用同一個 `timeout`**（#282）。
/// 以前 `write_all` 在 timeout 外面：資料大於 pipe buffer、對端活著但不讀時，寫入永遠 pending，逾時根本不啟動。
/// `None`＝逾時（`kill_on_drop` 會收掉 ssh）；`Some((輸出, 寫入結果))`——行程先退出時寫入會 EPIPE，
/// 呼叫端先看 exit status／stderr（那才是原因），行程成功但寫入失敗才報寫入錯。
async fn run_with_stdin(
    mut cmd: tokio::process::Command,
    data: &[u8],
    timeout: Duration,
) -> std::io::Result<Option<(std::process::Output, std::io::Result<()>)>> {
    use tokio::io::AsyncWriteExt;
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    let mut child = cmd.spawn()?;
    let mut sin = child.stdin.take();
    let write = async {
        let r = match sin.as_mut() {
            Some(s) => s.write_all(data).await,
            None => Ok(()),
        };
        if let Some(mut s) = sin.take() {
            s.shutdown().await.ok();
        }
        r
    };
    let run = async { tokio::join!(write, child.wait_with_output()) };
    match tokio::time::timeout(timeout, run).await {
        Err(_) => Ok(None),
        Ok((wrote, out)) => Ok(Some((out?, wrote))),
    }
}

/// SPEC §11.3.4.
fn spawn_supervisor(app: Arc<App>, conn: Arc<HostConn>, generation: u64) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut backoff = BACKOFF_MIN;
        loop {
            if conn.generation.load(Ordering::SeqCst) != generation {
                return; // superseded by a reconnect / config change
            }
            let attempt = async {
                let sock = conn.ensure_remote_session().await?;
                conn.start_master(&sock).await?;
                Ok::<_, anyhow::Error>(())
            }
            .await;

            match attempt {
                Ok(()) => {
                    backoff = BACKOFF_MIN;
                    conn.connected.store(true, Ordering::SeqCst);
                    *conn.error.lock().await = None;
                    crate::state::emit_host_changed(&app, &conn).await;

                    let reconciled = match crate::reconcile::reconcile_host(&app, &conn.name).await {
                        Ok(()) => true,
                        Err(e) => {
                            tracing::error!(host = %conn.name, error = ?e, "reconcile after connect failed");
                            false
                        }
                    };
                    crate::events::spawn_global_for_host(app.clone(), conn.name.clone()).await;
                    crate::hookrecv::replay_host(&app, &conn.name).await;
                    crate::tools::spawn_detect(app.clone(), conn.name.clone());
                    // 刪除 handler 的一次性 ssh purge 若在送出前 daemon 就死了，這台的已刪 bot 目錄靠連上時再掃一次收掉（#349）。
                    crate::remote_purge::spawn_sweep(app.clone(), conn.name.clone());
                    // daemon 升級後，長跑的遠端 bot 手上還是舊 shim：連上（重連也一樣）就補版，背景做、不擋連線（issue #124）。
                    crate::shim_refresh::spawn_remote_refresh(app.clone(), conn.name.clone());
                    // 同一個道理的權限：#494 的收緊在「啟動 bot」那一趟，換版前就在跑的遠端 bot 要等重啟才收得到（issue #501）。
                    crate::remote_perms::spawn_tighten(app.clone(), conn.name.clone());
                    // 開機那一輪跑的時候這台還沒連上，它的 autostart bot 因此從來沒被起過（review 2026-09-16）。
                    // 對帳成功才跑、每台一生一次：重連不能把使用者停掉的 bot 再開起來（core 5）。
                    crate::reconcile::autostart_after_reconcile(&app, &conn.name, reconciled).await;

                    loop {
                        tokio::time::sleep(PING_INTERVAL).await;
                        if conn.generation.load(Ordering::SeqCst) != generation {
                            return;
                        }
                        let master_dead = match conn.master.lock().await.as_mut() {
                            Some(m) => matches!(m.try_wait(), Ok(Some(_))),
                            None => true,
                        };
                        if master_dead {
                            tracing::warn!(host = %conn.name, "ssh master exited");
                            *conn.error.lock().await = Some("ssh master exited".into());
                            break;
                        }
                        if let Err(e) = conn.client.ping().await {
                            tracing::warn!(host = %conn.name, error = %e, "host ping failed");
                            *conn.error.lock().await = Some(format!("ping failed: {e}"));
                            break;
                        }
                    }
                    conn.connected.store(false, Ordering::SeqCst);
                    conn.kill_master().await;
                    crate::state::emit_host_changed(&app, &conn).await;
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    tracing::warn!(host = %conn.name, error = %msg, "host connect failed");
                    if conn.generation.load(Ordering::SeqCst) != generation {
                        return;
                    }
                    conn.connected.store(false, Ordering::SeqCst);
                    *conn.error.lock().await = Some(msg);
                    crate::state::emit_host_changed(&app, &conn).await;
                }
            }

            if conn.generation.load(Ordering::SeqCst) != generation {
                return;
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    })
}

/// Test seam: a fake "ssh" per host name, so remote-side effects (`rm -rf` of a bot dir …) can be observed without a network.
#[cfg(test)]
type SshFake = Arc<dyn Fn(&str) -> Result<String> + Send + Sync>;
#[cfg(test)]
static SSH_FAKES: std::sync::Mutex<Vec<(String, SshFake)>> = std::sync::Mutex::new(Vec::new());
#[cfg(test)]
pub(crate) fn set_ssh_fake(host: &str, f: impl Fn(&str) -> Result<String> + Send + Sync + 'static) {
    let mut v = SSH_FAKES.lock().unwrap();
    v.retain(|(h, _)| h != host);
    v.push((host.to_string(), Arc::new(f)));
}
#[cfg(test)]
fn ssh_fake_for(host: &str) -> Option<SshFake> {
    SSH_FAKES.lock().unwrap().iter().find(|(h, _)| h == host).map(|(_, f)| f.clone())
}

/// 以主機名為鍵、描述「那台機器」的快取全部丟掉（#347）：偵測結果（`app.tools`，含身分與 herdr CLI 版本）、
/// 額度（`<host>/…`，連重啟快取列）、模型清單（`<host>/<kind>/<identity>`）、這個 daemon 在那台開的 shell 清單。
/// 移除主機與同名改設定都走這裡；新連線上線後由偵測／探測重新填。
async fn forget_host_observations(app: &Arc<App>, name: &str) {
    let prefix = format!("{name}/");
    app.tools.lock().await.remove(name);
    let removed: Vec<String> = {
        let mut quotas = app.quotas.lock().await;
        let removed = quotas.keys().filter(|k| k.starts_with(&prefix)).cloned().collect();
        quotas.retain(|k, _| !k.starts_with(&prefix));
        removed
    };
    for key in removed {
        crate::quota::forget(app, &key).await;
    }
    app.models_cache.lock().await.retain(|k, _| !k.starts_with(&prefix));
    app.host_shells.lock().await.retain(|s| s.host != name);
}

/// Test seam: make every ssh leg to this host take that long *asynchronously* — a host that accepts the
/// connection and then says nothing. Pair it with [`set_ssh_fake`] for what the reply would have been.
#[cfg(test)]
static SSH_DELAYS: std::sync::Mutex<Vec<(String, Duration)>> = std::sync::Mutex::new(Vec::new());
#[cfg(test)]
pub(crate) fn set_ssh_delay(host: &str, d: Duration) {
    let mut v = SSH_DELAYS.lock().unwrap();
    v.retain(|(h, _)| h != host);
    v.push((host.to_string(), d));
}
#[cfg(test)]
fn ssh_delay_for(host: &str) -> Option<Duration> {
    SSH_DELAYS.lock().unwrap().iter().find(|(h, _)| h == host).map(|(_, d)| *d)
}

/// Authority token for an external observation of one host (issue #347): the connection object and its
/// generation as they were **before** the probe started. The observation may publish (into `app.tools`,
/// `app.quotas`…) only if [`HostManager::is_current`] still holds afterwards — a reconnect bumps the generation,
/// a reconfigure replaces the connection, a removal drops it, and any of those means the result describes a
/// machine that is no longer the authority for this host name. `ticket` orders overlapping observations within
/// one generation: an older one finishing after a newer one has published is discarded.
#[derive(Clone)]
pub struct HostFence {
    conn: Arc<HostConn>,
    generation: u64,
    ticket: u64,
}

impl HostFence {
    pub fn conn(&self) -> &Arc<HostConn> {
        &self.conn
    }

    /// Call while holding the lock of the state being published: true once per ticket, and never for a ticket
    /// older than one that already published.
    pub fn claim_publish(&self) -> bool {
        if self.conn.fence_published.load(Ordering::SeqCst) >= self.ticket {
            return false;
        }
        self.conn.fence_published.store(self.ticket, Ordering::SeqCst);
        true
    }
}

pub struct HostManager {
    conns: Mutex<HashMap<String, Arc<HostConn>>>,
}

impl HostManager {
    pub fn new(local: HerdrClient) -> Self {
        let mut m = HashMap::new();
        m.insert(LOCAL_HOST.to_string(), HostConn::local(local));
        Self { conns: Mutex::new(m) }
    }

    pub async fn get(&self, name: &str) -> Option<Arc<HostConn>> {
        self.conns.lock().await.get(name).cloned()
    }

    /// Capture the current authority for `name` (see [`HostFence`]); `None` = no such host.
    pub async fn fence(&self, name: &str) -> Option<HostFence> {
        let conn = self.get(name).await?;
        let generation = conn.generation.load(Ordering::SeqCst);
        let ticket = conn.fence_tickets.fetch_add(1, Ordering::SeqCst) + 1;
        Some(HostFence { conn, generation, ticket })
    }

    pub async fn is_current(&self, f: &HostFence) -> bool {
        let same = self.conns.lock().await.get(&f.conn.name).is_some_and(|c| Arc::ptr_eq(c, &f.conn));
        same && f.conn.generation.load(Ordering::SeqCst) == f.generation
    }

    /// Test seam: register a remote host without starting a supervisor (a reconfigure replaces it, like `apply_config`).
    #[cfg(test)]
    pub(crate) async fn insert_remote_for_test(&self, cfg: HostCfg) -> Arc<HostConn> {
        let conn = HostConn::remote(cfg, None);
        self.conns.lock().await.insert(conn.name.clone(), conn.clone());
        conn
    }

    /// Test seam: [`Self::apply_config`] 換掉同名連線的那一步（含快取失效），只是不起 supervisor、不真的連 ssh。
    #[cfg(test)]
    pub(crate) async fn replace_remote_for_test(&self, app: &Arc<App>, cfg: HostCfg) -> Arc<HostConn> {
        let conn = HostConn::remote(cfg, app.instance());
        self.install_conn(app, conn.clone()).await;
        conn
    }

    /// 把 `conn` 放上去當這個名字的權威。原本就有同名連線（設定改了、可能改指到另一台）時，舊連線量到的東西
    /// 一律作廢（#347）：先換連線、再清快取——順序反過來的話，清完到換上之間發布的舊觀測會通過權威檢查留下來。
    async fn install_conn(&self, app: &Arc<App>, conn: Arc<HostConn>) {
        let replaced = self.conns.lock().await.insert(conn.name.clone(), conn.clone()).is_some();
        if replaced {
            forget_host_observations(app, &conn.name).await;
        }
    }

    pub async fn client(&self, name: &str) -> Option<HerdrClient> {
        self.conns.lock().await.get(name).map(|c| c.client.clone())
    }

    /// `local` first, then the rest by name.
    pub async fn list(&self) -> Vec<Arc<HostConn>> {
        let g = self.conns.lock().await;
        let mut v: Vec<Arc<HostConn>> = g.values().cloned().collect();
        v.sort_by(|a, b| match (a.is_local(), b.is_local()) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.cmp(&b.name),
        });
        v
    }

    pub async fn names(&self) -> Vec<String> {
        self.list().await.into_iter().map(|c| c.name.clone()).collect()
    }

    /// Supervisors whose config is unchanged are left alone.
    pub async fn apply_config(&self, app: &Arc<App>, hosts: &[HostCfg]) -> HashSet<String> {
        let wanted: HashMap<String, HostCfg> = hosts.iter().map(|h| (h.name.clone(), h.clone())).collect();
        let mut changed_hosts = HashSet::new();

        let existing: Vec<Arc<HostConn>> = self.list().await;
        for c in existing {
            if c.is_local() {
                continue;
            }
            if !wanted.contains_key(&c.name) {
                self.remove(app, &c.name).await;
            }
        }

        for h in hosts {
            // 第二層保險（issue #506）：`projection::validate` 已經擋掉 `name = "local"`，但這支
            // 也被測試與 API 直接呼叫，而「把本機那顆 HostConn 換成 remote」是個沒有回頭路的形狀
            // ——socket 指到沒人在聽的 `<instance>/local.sock`，`is_local()` 翻面之後全部走 ssh。
            if h.name == LOCAL_HOST {
                tracing::warn!(host = %h.name, "[[hosts]] 不能叫 `local`（那是本機）；忽略這一列");
                continue;
            }
            let cur = self.get(&h.name).await;
            let changed = match &cur {
                None => true,
                Some(c) => c.cfg.as_ref().map(|old| cfg_differs(old, h)).unwrap_or(true),
            };
            if !changed {
                continue;
            }
            changed_hosts.insert(h.name.clone());
            if let Some(c) = cur {
                c.generation.fetch_add(1, Ordering::SeqCst);
                if let Some(t) = c.supervisor.lock().await.take() {
                    t.abort();
                }
                c.kill_master().await;
            }
            let conn = HostConn::remote(h.clone(), app.instance());
            self.install_conn(app, conn.clone()).await;
            let gen = conn.generation.load(Ordering::SeqCst);
            let t = spawn_supervisor(app.clone(), conn.clone(), gen);
            *conn.supervisor.lock().await = Some(t);
            tracing::info!(host = %h.name, ssh = %h.ssh, "host configured");
        }
        changed_hosts
    }

    pub async fn remove(&self, app: &Arc<App>, name: &str) {
        let Some(c) = self.conns.lock().await.remove(name) else { return };
        c.generation.fetch_add(1, Ordering::SeqCst);
        if let Some(t) = c.supervisor.lock().await.take() {
            t.abort();
        }
        c.kill_master().await;
        c.connected.store(false, Ordering::SeqCst);
        // Stale `<gone>/claude` quota rows would read as local downstream — SPEC §14.
        forget_host_observations(app, name).await;
        app.emit("host_changed", json!({"name": name, "connected": false, "error": "removed"})).await;
        crate::state::emit_daemon_status(app).await;
        tracing::info!(host = %name, "host removed");
    }

    /// Returns `(connected, error)`.
    pub async fn reconnect(&self, app: &Arc<App>, name: &str) -> Option<(bool, Option<String>)> {
        let conn = self.get(name).await?;
        if conn.is_local() {
            let ok = conn.client.ping().await.is_ok();
            conn.connected.store(ok, Ordering::SeqCst);
            app.connected.store(ok, Ordering::SeqCst);
            *conn.error.lock().await = if ok { None } else { Some("local herdr ping failed".into()) };
            crate::state::emit_host_changed(app, &conn).await;
            return Some((ok, conn.error_string().await));
        }
        conn.generation.fetch_add(1, Ordering::SeqCst);
        if let Some(t) = conn.supervisor.lock().await.take() {
            t.abort();
        }
        conn.kill_master().await;
        conn.connected.store(false, Ordering::SeqCst);
        *conn.error.lock().await = None;
        let gen = conn.generation.load(Ordering::SeqCst);
        let t = spawn_supervisor(app.clone(), conn.clone(), gen);
        *conn.supervisor.lock().await = Some(t);
        self.wait_for_connection(conn).await
    }

    /// Bounded wait so the HTTP caller gets a real answer. Returns `(connected, error)`.
    pub async fn wait_for_connection(&self, conn: Arc<HostConn>) -> Option<(bool, Option<String>)> {
        let deadline = std::time::Instant::now() + MASTER_UP_TIMEOUT + Duration::from_secs(15);
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            if conn.is_connected() {
                return Some((true, None));
            }
            let err = conn.error_string().await;
            if err.is_some() || std::time::Instant::now() >= deadline {
                return Some((false, err.or_else(|| Some("still connecting".into()))));
            }
        }
    }

    /// SPEC §11.3.5.
    pub async fn shutdown(&self) {
        for c in self.list().await {
            if c.is_local() {
                continue;
            }
            c.generation.fetch_add(1, Ordering::SeqCst);
            if let Some(t) = c.supervisor.lock().await.take() {
                t.abort();
            }
            c.kill_master().await;
        }
    }
}

fn cfg_differs(a: &HostCfg, b: &HostCfg) -> bool {
    a.ssh != b.ssh
        || a.ssh_port != b.ssh_port
        || a.ssh_opts != b.ssh_opts
        || a.herdr_session != b.herdr_session
        || a.remote_path != b.remote_path
}

/// `GET /api/fs/dirs?host=` (§11.5); same JSON shape as the local branch.
pub async fn remote_list_dirs(conn: &HostConn, path: Option<&str>, hidden: bool) -> Result<serde_json::Value> {
    let target = match path.map(str::trim).filter(|s| !s.is_empty()) {
        None => "\"$HOME\"".to_string(),
        Some(p) if p == "~" => "\"$HOME\"".to_string(),
        // Tail is user input: must be quoted; only `"$HOME"` stays outside.
        Some(p) if p.starts_with("~/") => format!("\"$HOME\"/{}", sh_quote(&p[2..])),
        Some(p) => sh_quote(p),
    };
    let globs = if hidden { "*/ .*/" } else { "*/" };
    let script = format!(
        r#"cd -- {target} 2>/dev/null || {{ printf 'AM_ERR=no such directory\n'; exit 0; }}
printf 'AM_HOME=%s\n' "$HOME"
printf 'AM_PATH=%s\n' "$(pwd -P)"
printf 'AM_PARENT=%s\n' "$(dirname -- "$(pwd -P)")"
for d in {globs} ; do
  [ -d "$d" ] || continue
  n=${{d%/}}
  case "$n" in '*'|'.*'|.|..) continue;; esac
  if [ -e "$n/.git" ]; then g=1; else g=0; fi
  printf 'AM_D\t%s\t%s\n' "$n" "$g"
done
"#
    );
    let out = conn.ssh_exec(&script).await?;
    let mut home = String::new();
    let mut cwd = String::new();
    let mut parent: Option<String> = None;
    let mut entries: Vec<serde_json::Value> = Vec::new();
    for line in out.lines() {
        if let Some(e) = line.strip_prefix("AM_ERR=") {
            bail!("{}", e.trim());
        } else if let Some(v) = line.strip_prefix("AM_HOME=") {
            home = v.trim_end().to_string();
        } else if let Some(v) = line.strip_prefix("AM_PATH=") {
            cwd = v.trim_end().to_string();
        } else if let Some(v) = line.strip_prefix("AM_PARENT=") {
            parent = Some(v.trim_end().to_string());
        } else if let Some(v) = line.strip_prefix("AM_D\t") {
            let mut it = v.trim_end_matches('\n').splitn(2, '\t');
            let name = it.next().unwrap_or("").to_string();
            let git = it.next().unwrap_or("0") == "1";
            if name.is_empty() || (name.starts_with('.') && !hidden) {
                continue;
            }
            let full = if cwd == "/" { format!("/{name}") } else { format!("{cwd}/{name}") };
            entries.push(json!({"name": name, "path": full, "git": git}));
        }
    }
    if cwd.is_empty() {
        bail!("remote directory listing produced no path:\n{}", out.trim());
    }
    entries.sort_by(|a, b| {
        a["name"].as_str().unwrap_or("").to_lowercase().cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
    });
    let parent = parent.filter(|p| p != &cwd);
    Ok(json!({"path": cwd, "parent": parent, "home": home, "entries": entries}))
}

pub async fn remote_canonical_dir(conn: &HostConn, path: &str) -> Result<String> {
    let target = if path == "~" {
        "\"$HOME\"".to_string()
    } else if let Some(rest) = path.strip_prefix("~/") {
        // Tail is user input: must be quoted; only `"$HOME"` stays outside.
        format!("\"$HOME\"/{}", sh_quote(rest))
    } else {
        sh_quote(path)
    };
    let out = conn
        .ssh_exec(&format!("cd -- {target} 2>/dev/null && pwd -P || printf 'AM_ERR\\n'"))
        .await?;
    let line = out.lines().next().unwrap_or("").trim().to_string();
    if line.is_empty() || line == "AM_ERR" {
        bail!("remote directory does not exist: {path}");
    }
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_path_is_one_quoted_path_entry() {
        assert_eq!(remote_path_prefix(""), "");
        assert_eq!(remote_path_prefix("  "), "");
        assert_eq!(remote_path_prefix("/opt/x/bin"), "export PATH='/opt/x/bin':\"$PATH\"\n");
        // A space no longer splits the export; a `;` or `$(…)` is data, not a command.
        let p = remote_path_prefix("/Users/me/my tools/bin;$(touch /tmp/pwned)");
        assert_eq!(p, "export PATH='/Users/me/my tools/bin;$(touch /tmp/pwned)':\"$PATH\"\n");
    }

    /// #241：`$HOME/.local/bin` 以前整串單引號、字面 `$HOME` 進 PATH，遠端找不到 herdr。
    #[test]
    fn remote_path_dirs_expand_only_a_leading_home() {
        assert!(remote_path_dirs("", "/home/u").is_empty());
        assert_eq!(
            remote_path_dirs("/opt/homebrew/bin:$HOME/.local/bin:${HOME}/b:~/c:~", "/home/u"),
            ["/opt/homebrew/bin", "/home/u/.local/bin", "/home/u/b", "/home/u/c", "/home/u"]
        );
        assert_eq!(remote_path_dirs("/x/$HOME/bin:$HOMEX/bin:~u/bin", "/home/u"), ["/x/$HOME/bin", "$HOMEX/bin", "~u/bin"]);
    }

    #[test]
    fn remote_path_expands_a_leading_home_and_splits_on_colon() {
        assert_eq!(remote_path_prefix("$HOME/.local/bin"), "export PATH=\"$HOME\"'/.local/bin':\"$PATH\"\n");
        assert_eq!(remote_path_prefix("${HOME}/bin"), "export PATH=\"$HOME\"'/bin':\"$PATH\"\n");
        assert_eq!(remote_path_prefix("~/bin"), "export PATH=\"$HOME\"'/bin':\"$PATH\"\n");
        assert_eq!(
            remote_path_prefix("/opt/homebrew/bin:$HOME/.local/bin"),
            "export PATH='/opt/homebrew/bin':\"$HOME\"'/.local/bin':\"$PATH\"\n"
        );
        // `$HOME` 只在項目開頭展開；其他 `$` 仍是資料。
        assert_eq!(remote_path_prefix("/x/$HOME/bin"), "export PATH='/x/$HOME/bin':\"$PATH\"\n");
        assert_eq!(remote_path_prefix("$HOMEX/bin"), "export PATH='$HOMEX/bin':\"$PATH\"\n");
        let p = remote_path_prefix("$HOME/a;$(touch /tmp/pwned)");
        assert_eq!(p, "export PATH=\"$HOME\"'/a;$(touch /tmp/pwned)':\"$PATH\"\n");
    }

    /// 兩個 daemon 實例（正式＋隔離，或兩個資料目錄不同的隔離實例）以前共用同一個
    /// `/tmp/agents-manager-<uid>`：管同一台遠端主機時會撞同一個 ctl／sock，一邊
    /// `kill_master`／explicit reconnect 會把另一邊的隧道也斷掉（issue #85）。
    #[test]
    fn short_dir_is_namespaced_by_instance() {
        let production = short_dir(None);
        let iso_a = short_dir(Some("a1b2c3d4e5f6a7b8"));
        let iso_b = short_dir(Some("00112233445566ff"));
        assert_ne!(production, iso_a, "正式實例跟隔離實例不能共用同一個目錄");
        assert_ne!(iso_a, iso_b, "兩個不同的隔離實例不能撞同一個目錄");
        assert_eq!(short_dir(Some("a1b2c3d4e5f6a7b8")), iso_a, "同一個實例重複呼叫要拿到同一條路徑");
    }

    /// `instance_slug()` 固定 16 個 hex 字元、host name 上限 32（`config::valid_host_name`）；
    /// 兩者疊到 `short_dir` 之後仍要留在 macOS AF_UNIX 的長度上限內——`start_master` 對
    /// forwarded socket 路徑超過 100 bytes 會直接 bail，不是等 ssh 自己失敗。
    #[test]
    fn a_max_length_host_name_with_an_instance_slug_still_fits_af_unix() {
        let slug = "0123456789abcdef";
        let host_name = "a".repeat(32);
        let dir = short_dir(Some(slug));
        for suffix in ["ctl", "sock"] {
            let p = dir.join(format!("{host_name}.{suffix}"));
            assert!(p.to_string_lossy().len() <= 100, "{suffix} 路徑超過 AF_UNIX 上限：{}", p.display());
        }
    }

    /// #282：對端活著但不讀 stdin（`sleep`），資料又大於 pipe buffer——寫入卡住時整個呼叫要在 timeout 內回「逾時」。
    #[tokio::test]
    async fn a_stalled_stdin_write_is_bounded_by_the_timeout() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg("sleep 30");
        let data = vec![7u8; 8 * 1024 * 1024];
        let started = std::time::Instant::now();
        let r = tokio::time::timeout(Duration::from_secs(10), run_with_stdin(cmd, &data, Duration::from_millis(500)))
            .await
            .expect("寫入卡住時 timeout 必須生效，不能整個呼叫掛住");
        assert!(matches!(r, Ok(None)), "要回逾時：{r:?}");
        // 對端 `sleep 30`：真的卡住會等滿 30 秒；上限只要低於它就分得出來，不必貼著名義時間（慢 runner 才不會翻紅）。
        assert!(started.elapsed() < Duration::from_secs(25));
    }

    /// 正常路徑：資料完整送到、輸出照拿。
    #[tokio::test]
    async fn stdin_data_reaches_the_child_and_its_output_comes_back() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg("wc -c");
        let data = vec![1u8; 300_000];
        let (out, wrote) = run_with_stdin(cmd, &data, Duration::from_secs(10)).await.unwrap().unwrap();
        wrote.unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "300000");
    }

    #[tokio::test]
    async fn sh_local_timeout_kills_the_child() {
        let marker = format!("am-sh-local-{}", crate::db::ulid());
        // `sh -c` may fork rather than exec the last command; the group kill has to reach it.
        let script = format!("sleep 30 # {marker}\nsleep 30 # {marker}");
        let t0 = std::time::Instant::now();
        let r = sh_local(&script, Duration::from_millis(300)).await.unwrap();
        assert!(r.is_none(), "expected a timeout");
        assert!(t0.elapsed() < Duration::from_secs(25), "子行程 `sleep 30`：沒被逾時砍掉才會等滿");
        tokio::time::sleep(Duration::from_millis(200)).await;
        let ps = std::process::Command::new("/bin/ps").args(["-axo", "command"]).output().unwrap();
        let alive: Vec<&str> = std::str::from_utf8(&ps.stdout).unwrap().lines().filter(|l| l.contains(&marker) && !l.contains("ps ")).collect();
        assert!(alive.is_empty(), "children survived the timeout: {alive:?}");
        let ok = sh_local("printf hi; exit 3", Duration::from_secs(5)).await.unwrap().unwrap();
        assert_eq!(ok.status.code(), Some(3));
        assert_eq!(ok.stdout, b"hi");
    }

    fn cfg() -> HostCfg {
        HostCfg {
            name: "m4p".into(),
            ssh: "m4p@100.112.229.82".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        }
    }

    /// issue #506：`apply_config` 的套用迴圈以前沒像上面的移除迴圈那樣跳過 local，
    /// 一列 `name = "local"` 就會把本機那顆 `HostConn` 換成 `HostConn::remote`——
    /// client socket 變成沒人在聽的 `<instance>/local.sock`、`is_local()` 翻成 false，
    /// 之後所有「本機走直接路徑、遠端走 ssh」的分支整批翻面。
    #[tokio::test]
    async fn a_hosts_entry_named_local_never_replaces_the_local_connection() {
        let env = crate::testing::env().await;
        let before = env.app.hosts.get(LOCAL_HOST).await.expect("本機那顆一開始就在");
        assert!(before.is_local());

        let mut bad = cfg();
        bad.name = LOCAL_HOST.into();
        let changed = env.app.hosts.apply_config(&env.app, &[bad]).await;

        assert!(!changed.contains(LOCAL_HOST), "不該把 local 當成「設定變了」的主機");
        let after = env.app.hosts.get(LOCAL_HOST).await.expect("本機那顆還要在");
        assert!(after.is_local(), "local 不可以變成 ssh 遠端");
        assert!(Arc::ptr_eq(&before, &after), "連那顆 HostConn 都不該被換掉（supervisor、master 都還在原位）");
    }

    /// #347：同名主機改設定（可能已經指到另一台）時，以主機名為鍵的觀測快取要當場作廢，
    /// 不然新連線的偵測／探測回來之前，舊機器的身分、撞限、模型清單會被當成新機器的事實。
    #[tokio::test]
    async fn replacing_a_host_forgets_what_was_observed_through_the_old_connection() {
        let env = crate::testing::env().await;
        let app = &env.app;
        let mk = |name: &str, ssh: &str| HostCfg { name: name.into(), ssh: ssh.into(), ..cfg() };
        app.hosts.insert_remote_for_test(mk("inv-347", "target-a")).await;
        app.hosts.insert_remote_for_test(mk("other-347", "target-o")).await;
        let reading = || crate::quota::Quota {
            five_hour: None, seven_day: None, fable: None, reset_credits: None, limit_hit: None, plan: None,
            updated_at: crate::db::now(), source: "test".into(), account: None, host: String::new(),
        };
        let tools = || crate::tools::HostTools {
            tools: Default::default(), identities: Default::default(), shell_identities: vec![], utc_offset_secs: None, herdr_cli: None,
            checked_at: crate::db::now(),
        };
        let shell = |host: &str| crate::api::shell::HostShell {
            host: host.into(), herdr_session: "agents-manager".into(), workspace_id: "w1".into(), tab_id: "w1:t1".into(),
            pane_id: "w1:p1".into(), cwd: "/".into(), created_at: crate::db::now(),
        };
        for h in ["inv-347", "other-347"] {
            app.tools.lock().await.insert(h.into(), tools());
            crate::quota::set(app, h, "codex", reading()).await;
            app.models_cache.lock().await.insert(format!("{h}/codex/"), (std::time::Instant::now(), json!({})));
            app.host_shells.lock().await.push(shell(h));
        }
        let cached_rows = |key: &'static str| async move {
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM quota_cache WHERE key = ?").bind(key).fetch_one(&app.db).await.unwrap()
        };
        assert_eq!(cached_rows("inv-347/codex").await, 1);

        app.hosts.replace_remote_for_test(app, mk("inv-347", "target-b")).await;

        assert!(!app.tools.lock().await.contains_key("inv-347"), "舊機器的偵測結果（身分、登入）要丟");
        assert!(!app.quotas.lock().await.contains_key("inv-347/codex"), "舊機器的額度不能擋新機器");
        assert_eq!(cached_rows("inv-347/codex").await, 0, "重啟快取列也要清，不然下次開機又種回來");
        assert!(!app.models_cache.lock().await.contains_key("inv-347/codex/"), "舊機器的模型清單要丟");
        assert!(!app.host_shells.lock().await.iter().any(|s| s.host == "inv-347"), "舊機器上的 shell 不能拿來對新機器的 pane");
        assert!(app.tools.lock().await.contains_key("other-347"), "別台不動");
        assert!(app.quotas.lock().await.contains_key("other-347/codex"));
        assert!(app.models_cache.lock().await.contains_key("other-347/codex/"));
        assert!(app.host_shells.lock().await.iter().any(|s| s.host == "other-347"));
    }

    #[test]
    fn only_forward_shaping_fields_reconnect() {
        let a = cfg();
        let mut b = cfg();
        assert!(!cfg_differs(&a, &b));
        b.ssh_port = 2222;
        assert!(cfg_differs(&a, &b));
    }

    #[tokio::test]
    async fn reconnect_does_not_return_a_stale_error() {
        let env = crate::testing::env().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut host_cfg = cfg();
        host_cfg.name = "reconnect-stale-error-test".into();
        // Never accept: ssh stays in handshake past the first 250 ms poll, exposing a leaked stale error.
        host_cfg.ssh = "127.0.0.1".into();
        host_cfg.ssh_port = port;
        host_cfg.ssh_opts = vec!["-o".into(), "ConnectTimeout=2".into()];
        let conn = HostConn::remote(host_cfg, env.app.instance());
        *conn.error.lock().await = Some("previous error".into());
        env.app.hosts.conns.lock().await.insert(conn.name.clone(), conn.clone());

        let result = tokio::time::timeout(Duration::from_secs(5), env.app.hosts.reconnect(&env.app, &conn.name))
            .await
            .expect("reconnect should not wait for the full timeout")
            .expect("host should exist");
        assert!(!result.0);
        assert_ne!(result.1.as_deref(), Some("previous error"));

        let task = conn.supervisor.lock().await.take();
        if let Some(task) = task {
            task.abort();
        }
    }
}
