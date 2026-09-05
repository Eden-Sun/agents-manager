//! Remote hosts over SSH (SPEC §11.3).
//!
//! One `HostConn` per configured `[[hosts]]` entry plus the implicit `local` one.
//! A remote host is reached by an `ssh -N -M` master that
//!   * forwards the remote herdr socket to a **short** local path
//!     (`/tmp/agents-manager-<uid>/<host>.sock`; macOS AF_UNIX caps paths at 104 bytes), and
//!   * reverse-forwards `hook_port` back to the daemon so remote hook scripts can call in.
//!
//! Everything else in the daemon then talks to `HerdrClient` exactly as it does locally.

use crate::config::{HostCfg, LOCAL_HOST};
use crate::herdr::HerdrClient;
use crate::state::App;
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// Health probe interval once a master is up (SPEC §11.3.4).
const PING_INTERVAL: Duration = Duration::from_secs(10);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// How long to wait for `ssh -M` + the forwarded socket to become pingable.
const MASTER_UP_TIMEOUT: Duration = Duration::from_secs(20);
/// One-shot `ssh <host> '<script>'` budget.
const SSH_EXEC_TIMEOUT: Duration = Duration::from_secs(30);

/// Short directory for AF_UNIX paths (control socket + forwarded herdr socket).
pub fn short_dir() -> PathBuf {
    use std::os::unix::fs::MetadataExt;
    let uid = dirs::home_dir().and_then(|h| std::fs::metadata(h).ok()).map(|m| m.uid()).unwrap_or(0);
    PathBuf::from(format!("/tmp/agents-manager-{uid}"))
}

// ---------------------------------------------------------------- shell helpers

/// POSIX single-quote one argument for embedding in a remote `sh` script.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ---------------------------------------------------------------- HostConn

pub struct HostConn {
    pub name: String,
    /// `None` for `local`.
    pub cfg: Option<HostCfg>,
    pub client: HerdrClient,
    pub connected: AtomicBool,
    pub error: Mutex<Option<String>>,
    /// Remote `$HOME`, learned during `ensure remote session`.
    pub remote_home: Mutex<Option<String>>,
    master: Mutex<Option<tokio::process::Child>>,
    supervisor: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Bumped on every explicit reconnect so a stale supervisor exits.
    generation: std::sync::atomic::AtomicU64,
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
        })
    }

    fn remote(cfg: HostCfg) -> Arc<Self> {
        let sock = short_dir().join(format!("{}.sock", cfg.name));
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
        })
    }

    pub fn is_local(&self) -> bool {
        self.cfg.is_none()
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    pub async fn error_string(&self) -> Option<String> {
        self.error.lock().await.clone()
    }

    /// The port remote hook scripts POST to (reverse-forwarded to the daemon).
    pub fn hook_port(&self, daemon_port: u16) -> u16 {
        self.cfg.as_ref().and_then(|c| c.hook_port).unwrap_or(daemon_port)
    }

    fn ctl_path(&self) -> PathBuf {
        short_dir().join(format!("{}.ctl", self.name))
    }

    /// `ssh` arguments shared by the master and every one-shot command.
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

    /// Run one `sh` script on the remote and return stdout. Errors carry stderr.
    ///
    /// The script is piped into `/bin/sh -s` rather than passed as an argument: the login
    /// shell on the far side may be zsh/fish/…, and this keeps everything POSIX sh and
    /// sidesteps a second round of shell quoting.
    pub async fn ssh_exec(&self, script: &str) -> Result<String> {
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
        let out = tokio::time::timeout(SSH_EXEC_TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| anyhow::anyhow!("ssh to {} timed out", cfg.ssh))?
            .with_context(|| format!("run ssh {}", cfg.ssh))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            bail!("ssh {} failed ({}): {}", cfg.ssh, out.status, if err.is_empty() { "no stderr".into() } else { err });
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// Same, but with the remote PATH fixed up first (SPEC §11.2 `remote_path`).
    pub async fn ssh_exec_path(&self, script: &str) -> Result<String> {
        let prefix = match self.cfg.as_ref().map(|c| c.remote_path.clone()).unwrap_or_default() {
            p if p.trim().is_empty() => String::new(),
            p => format!("export PATH={}:$PATH\n", p),
        };
        self.ssh_exec(&format!("{prefix}{script}")).await
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

    // ------------------------------------------------------------ connect

    /// SPEC §11.3.1 — make sure the remote named session's herdr server is running.
    ///
    /// On macOS the server is registered as a **launchd GUI-domain agent**
    /// (`launchctl bootstrap gui/<uid>`) instead of being `nohup`ed from the ssh shell:
    /// a process spawned from a non-interactive ssh session cannot read the user's login
    /// Keychain (`security` → errSecInteractionNotAllowed), so Claude Code started inside
    /// that herdr reports "Not logged in" even though the host *is* logged in. A GUI-domain
    /// agent runs in the console user's session where the Keychain is unlocked, survives
    /// ssh disconnects, and launchd restarts it if it dies. Falls back to `nohup` when the
    /// remote is not macOS, nobody owns /dev/console, or launchctl refuses.
    async fn ensure_remote_session(&self) -> Result<String> {
        let cfg = self.cfg.as_ref().unwrap();
        let sess = &cfg.herdr_session;
        let q = sh_quote(sess);
        let path_prefix = if cfg.remote_path.trim().is_empty() { String::new() } else { format!("{}:", cfg.remote_path.trim()) };
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

    /// SPEC §11.3.2 — bring up (or replace) the ssh master with both forwards.
    async fn start_master(&self, remote_sock: &str, daemon_port: u16) -> Result<()> {
        let cfg = self.cfg.as_ref().unwrap();
        let dir = short_dir();
        std::fs::create_dir_all(&dir).ok();
        let ctl = self.ctl_path();
        let local_sock = short_dir().join(format!("{}.sock", self.name));
        if local_sock.to_string_lossy().len() > 100 {
            bail!("forwarded socket path is too long for AF_UNIX: {}", local_sock.display());
        }

        // Tear down anything left over from a previous run.
        self.kill_master().await;

        let hook_port = self.hook_port(daemon_port);
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
            .arg(format!("{}:{}", local_sock.display(), remote_sock))
            .arg("-R")
            .arg(format!("{hook_port}:127.0.0.1:{daemon_port}"))
            .arg(&cfg.ssh);
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

        // Wait for the forwarded socket to answer a herdr ping.
        let deadline = std::time::Instant::now() + MASTER_UP_TIMEOUT;
        loop {
            if let Some(m) = self.master.lock().await.as_mut() {
                if let Ok(Some(st)) = m.try_wait() {
                    bail!("ssh master exited immediately ({st}); check credentials / forwards");
                }
            }
            if self.client.ping().await.is_ok() {
                tracing::info!(host = %self.name, socket = %local_sock.display(), hook_port, "ssh master up; herdr ping ok");
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
        let _ = std::fs::remove_file(short_dir().join(format!("{}.sock", self.name)));
    }
}

// ---------------------------------------------------------------- supervisor

/// Per-host connect / health-check / backoff loop (SPEC §11.3.4).
fn spawn_supervisor(app: Arc<App>, conn: Arc<HostConn>, generation: u64) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut backoff = BACKOFF_MIN;
        loop {
            if conn.generation.load(Ordering::SeqCst) != generation {
                return; // superseded by a reconnect / config change
            }
            let attempt = async {
                let sock = conn.ensure_remote_session().await?;
                conn.start_master(&sock, app.port).await?;
                Ok::<_, anyhow::Error>(())
            }
            .await;

            match attempt {
                Ok(()) => {
                    backoff = BACKOFF_MIN;
                    conn.connected.store(true, Ordering::SeqCst);
                    *conn.error.lock().await = None;
                    crate::state::emit_host_changed(&app, &conn).await;

                    // Reconcile + (re)build subscriptions for this host.
                    if let Err(e) = crate::reconcile::reconcile_host(&app, &conn.name).await {
                        tracing::error!(host = %conn.name, error = ?e, "reconcile after connect failed");
                    }
                    crate::events::spawn_global_for_host(app.clone(), conn.name.clone()).await;
                    crate::hookrecv::replay_host(&app, &conn.name).await;
                    // v4.0: tool detection (claude / codex / grok) runs once per connect, off-path.
                    crate::tools::spawn_detect(app.clone(), conn.name.clone());

                    // Health loop.
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

// ---------------------------------------------------------------- HostManager

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

    /// Reconcile the live connection set against `[[hosts]]`. Started supervisors are
    /// left alone when their config is unchanged.
    pub async fn apply_config(&self, app: &Arc<App>, hosts: &[HostCfg]) {
        let wanted: HashMap<String, HostCfg> = hosts.iter().map(|h| (h.name.clone(), h.clone())).collect();

        // Remove hosts that are gone.
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
            let cur = self.get(&h.name).await;
            let changed = match &cur {
                None => true,
                Some(c) => c.cfg.as_ref().map(|old| cfg_differs(old, h)).unwrap_or(true),
            };
            if !changed {
                continue;
            }
            if let Some(c) = cur {
                c.generation.fetch_add(1, Ordering::SeqCst);
                if let Some(t) = c.supervisor.lock().await.take() {
                    t.abort();
                }
                c.kill_master().await;
            }
            let conn = HostConn::remote(h.clone());
            self.conns.lock().await.insert(h.name.clone(), conn.clone());
            let gen = conn.generation.load(Ordering::SeqCst);
            let t = spawn_supervisor(app.clone(), conn.clone(), gen);
            *conn.supervisor.lock().await = Some(t);
            tracing::info!(host = %h.name, ssh = %h.ssh, "host configured");
        }
    }

    pub async fn remove(&self, app: &Arc<App>, name: &str) {
        let Some(c) = self.conns.lock().await.remove(name) else { return };
        c.generation.fetch_add(1, Ordering::SeqCst);
        if let Some(t) = c.supervisor.lock().await.take() {
            t.abort();
        }
        c.kill_master().await;
        c.connected.store(false, Ordering::SeqCst);
        app.emit("host_changed", json!({"name": name, "connected": false, "error": "removed"})).await;
        crate::state::emit_daemon_status(app).await;
        tracing::info!(host = %name, "host removed");
    }

    /// Force a reconnect and wait (bounded) for the outcome. Returns `(connected, error)`.
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
        let gen = conn.generation.load(Ordering::SeqCst);
        let t = spawn_supervisor(app.clone(), conn.clone(), gen);
        *conn.supervisor.lock().await = Some(t);
        // Give the first attempt a chance so the HTTP caller gets a real answer.
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

    /// SPEC §11.3.5 — close every master on daemon exit.
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
        || a.hook_port != b.hook_port
}

// ---------------------------------------------------------------- remote fs (§11.5)

/// `GET /api/fs/dirs?host=` for a remote host. Same JSON shape as the local branch.
pub async fn remote_list_dirs(conn: &HostConn, path: Option<&str>) -> Result<serde_json::Value> {
    let target = match path.map(str::trim).filter(|s| !s.is_empty()) {
        None => "$HOME".to_string(),
        Some(p) if p == "~" => "$HOME".to_string(),
        Some(p) if p.starts_with("~/") => format!("$HOME/{}", &p[2..]),
        Some(p) => sh_quote(p),
    };
    let script = format!(
        r#"cd -- {target} 2>/dev/null || {{ printf 'AM_ERR=no such directory\n'; exit 0; }}
printf 'AM_HOME=%s\n' "$HOME"
printf 'AM_PATH=%s\n' "$(pwd -P)"
printf 'AM_PARENT=%s\n' "$(dirname -- "$(pwd -P)")"
for d in */ ; do
  [ -d "$d" ] || continue
  n=${{d%/}}
  case "$n" in '*') continue;; esac
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
            if name.is_empty() || name.starts_with('.') {
                continue; // hidden directories are skipped, like the local branch
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

/// Validate + canonicalize a project path on a remote host.
pub async fn remote_canonical_dir(conn: &HostConn, path: &str) -> Result<String> {
    let target = if path == "~" {
        "$HOME".to_string()
    } else if let Some(rest) = path.strip_prefix("~/") {
        format!("$HOME/{rest}")
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
