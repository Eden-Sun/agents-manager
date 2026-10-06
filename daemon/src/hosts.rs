//! Remote hosts over SSH (SPEC §11.3).
//!
//! One `ssh -N -M` master per host forwards the remote herdr socket to a **short** local path
//! (macOS AF_UNIX caps paths at 104 bytes). That single `-L` is the whole tunnel: hooks report
//! via the remote herdr + a spool file read over ssh (SPEC §11.4), so no `-R` / `hook_port`.

use crate::config::{HostCfg, LOCAL_HOST};
use crate::herdr::HerdrClient;
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// 能拿到「主機表」就夠的呼叫端（git_sh、memstat、trust…）要的最小能力：不看 `App` 的其他欄位。
/// `App` 在 composition 層（`app_ports_p3`）實作；`Arc<T>`／`HostManager` 本身也算，所以既有的 `(&app, …)` 呼叫不用改。
pub trait HostsAccess: Send + Sync {
    fn hosts(&self) -> &HostManager;
}

impl<T: HostsAccess + ?Sized> HostsAccess for Arc<T> {
    fn hosts(&self) -> &HostManager {
        (**self).hosts()
    }
}

impl HostsAccess for HostManager {
    fn hosts(&self) -> &HostManager {
        self
    }
}

/// daemon 實例的 slug（隔離測試用，決定 ssh master／轉發 socket 的路徑；`None`＝正式實例）。
pub trait HostInstance: Send + Sync {
    fn instance(&self) -> Option<String>;
}

impl<T: HostInstance + ?Sized> HostInstance for Arc<T> {
    fn instance(&self) -> Option<String> {
        (**self).instance()
    }
}

pub const REMOTE_ROOT: &str = ".config/agents-manager";

/// 遠端主機上這顆實例的根目錄（`$HOME` 之下的相對路徑）。
pub fn remote_root_for(slug: Option<&str>) -> String {
    match slug {
        Some(s) => format!("{REMOTE_ROOT}/instances/{s}"),
        None => REMOTE_ROOT.to_string(),
    }
}

/// `HostManager` 的 supervisor／設定套用要叫回去的事（SPEC §11.3.4）：連上之後的整串對帳、事件推送、觀測快取清除。
/// 這些原本直接呼叫 reconcile、events、hookrecv、tools、quota… 一票 feature；現在 hosts 只認這個 trait，
/// 順序與內容由 `app_ports_p3` 的 `App` 實作保持不變。
pub trait HostHooks: HostsAccess + HostInstance + 'static {
    /// 這台是不是共用一個 herdr session 的主機（`[[hosts]] shared_session`）。
    fn is_shared_host(app: &Arc<Self>, host: &str) -> impl Future<Output = bool> + Send;
    /// 這台的連線狀態變了（連上、斷線、連不上）：推 `host_changed`。
    fn host_changed(app: &Arc<Self>, fence: &HostFence) -> impl Future<Output = ()> + Send;
    /// 本機 herdr 的 ping 結果（`reconnect("local")`）。
    fn set_local_herdr_connected(&self, ok: bool);
    /// 連上之後（重連也一樣）要做的整串事：對帳、重掛 watcher、事件訂閱、spool 重播、偵測、purge、shim／權限補版、autostart。
    fn host_connected(app: Arc<Self>, host: String) -> impl Future<Output = ()> + Send;
    /// 以主機名為鍵、描述「那台機器」的快取全部丟掉（#347）。
    fn forget_host_observations(app: &Arc<Self>, host: &str) -> impl Future<Output = ()> + Send;
    /// 主機被移除之後：快取清掉、推 `host_changed`（removed）與 `daemon_status`。
    fn host_removed(app: &Arc<Self>, host: &str) -> impl Future<Output = ()> + Send;
}

/// SPEC §11.3.4.
const PING_INTERVAL: Duration = Duration::from_secs(10);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
const MASTER_UP_TIMEOUT: Duration = Duration::from_secs(20);
const SSH_EXEC_TIMEOUT: Duration = Duration::from_secs(30);
const SSH_PUT_TIMEOUT: Duration = Duration::from_secs(120);
/// 連不出去（睡著、tailscale 斷線、封包被丟掉）時 ssh 自己放棄的秒數；沒設就等系統的 TCP 逾時（Linux 約 2 分鐘）。
const SSH_CONNECT_TIMEOUT_SECS: u64 = 15;

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
        let read_stdout = async move {
            let mut stdout = Vec::new();
            if let Some(mut o) = out {
                o.read_to_end(&mut stdout).await.ok();
            }
            stdout
        };
        let read_stderr = async move {
            let mut stderr = Vec::new();
            if let Some(mut e) = err {
                e.read_to_end(&mut stderr).await.ok();
            }
            stderr
        };
        tokio::join!(read_stdout, read_stderr)
    };
    let (status, (stdout, stderr)) = tokio::select! {
        r = async { tokio::join!(child.wait(), gather) } => (r.0.context("wait /bin/sh")?, r.1),
        _ = tokio::time::sleep(timeout) => {
            if let Some(pid) = pid {
                let _ = tokio::process::Command::new("/bin/kill").args(["-9", "--", &format!("-{pid}")]).status().await;
            }
            let _ = child.kill().await;
            return Ok(None);
        }
    };
    Ok(Some(std::process::Output { status, stdout, stderr }))
}

/// 本機跑一段 `/bin/sh -c`、只要 stdout；超過 `timeout` 回錯（`what timed out`）。
/// 不能寫成 `tokio::time::timeout(Command::output())`：逾時只是丟掉 future，子行程沒人收（沒有 `kill_on_drop`、更碰不到孫行程），
/// 卡住的 `--version`／`grok models`（`-lic` 互動 shell）每輪巡邏就多留一個。走 [`sh_local`]，逾時整個行程群組砍掉。
pub async fn sh_local_stdout(script: &str, timeout: Duration, what: &str) -> Result<String> {
    let out = sh_local(script, timeout).await?.ok_or_else(|| anyhow::anyhow!("{what} timed out"))?;
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
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
    /// 這台從什麼時候開始連不上（`db::now()` 格式）；連著時為 `None`。daemon 起來後還沒連上過＝從建立連線物件算起。
    down_since: std::sync::Mutex<Option<String>>,
    pub remote_home: Mutex<Option<String>>,
    master: Mutex<Option<tokio::process::Child>>,
    supervisor: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Bumped on every explicit reconnect so a stale supervisor exits.
    generation: std::sync::atomic::AtomicU64,
    /// Set under `HostManager::conns` before a config replacement/shutdown makes this
    /// connection unavailable for new fenced work.
    retiring: AtomicBool,
    /// Serializes authority retirement with host-side destructive RPCs without holding the
    /// manager-wide connection map while network I/O is in flight.
    authority_gate: tokio::sync::RwLock<()>,
    /// [`HostFence`] tickets handed out / the newest ticket whose result was published (issue #347).
    fence_tickets: std::sync::atomic::AtomicU64,
    fence_published: std::sync::atomic::AtomicU64,
    /// This daemon's instance slug at the time this host was (re)configured — namespaces the
    /// ctl/sock paths (issue #85). Unused for `local` (never spawns an SSH master).
    instance: Option<String>,
}

/// 每條新連線（本機、設定換掉的同名遠端）的世代從不同的區間開始：am-ports 的 `HostFence` 只有 host id＋世代，沒有連線物件，
/// 世代若每條連線都從 0 起算，換過的連線會跟舊 fence 的世代撞號，舊世代的操作就會送到新連線上。
fn first_generation() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    NEXT.fetch_add(1 << 24, Ordering::SeqCst)
}

impl HostConn {
    fn local(client: HerdrClient) -> Arc<Self> {
        Arc::new(Self {
            name: LOCAL_HOST.to_string(),
            cfg: None,
            client,
            connected: AtomicBool::new(false),
            error: Mutex::new(None),
            down_since: std::sync::Mutex::new(None),
            remote_home: Mutex::new(dirs::home_dir().map(|p| p.to_string_lossy().to_string())),
            master: Mutex::new(None),
            supervisor: Mutex::new(None),
            generation: std::sync::atomic::AtomicU64::new(first_generation()),
            retiring: AtomicBool::new(false),
            authority_gate: tokio::sync::RwLock::new(()),
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
            down_since: std::sync::Mutex::new(Some(crate::db::now())),
            remote_home: Mutex::new(None),
            master: Mutex::new(None),
            supervisor: Mutex::new(None),
            generation: std::sync::atomic::AtomicU64::new(first_generation()),
            retiring: AtomicBool::new(false),
            authority_gate: tokio::sync::RwLock::new(()),
            fence_tickets: std::sync::atomic::AtomicU64::new(0),
            fence_published: std::sync::atomic::AtomicU64::new(0),
            instance,
        })
    }

    #[cfg(test)]
    fn remote_with_client_for_test(cfg: HostCfg, instance: Option<String>, client: HerdrClient) -> Arc<Self> {
        let mut conn = Self::remote(cfg, instance);
        Arc::get_mut(&mut conn).expect("test remote connection is uniquely owned").client = client;
        conn
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

    /// 遠端主機斷線起點（API 的 `disconnected_since`）；本機與連著的主機一律 `None`。
    pub fn disconnected_since(&self) -> Option<String> {
        if self.is_local() || self.is_connected() {
            return None;
        }
        self.down_since.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn mark_up(&self) {
        *self.down_since.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// 只記第一次：重試失敗不會把斷線起點往後推。
    fn mark_down(&self) {
        self.down_since.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(crate::db::now);
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
        // 在使用者的選項之後：ssh 取第一個拿到的值，預設值不能擋住使用者為慢線路明寫的。
        v.push("-o".into());
        v.push(format!("ConnectTimeout={SSH_CONNECT_TIMEOUT_SECS}"));
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
        run_script_over_stdin(cmd, script, timeout, &cfg.ssh).await
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

    /// [`Self::ensure_remote_session`] 在遠端跑的腳本。`shared`（#709）＝這個 session 也是別顆 daemon 的：
    /// server 在跑就原樣沿用，絕不 `server stop`、不改交給 launchd。
    fn remote_session_script(cfg: &HostCfg, shared: bool) -> String {
        let sess = &cfg.herdr_session;
        let shared = u8::from(shared);
        let q = sh_quote(sess);
        let script = format!(
            r#"printf 'AM_HOME=%s\n' "$HOME"
S={q}
SHARED={shared}
SOCK="$HOME/.config/herdr/sessions/$S/herdr.sock"
LABEL="dev.agents-manager.herdr-$S"
xml_escape() {{ printf '%s' "$1" | sed 's/&/\&amp;/g; s/</\&lt;/g; s/>/\&gt;/g'; }}
running() {{ herdr session list 2>/dev/null | grep -q "^$S[[:space:]].*running"; }}
HERDR_BIN=$(command -v herdr 2>/dev/null)
CONSOLE_USER=$(stat -f %Su /dev/console 2>/dev/null || true)
MODE=nohup
if [ "$SHARED" = 1 ] && running; then
  # #709：這個 session 也是別顆 daemon 的（共用），server 在跑就一點都不碰：不 stop、不交給 launchd。
  MODE=shared
  printf 'AM_MODE=shared-running\n'
elif [ "$(uname -s)" = Darwin ] && [ -n "$HERDR_BIN" ] && [ "$CONSOLE_USER" = "$(id -un)" ] && command -v launchctl >/dev/null 2>&1; then
  MODE=launchd
elif [ "$(uname -s)" = Linux ] && command -v systemctl >/dev/null 2>&1; then
  # issue #677：Linux 裝了 herdr@.service（scripts/ops/systemd/）就交給它看管；沒有 user bus 時補 runtime dir。
  : "${{XDG_RUNTIME_DIR:=/run/user/$(id -u)}}"; export XDG_RUNTIME_DIR
  systemctl --user cat "herdr@$S.service" >/dev/null 2>&1 && MODE=systemd
fi
if [ "$MODE" = systemd ]; then
  if running; then
    # 已經在跑（不管是誰起的）就不動：停它會收掉每個 pane 裡的 agent。
    printf 'AM_MODE=systemd-existing\n'
  elif systemctl --user start "herdr@$S.service" 2>/tmp/am-systemctl.err; then
    printf 'AM_MODE=systemd\n'
  else
    printf 'AM_MODE=nohup-fallback systemctl: %s\n' "$(tr '\n' ' ' </tmp/am-systemctl.err)"
    MODE=nohup
  fi
fi
if [ "$MODE" = launchd ]; then
  PL="$HOME/Library/LaunchAgents/$LABEL.plist"
  umask 077
  mkdir -p "$HOME/Library/LaunchAgents"
  if launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1; then
    printf 'AM_MODE=launchd-existing\n'
  else
    XML_LABEL=$(xml_escape "$LABEL")
    XML_HERDR_BIN=$(xml_escape "$HERDR_BIN")
    XML_PATH=$(xml_escape "$PATH")
    cat > "$PL" <<AM_PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>$XML_LABEL</string>
  <key>ProgramArguments</key><array><string>$XML_HERDR_BIN</string><string>--session</string><string>$S</string><string>server</string></array>
  <key>EnvironmentVariables</key><dict><key>PATH</key><string>$XML_PATH</string></dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardOutPath</key><string>/tmp/herdr-$S.log</string>
  <key>StandardErrorPath</key><string>/tmp/herdr-$S.log</string>
</dict></plist>
AM_PLIST
    if chmod 600 "$PL" 2>/dev/null; then
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
    else
      printf 'AM_MODE=nohup-fallback launchd plist permissions\n'
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
        script
    }

    /// SPEC §11.3.1. On macOS herdr runs as a launchd GUI-domain agent, not `nohup` from ssh:
    /// an ssh-spawned process can't read the login Keychain, so Claude Code inside reports
    /// "Not logged in". Falls back to `nohup` when not macOS / no console owner / launchctl refuses.
    async fn ensure_remote_session(&self, shared: bool) -> Result<String> {
        let cfg = self.cfg.as_ref().unwrap();
        let sess = &cfg.herdr_session;
        let script = Self::remote_session_script(cfg, shared);
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

/// `ssh … /bin/sh -s` 的執行：script 走 stdin，**寫入與等待共用同一個 timeout**（#282 的同一個洞，見 [`run_with_stdin`]）。
/// 以前 `write_all` 在 timeout 外面：script 大於 pipe buffer、遠端不讀（TCP 收下但不回話）時寫入永遠 pending，逾時根本不啟動。
/// 寫入失敗（`sh -s` 先結束）不算錯，看 exit status／stderr 才是原因。
async fn run_script_over_stdin(cmd: tokio::process::Command, script: &str, timeout: Duration, target: &str) -> Result<String> {
    let (out, _wrote) = run_with_stdin(cmd, script.as_bytes(), timeout)
        .await
        .with_context(|| format!("run ssh {target}"))?
        .ok_or_else(|| anyhow::anyhow!("ssh to {} timed out after {}s", target, timeout.as_secs()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        bail!("ssh {} failed ({}): {}", target, out.status, if err.is_empty() { "no stderr".into() } else { err });
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
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
fn spawn_supervisor<H: HostHooks>(app: Arc<H>, conn: Arc<HostConn>, generation: u64) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let Some(fence) = app.hosts().fence_for_generation(&conn, generation).await else { return };
        let mut backoff = BACKOFF_MIN;
        loop {
            if conn.generation.load(Ordering::SeqCst) != generation {
                return; // superseded by a reconnect / config change
            }
            let attempt = async {
                let sock = conn.ensure_remote_session(H::is_shared_host(&app, &conn.name).await).await?;
                conn.start_master(&sock).await?;
                Ok::<_, anyhow::Error>(())
            }
            .await;

            match attempt {
                Ok(()) => {
                    backoff = BACKOFF_MIN;
                    conn.mark_up();
                    conn.connected.store(true, Ordering::SeqCst);
                    *conn.error.lock().await = None;
                    H::host_changed(&app, &fence).await;
                    H::host_connected(app.clone(), conn.name.clone()).await;

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
                    conn.mark_down();
                    conn.connected.store(false, Ordering::SeqCst);
                    conn.kill_master().await;
                    H::host_changed(&app, &fence).await;
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    tracing::warn!(host = %conn.name, error = %msg, "host connect failed");
                    if conn.generation.load(Ordering::SeqCst) != generation {
                        return;
                    }
                    conn.mark_down();
                    conn.connected.store(false, Ordering::SeqCst);
                    *conn.error.lock().await = Some(msg);
                    H::host_changed(&app, &fence).await;
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

/// Weak cache identity for one fenced host generation. Keeping this in a process-wide cache must not pin a retired
/// [`HostConn`] (its supervisor and client resources); the weak pointer still distinguishes replaced connections.
#[derive(Clone)]
pub(crate) struct HostAuthorityKey {
    conn: std::sync::Weak<HostConn>,
    generation: u64,
}

impl HostAuthorityKey {
    pub(crate) fn matches(&self, fence: &HostFence) -> bool {
        self.generation == fence.generation && self.conn.ptr_eq(&Arc::downgrade(&fence.conn))
    }
}

impl HostFence {
    pub fn conn(&self) -> &Arc<HostConn> {
        &self.conn
    }

    /// Tickets from the same connection generation represent the same host authority.
    pub(crate) fn same_authority(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.conn, &other.conn) && self.generation == other.generation
    }

    pub(crate) fn authority_key(&self) -> HostAuthorityKey {
        HostAuthorityKey { conn: Arc::downgrade(&self.conn), generation: self.generation }
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

/// Resolve the home directory belonging to this captured host authority. A remote read failure
/// is unknown authority and must never borrow this daemon's HOME (#595, #616, #618).
pub(crate) async fn home_for_fence(fence: &HostFence) -> Result<String> {
    let conn = fence.conn();
    if conn.is_local() {
        return Ok(dirs::home_dir().map(|p| p.display().to_string()).unwrap_or_else(|| "/tmp".into()));
    }
    conn.home().await.with_context(|| format!("remote HOME for host `{}` is unreadable", conn.name))
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

    /// 窄介面（am-ports `HostFence`＝host id＋世代）用：這台目前的連線世代；不領 ticket。沒這台或正在退場＝`None`。
    pub(crate) async fn current_generation(&self, name: &str) -> Option<u64> {
        let conn = self.get(name).await?;
        (!conn.retiring.load(Ordering::SeqCst)).then(|| conn.generation.load(Ordering::SeqCst))
    }

    /// 世代還對得上才回連線：重連／改設定之後舊世代拿到 `None`，操作不會被送到換過的那條連線上。
    pub(crate) async fn conn_at_generation(&self, name: &str, generation: u64) -> Option<Arc<HostConn>> {
        let conn = self.get(name).await?;
        (!conn.retiring.load(Ordering::SeqCst) && conn.generation.load(Ordering::SeqCst) == generation).then_some(conn)
    }

    /// Capture the current authority for `name` (see [`HostFence`]); `None` = no such host.
    pub async fn fence(&self, name: &str) -> Option<HostFence> {
        let conn = self.get(name).await?;
        let generation = conn.generation.load(Ordering::SeqCst);
        self.fence_for_generation(&conn, generation).await
    }

    /// Bind a supervisor to its original connection generation. A stale task must not look up a
    /// fresh fence by name after replacement, or it could publish old state under the new host.
    pub(crate) async fn fence_for_generation(&self, conn: &Arc<HostConn>, generation: u64) -> Option<HostFence> {
        let current = self.conns.lock().await.get(&conn.name).is_some_and(|c| Arc::ptr_eq(c, conn));
        if !current || conn.retiring.load(Ordering::SeqCst) || conn.generation.load(Ordering::SeqCst) != generation {
            return None;
        }
        let ticket = conn.fence_tickets.fetch_add(1, Ordering::SeqCst) + 1;
        Some(HostFence { conn: conn.clone(), generation, ticket })
    }

    pub async fn is_current(&self, f: &HostFence) -> bool {
        let same = self.conns.lock().await.get(&f.conn.name).is_some_and(|c| Arc::ptr_eq(c, &f.conn));
        same && !f.conn.retiring.load(Ordering::SeqCst) && f.conn.generation.load(Ordering::SeqCst) == f.generation
    }

    /// Run one host-side operation while its captured connection remains the configured authority.
    /// Config replacement/removal and reconnect generation changes take the write side of this
    /// connection's gate, so they linearize before this operation (then it is rejected) or after
    /// it (then the whole RPC stays on the original connection).
    pub async fn run_if_current<F: Future>(&self, fence: &HostFence, operation: F) -> Option<F::Output> {
        let _authority = fence.conn.authority_gate.read().await;
        let current = {
            let conns = self.conns.lock().await;
            conns.get(&fence.conn.name).is_some_and(|c| Arc::ptr_eq(c, &fence.conn))
                && !fence.conn.retiring.load(Ordering::SeqCst)
                && fence.conn.generation.load(Ordering::SeqCst) == fence.generation
        };
        if !current {
            return None;
        }
        Some(operation.await)
    }

    /// Test seam: register a remote host without starting a supervisor (a reconfigure replaces it, like `apply_config`).
    #[cfg(test)]
    pub(crate) async fn insert_remote_for_test(&self, cfg: HostCfg) -> Arc<HostConn> {
        let conn = HostConn::remote(cfg, None);
        self.insert_remote_conn_for_test(conn).await
    }

    /// Test seam: like [`Self::insert_remote_for_test`], with a caller-provided fake Herdr socket.
    #[cfg(test)]
    pub(crate) async fn insert_remote_with_client_for_test(&self, cfg: HostCfg, client: HerdrClient) -> Arc<HostConn> {
        let conn = HostConn::remote_with_client_for_test(cfg, None, client);
        self.insert_remote_conn_for_test(conn).await
    }

    #[cfg(test)]
    async fn insert_remote_conn_for_test(&self, conn: Arc<HostConn>) -> Arc<HostConn> {
        if let Some(old) = self.get(&conn.name).await {
            let _authority = old.authority_gate.write().await;
            self.conns.lock().await.insert(conn.name.clone(), conn.clone());
        } else {
            self.conns.lock().await.insert(conn.name.clone(), conn.clone());
        }
        conn
    }

    /// Test seam: [`Self::apply_config`] 換掉同名連線的那一步（含快取失效），只是不起 supervisor、不真的連 ssh。
    #[cfg(test)]
    pub(crate) async fn replace_remote_for_test<H: HostHooks>(&self, app: &Arc<H>, cfg: HostCfg) -> Arc<HostConn> {
        let conn = HostConn::remote(cfg, app.instance());
        if let Some(old) = self.get(&conn.name).await {
            let _authority = old.authority_gate.write().await;
            self.install_conn(app, conn.clone()).await;
        } else {
            self.install_conn(app, conn.clone()).await;
        }
        conn
    }

    /// Test seam: repoint a named remote while keeping both generations on distinct fake sockets.
    #[cfg(test)]
    pub(crate) async fn replace_remote_with_client_for_test<H: HostHooks>(
        &self,
        app: &Arc<H>,
        cfg: HostCfg,
        client: HerdrClient,
    ) -> Arc<HostConn> {
        let conn = HostConn::remote_with_client_for_test(cfg, app.instance(), client);
        if let Some(old) = self.get(&conn.name).await {
            let _authority = old.authority_gate.write().await;
            self.install_conn(app, conn.clone()).await;
        } else {
            self.install_conn(app, conn.clone()).await;
        }
        conn
    }

    /// 把 `conn` 放上去當這個名字的權威。原本就有同名連線（設定改了、可能改指到另一台）時，舊連線量到的東西
    /// 一律作廢（#347）：先換連線、再清快取——順序反過來的話，清完到換上之間發布的舊觀測會通過權威檢查留下來。
    async fn install_conn<H: HostHooks>(&self, app: &Arc<H>, conn: Arc<HostConn>) {
        let replaced = self.conns.lock().await.insert(conn.name.clone(), conn.clone()).is_some();
        if replaced {
            H::forget_host_observations(app, &conn.name).await;
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
    pub async fn apply_config<H: HostHooks>(&self, app: &Arc<H>, hosts: &[HostCfg]) -> HashSet<String> {
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
                let _authority = c.authority_gate.write().await;
                {
                    let conns = self.conns.lock().await;
                    if conns.get(&c.name).is_some_and(|current| Arc::ptr_eq(current, &c)) {
                        c.retiring.store(true, Ordering::SeqCst);
                        c.generation.fetch_add(1, Ordering::SeqCst);
                    }
                }
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

    pub async fn remove<H: HostHooks>(&self, app: &Arc<H>, name: &str) {
        let Some(c) = self.get(name).await else { return };
        let _authority = c.authority_gate.write().await;
        let Some(c) = ({
            let mut conns = self.conns.lock().await;
            if !conns.get(name).is_some_and(|current| Arc::ptr_eq(current, &c)) {
                None
            } else {
                let removed = conns.remove(name);
                if let Some(c) = &removed {
                    c.retiring.store(true, Ordering::SeqCst);
                    c.generation.fetch_add(1, Ordering::SeqCst);
                }
                removed
            }
        }) else {
            return;
        };
        if let Some(t) = c.supervisor.lock().await.take() {
            t.abort();
        }
        c.kill_master().await;
        c.connected.store(false, Ordering::SeqCst);
        // Stale `<gone>/claude` quota rows would read as local downstream — SPEC §14.
        H::host_removed(app, name).await;
        tracing::info!(host = %name, "host removed");
    }

    /// Returns `(connected, error)`.
    pub async fn reconnect<H: HostHooks>(&self, app: &Arc<H>, name: &str) -> Option<(bool, Option<String>)> {
        let fence = self.fence(name).await?;
        let conn = fence.conn().clone();
        if conn.is_local() {
            let ok = conn.client.ping().await.is_ok();
            conn.connected.store(ok, Ordering::SeqCst);
            app.set_local_herdr_connected(ok);
            *conn.error.lock().await = if ok { None } else { Some("local herdr ping failed".into()) };
            H::host_changed(&app, &fence).await;
            return Some((ok, conn.error_string().await));
        }
        let _authority = conn.authority_gate.write().await;
        {
            let conns = self.conns.lock().await;
            if !conns.get(name).is_some_and(|current| Arc::ptr_eq(current, &conn)) || conn.retiring.load(Ordering::SeqCst) {
                return None;
            }
            conn.generation.fetch_add(1, Ordering::SeqCst);
        }
        if let Some(t) = conn.supervisor.lock().await.take() {
            t.abort();
        }
        conn.kill_master().await;
        conn.mark_down();
        conn.connected.store(false, Ordering::SeqCst);
        *conn.error.lock().await = None;
        let gen = conn.generation.load(Ordering::SeqCst);
        let t = spawn_supervisor(app.clone(), conn.clone(), gen);
        *conn.supervisor.lock().await = Some(t);
        drop(_authority);
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
            let _authority = c.authority_gate.write().await;
            {
                let conns = self.conns.lock().await;
                if conns.get(&c.name).is_some_and(|current| Arc::ptr_eq(current, &c)) {
                    c.retiring.store(true, Ordering::SeqCst);
                    c.generation.fetch_add(1, Ordering::SeqCst);
                }
            }
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

/// 一次最多列幾個子目錄（本機與遠端同一個數字）；超過就截斷並回 `truncated:true`。
pub(crate) const DIR_LIST_LIMIT: usize = 2000;

/// 遠端目錄列舉要跑的 sh（純字串，可以拿到本機直接跑來驗）。

/// 輸出靠 `AM_*` 開頭的行解析，所以：目前路徑或目錄名字含控制字元（換行、tab、CR…）的一律不列——否則一個叫
/// `a\nAM_PATH=/etc` 的目錄就能偽造「目前路徑」；憑證與金鑰目錄（`~/.ssh`、`~/.gnupg`、`~/.aws`、`~/.kube`、daemon 與各 CLI 的設定目錄）
/// 不給進；子目錄最多 [`DIR_LIST_LIMIT`] 個。
pub(crate) fn dir_list_script(path: Option<&str>, hidden: bool) -> String {
    let target = match path.map(str::trim).filter(|s| !s.is_empty()) {
        None => "\"$HOME\"".to_string(),
        Some(p) if p == "~" => "\"$HOME\"".to_string(),
        // Tail is user input: must be quoted; only `"$HOME"` stays outside.
        Some(p) if p.starts_with("~/") => format!("\"$HOME\"/{}", sh_quote(&p[2..])),
        Some(p) => sh_quote(p),
    };
    let globs = if hidden { "*/ .*/" } else { "*/" };
    format!(
        r#"cd -- {target} 2>/dev/null || {{ printf 'AM_ERR=no such directory\n'; exit 0; }}
P=$(pwd -P)
case "$P" in *[[:cntrl:]]*) printf 'AM_ERR=directory name has control characters\n'; exit 0;; esac
H=$(cd -- "$HOME" 2>/dev/null && pwd -P || printf '%s' "$HOME")
for S in "$H"/.ssh "$H"/.gnupg "$H"/.aws "$H"/.kube "$H"/.config/agents-manager "$H"/.claude "$H"/.claude-* "$H"/.codex "$H"/.grok; do
  [ -d "$S" ] || continue
  R=$(cd -- "$S" 2>/dev/null && pwd -P) || continue
  case "$P" in "$R"|"$R"/*) printf 'AM_ERR=forbidden directory\n'; exit 0;; esac
done
printf 'AM_HOME=%s\n' "$HOME"
printf 'AM_PATH=%s\n' "$P"
printf 'AM_PARENT=%s\n' "$(dirname -- "$P")"
count=0
for d in {globs} ; do
  [ -d "$d" ] || continue
  n=${{d%/}}
  case "$n" in '*'|'.*'|.|..) continue;; esac
  case "$n" in *[[:cntrl:]]*) continue;; esac
  count=$((count+1))
  if [ "$count" -gt {DIR_LIST_LIMIT} ]; then printf 'AM_TRUNC=1\n'; break; fi
  if [ -e "$n/.git" ]; then g=1; else g=0; fi
  printf 'AM_D\t%s\t%s\n' "$n" "$g"
done
"#
    )
}

/// [`dir_list_script`] 的輸出 → 與本機同形的 JSON。
pub(crate) fn parse_dir_listing(out: &str, hidden: bool) -> Result<serde_json::Value> {
    let mut home = String::new();
    let mut cwd = String::new();
    let mut parent: Option<String> = None;
    let mut entries: Vec<serde_json::Value> = Vec::new();
    let mut truncated = false;
    for line in out.lines() {
        if line == "AM_TRUNC=1" {
            truncated = true;
        } else if let Some(e) = line.strip_prefix("AM_ERR=") {
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
            if name.is_empty() || name.chars().any(char::is_control) || (name.starts_with('.') && !hidden) {
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
    Ok(json!({"path": cwd, "parent": parent, "home": home, "entries": entries, "truncated": truncated}))
}

/// `GET /api/fs/dirs?host=` (§11.5); same JSON shape as the local branch.
pub async fn remote_list_dirs(conn: &HostConn, path: Option<&str>, hidden: bool) -> Result<serde_json::Value> {
    let out = conn.ssh_exec(&dir_list_script(path, hidden)).await?;
    parse_dir_listing(&out, hidden)
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

    #[tokio::test]
    async fn a_remote_autostart_pass_waits_for_the_api_readiness_transition() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let host = "remote-autostart-readiness-test";
        app.set_startup_ready(false);
        let pass_app = app.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let pass = tokio::spawn(async move {
            let _ = started_tx.send(());
            pass_app.wait_until_startup_ready().await;
            crate::reconcile::autostart_after_reconcile(&pass_app, host, true).await
        });

        started_rx.await.unwrap();
        assert!(!pass.is_finished(), "remote autostart must wait while API-facing startup work is unavailable");
        assert!(!app.autostart_hosts.lock().unwrap().contains_key(host), "pass must not claim before ready");

        app.set_startup_ready(true);
        assert!(pass.await.unwrap(), "once ready, a successful reconcile should run the host pass");
        assert_eq!(app.autostart_hosts.lock().unwrap().get(host), Some(&crate::state::AutostartHostStatus::Done));
    }

    /// A captured old host must not publish its state or combine it with the replacement's tools cache.
    #[tokio::test]
    async fn a_repointed_host_cannot_publish_the_old_connection_snapshot() {
        let env = crate::testing::env().await;
        let host = "host-changed-fence-test";
        let cfg = |ssh: &str| HostCfg {
            shared_session: false,
            name: host.into(),
            ssh: ssh.into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "test".into(),
            remote_path: String::new(),
        };
        let old = env.app.hosts.insert_remote_for_test(cfg("target-a")).await;
        *old.error.lock().await = Some("server A error".into());
        let old_fence = env.app.hosts.fence(host).await.unwrap();
        let app = env.app.clone();
        let stale_fence = old_fence.clone();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (publish_tx, publish_rx) = tokio::sync::oneshot::channel();
        let stale_emit = tokio::spawn(async move {
            let _ = ready_tx.send(());
            let _ = publish_rx.await;
            crate::state::emit_host_changed(&app, &stale_fence).await;
        });
        ready_rx.await.unwrap();

        let new = env.app.hosts.replace_remote_for_test(&env.app, cfg("target-b")).await;
        new.connected.store(true, Ordering::SeqCst);
        env.app.tools.lock().await.insert(
            host.into(),
            crate::tools::HostTools {
                tools: Default::default(),
                identities: Default::default(),
                shell_identities: vec![],
                utc_offset_secs: None,
                herdr_cli: Some("herdr 0.9.7".into()),
                checked_at: crate::db::now(),
            },
        );
        let mut events = env.app.subscribe();

        // Replacement wins before publish; A's connected/error must not be paired with B's tools.
        publish_tx.send(()).unwrap();
        stale_emit.await.unwrap();
        assert!(
            matches!(events.try_recv(), Err(tokio::sync::broadcast::error::TryRecvError::Empty)),
            "the superseded A connection must publish no host_changed or daemon_status event"
        );

        let new_fence = env.app.hosts.fence(host).await.unwrap();
        crate::state::emit_host_changed(&env.app, &new_fence).await;
        let current = events.try_recv().unwrap();
        assert_eq!(current.kind, "host_changed");
        assert_eq!(current.data["name"], host);
        assert_eq!(current.data["connected"], true);
        assert_eq!(current.data["error"], serde_json::Value::Null);
        assert_eq!(current.data["herdr"]["cli_version"], "0.9.7");
    }

    /// 主機移除／換連線時 `forget_host_observations` 丟掉 `<host>/…` 的額度，但沒告訴前端：額度條在下一次 5 分鐘輪詢以前
    /// 還顯示著那台機器的數字（換連線時甚至是另一台機器的）。每個被丟掉的 key 要發一則 `quota_updated`（`quota:null`），
    /// 跟 `identity_kind::cleanup_host` 同一個形狀。
    #[tokio::test]
    async fn forgetting_a_hosts_quota_tells_the_clients() {
        let env = crate::testing::env().await;
        let host = "quota-forget-test";
        let key = format!("{host}/claude");
        env.app.quotas.lock().await.insert(
            key.clone(),
            crate::quota::Quota {
                five_hour: None,
                seven_day: None,
                fable: None,
                reset_credits: None,
                limit_hit: None,
                plan: None,
                updated_at: crate::db::now(),
                source: "test".into(),
                account: None,
                host: host.into(),
            },
        );
        let mut events = env.app.subscribe();
        <crate::state::App as HostHooks>::forget_host_observations(&env.app, host).await;
        assert!(env.app.quotas.lock().await.get(&key).is_none(), "前提：額度已丟掉");
        let mut seen = None;
        while let Ok(ev) = events.try_recv() {
            if ev.kind == "quota_updated" && ev.data["kind"] == key.as_str() {
                seen = Some(ev);
            }
        }
        let ev = seen.expect("丟掉的額度 key 要發 quota_updated");
        assert!(ev.data["quota"].is_null(), "{}", ev.data);
        assert_eq!(ev.data["host"], host);
    }

    #[tokio::test]
    async fn xreview_forgetting_a_host_also_drops_its_baseline_and_stale_shim_incident() {
        let env = crate::testing::env().await;
        let host = "forget-observations-test";
        env.app.host_baseline.lock().await.insert(
            host.into(),
            crate::host_baseline::BaselineReport {
                os: Some("Linux".into()),
                issues: Some(vec![]),
                checked_at: crate::db::now(),
                failed_at: None,
                error: None,
                stale: false,
            },
        );
        env.app
            .remote_shim_stale
            .lock()
            .await
            .insert(host.into(), "old host shim could not be refreshed".into());
        env.app
            .remote_shim_stale
            .lock()
            .await
            .insert("other-host".into(), "keep this other host".into());

        <crate::state::App as HostHooks>::forget_host_observations(&env.app, host).await;

        assert!(
            !env.app.host_baseline.lock().await.contains_key(host),
            "a same-name replacement must not inherit another machine's baseline"
        );
        let stale = env.app.remote_shim_stale.lock().await;
        assert!(
            !stale.contains_key(host),
            "removed hosts must not keep reporting stale shim incidents"
        );
        assert_eq!(
            stale.get("other-host").map(String::as_str),
            Some("keep this other host")
        );
    }

    #[tokio::test]
    async fn a_fenced_host_operation_holds_its_authority_until_completion() {
        let env = crate::testing::env().await;
        let host = "authority-gate-test";
        let cfg = HostCfg {
            shared_session: false,
            name: host.into(),
            ssh: "target-a".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "test".into(),
            remote_path: String::new(),
        };
        env.app.hosts.insert_remote_for_test(cfg).await;
        let fence = env.app.hosts.fence(host).await.unwrap();
        let app = env.app.clone();
        let fence_for_task = fence.clone();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            app.hosts
                .run_if_current(&fence_for_task, async move {
                    let _ = entered_tx.send(());
                    let _ = release_rx.await;
                    "original authority"
                })
                .await
        });
        entered_rx.await.unwrap();
        assert!(fence.conn.authority_gate.try_write().is_err(), "repoint/reconnect must wait while the RPC is in flight");
        release_tx.send(()).unwrap();
        assert_eq!(task.await.unwrap(), Some("original authority"));
        assert!(fence.conn.authority_gate.try_write().is_ok(), "the authority gate releases after the operation");
    }

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

    /// ssh_exec 的 script 走 stdin：遠端不讀、script 又大於 pipe buffer 時，寫入卡住也要在 timeout 內回「逾時」（同 #282）。
    #[tokio::test]
    async fn a_stalled_script_write_is_bounded_by_the_exec_timeout() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg("sleep 30");
        let script = format!("# {}\n", "x".repeat(8 * 1024 * 1024));
        let started = std::time::Instant::now();
        let r = tokio::time::timeout(Duration::from_secs(10), run_script_over_stdin(cmd, &script, Duration::from_millis(500), "h"))
            .await
            .expect("寫入卡住時 timeout 必須生效，不能整個呼叫掛住");
        let e = r.expect_err("要回逾時");
        assert!(e.to_string().contains("timed out"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(25));
    }

    /// 正常路徑：script 完整送到、stdout 照拿；非 0 退出帶 stderr。
    #[tokio::test]
    async fn the_script_reaches_sh_and_failures_carry_stderr() {
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-s");
        assert_eq!(run_script_over_stdin(cmd, "printf hi", Duration::from_secs(10), "h").await.unwrap(), "hi");
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-s");
        let e = run_script_over_stdin(cmd, "echo boom >&2; exit 3", Duration::from_secs(10), "h").await.unwrap_err();
        assert!(e.to_string().contains("boom"), "{e}");
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

    /// 本機探測逾時：子行程（含孫行程）要被收掉，不能留著等下一輪再多留一個。
    #[tokio::test]
    async fn a_timed_out_local_probe_leaves_nothing_running() {
        let marker = format!("am-local-probe-{}", crate::db::ulid());
        let script = format!("sleep 30 # {marker}\nsleep 30 # {marker}");
        let t0 = std::time::Instant::now();
        let e = sh_local_stdout(&script, Duration::from_millis(300), "probe").await.unwrap_err();
        assert!(e.to_string().contains("timed out"), "{e}");
        assert!(t0.elapsed() < Duration::from_secs(25));
        tokio::time::sleep(Duration::from_millis(200)).await;
        let ps = std::process::Command::new("/bin/ps").args(["-axo", "command"]).output().unwrap();
        let alive: Vec<&str> = std::str::from_utf8(&ps.stdout).unwrap().lines().filter(|l| l.contains(&marker) && !l.contains("ps ")).collect();
        assert!(alive.is_empty(), "children survived the timeout: {alive:?}");
    }

    #[tokio::test]
    async fn a_local_probe_returns_its_stdout() {
        assert_eq!(sh_local_stdout("printf ok", Duration::from_secs(10), "probe").await.unwrap(), "ok");
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

    #[tokio::test]
    async fn macos_local_sh_local_drains_stdout_and_stderr_concurrently() {
        let output = sh_local(
            r"head -c 200000 /dev/zero | tr '\0' x >&2; printf done",
            Duration::from_secs(3),
        )
        .await
        .unwrap()
        .expect("large stderr output must not deadlock until timeout");

        assert!(output.status.success());
        assert_eq!(output.stdout, b"done");
        assert_eq!(output.stderr.len(), 200_000);
        assert!(output.stderr.iter().all(|byte| *byte == b'x'));
    }

    fn cfg() -> HostCfg {
        HostCfg {
            shared_session: false,
            name: "m4p".into(),
            ssh: "m4p@100.112.229.82".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        }
    }

    /// 主機睡著／tailscale 斷線時 ssh 連不出去：沒有 ConnectTimeout 就只能等系統的 TCP 逾時（Linux 約 2 分鐘），
    /// 所以每條 ssh 都帶 ConnectTimeout（跟 remote_cargo 的 SSH_LIVENESS_OPTS 同一個數字）。放在使用者的 `ssh_opts` **後面**：
    /// ssh 取第一個拿到的值，使用者為慢線路明寫的 `ConnectTimeout=60` 才蓋得過去。
    #[test]
    fn every_ssh_leg_has_a_connect_timeout_the_users_opts_can_override() {
        let mut c = cfg();
        c.ssh_opts = vec!["-o".into(), "ConnectTimeout=60".into()];
        let args = HostConn::remote(c, None).ssh_args();
        let at = |needle: &str| args.iter().position(|a| a.starts_with(needle));
        let ours = at("ConnectTimeout=15").expect("沒有預設的 ConnectTimeout");
        let theirs = at("ConnectTimeout=60").expect("使用者的選項不見了");
        assert!(theirs < ours, "使用者的要排在前面才蓋得過預設：{args:?}");
        assert!(HostConn::remote(cfg(), None).ssh_args().iter().any(|a| a == "ConnectTimeout=15"));
    }

    /// 離線警示條的「離線多久」：斷線起點只在第一次斷時記下，重試失敗不往後推；連上就清掉。
    #[test]
    fn disconnected_since_keeps_the_first_drop_and_clears_on_connect() {
        let conn = HostConn::remote(cfg(), None);
        let first = conn.disconnected_since().expect("還沒連上過就算離線");
        conn.mark_down();
        assert_eq!(conn.disconnected_since().as_deref(), Some(first.as_str()));
        conn.mark_up();
        conn.connected.store(true, Ordering::SeqCst);
        assert_eq!(conn.disconnected_since(), None);
        conn.mark_down();
        conn.connected.store(false, Ordering::SeqCst);
        assert!(conn.disconnected_since().is_some_and(|t| t >= first));
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
            if h == "inv-347" {
                crate::login_assist::reserve(app, h, "cc9").unwrap().register("w1:p1", &shell(h).created_at);
            }
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
        assert!(!crate::login_assist::is_registered(app, "inv-347", "w1:p1"), "舊機器上的登入 pane 也不能阻擋或指向新機器");
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

#[cfg(test)]
mod shared_session_script_tests {
    //! #709：連上共用 session 的主機時，對方的 herdr server 在跑就一點都不碰（絕不 `server stop`）。
    use super::*;

    fn fake(bin: &std::path::Path, name: &str, body: &str) {
        crate::testing::write_exec(bin.join(name), format!("#!/bin/sh\n{body}\n"));
    }

    fn generated_launchd_plist() -> (String, String, u32, u32, u32) {
        use std::os::unix::fs::MetadataExt;

        let root = std::path::PathBuf::from(format!("/tmp/am-rs-{}", &crate::db::ulid()[18..]));
        let home = root.join("home & files");
        let bin = home.join("bin & tools");
        let (log, capture) = (root.join("log"), root.join("captured.plist"));
        std::fs::create_dir_all(&bin).unwrap();
        let sock_dir = home.join(".config/herdr/sessions/test");
        std::fs::create_dir_all(&sock_dir).unwrap();
        let _sock = std::os::unix::net::UnixListener::bind(sock_dir.join("herdr.sock")).unwrap();
        fake(&bin, "herdr", r#"echo "herdr $*" >> "$AM_LOG"; [ "$1" = session ] && [ "$2" = list ] && echo "test running"; exit 0"#);
        fake(&bin, "uname", "echo Darwin");
        fake(&bin, "stat", "id -un");
        fake(
            &bin,
            "launchctl",
            r#"echo "launchctl $*" >> "$AM_LOG"; [ "$1" = print ] && exit 1; [ "$1" = bootstrap ] && cp "$3" "$AM_CAPTURE"; exit 0"#,
        );
        fake(&bin, "sleep", "exit 0");
        let cfg = HostCfg {
            name: "sh1".into(),
            ssh: "sh1.invalid".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "test".into(),
            remote_path: String::new(),
            shared_session: false,
        };
        let command = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("umask 000; {}", HostConn::remote_session_script(&cfg, false)))
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("HOME", &home)
            .env("AM_LOG", &log)
            .env("AM_CAPTURE", &capture)
            .output()
            .unwrap();
        assert!(command.status.success(), "{}", String::from_utf8_lossy(&command.stderr));
        let plist = std::fs::read_to_string(&capture).unwrap();
        let meta = std::fs::metadata(&capture).unwrap();
        let dir_mode = std::fs::metadata(home.join("Library/LaunchAgents")).unwrap().mode() & 0o777;
        let bin_path = bin.join("herdr").to_string_lossy().into_owned();
        let mode = meta.mode() & 0o777;
        let uid = meta.uid();
        std::fs::remove_dir_all(&root).ok();
        (plist, bin_path, mode, dir_mode, uid)
    }

    /// 假的 macOS：herdr 說 session 在跑（nohup 起的，launchd 沒有它）——正是以前會 `server stop` 交給 launchd 的形狀。
    fn run(shared: bool) -> (String, String) {
        let root = std::path::PathBuf::from(format!("/tmp/am-rs-{}", &crate::db::ulid()[18..]));
        let (bin, home, log) = (root.join("bin"), root.join("home"), root.join("log"));
        std::fs::create_dir_all(&bin).unwrap();
        let sock_dir = home.join(".config/herdr/sessions/test");
        std::fs::create_dir_all(&sock_dir).unwrap();
        let _sock = std::os::unix::net::UnixListener::bind(sock_dir.join("herdr.sock")).unwrap();
        fake(&bin, "herdr", r#"echo "herdr $*" >> "$AM_LOG"; [ "$1" = session ] && [ "$2" = list ] && echo "test running"; exit 0"#);
        fake(&bin, "uname", "echo Darwin");
        fake(&bin, "stat", "id -un");
        fake(&bin, "launchctl", r#"echo "launchctl $*" >> "$AM_LOG"; [ "$1" = print ] && exit 1; exit 0"#);
        fake(&bin, "sleep", "exit 0");
        let cfg = HostCfg {
            name: "sh1".into(),
            ssh: "sh1.invalid".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "test".into(),
            remote_path: String::new(),
            shared_session: shared,
        };
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(HostConn::remote_session_script(&cfg, shared))
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("HOME", &home)
            .env("AM_LOG", &log)
            .output()
            .unwrap();
        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        std::fs::remove_dir_all(&root).ok();
        (String::from_utf8_lossy(&out.stdout).to_string(), calls)
    }

    #[test]
    fn a_running_shared_session_is_never_stopped_or_handed_to_launchd() {
        let (out, calls) = run(true);
        assert!(out.contains("AM_MODE=shared-running") && out.contains("AM_OK=1"), "{out}");
        assert!(!calls.contains("server stop"), "{calls}");
        assert!(!calls.contains("launchctl bootstrap"), "{calls}");

        let (out, calls) = run(false);
        assert!(calls.contains("server stop"), "不共用時照舊交給 launchd（對照組）：{calls}\n{out}");
    }

    #[test]
    fn macos_local_generated_launchd_plist_xml_escapes_paths_with_xml_metacharacters() {
        let (plist, bin_path, _, _, _) = generated_launchd_plist();
        let escaped = bin_path.replace('&', "&amp;");
        assert!(plist.contains(&escaped), "plist did not XML-escape the executable path: {plist}");
        assert!(!plist.contains(&bin_path), "plist still contains the unescaped executable path: {plist}");
    }

    #[test]
    fn macos_local_generated_launchd_plist_and_directory_are_private_under_a_permissive_umask() {
        let (_, _, file_mode, dir_mode, owner) = generated_launchd_plist();
        assert_eq!(owner, unsafe { libc::geteuid() }, "launch agent plist must belong to the SSH user");
        assert_eq!(file_mode & 0o077, 0, "launch agent plist must not be group/world accessible");
        assert_eq!(dir_mode & 0o022, 0, "new LaunchAgents directory must not be group/world writable");
    }
}

#[cfg(test)]
mod linux_systemd_script_tests {
    //! issue #677：遠端是 Linux、裝了 `herdr@.service` 就交給 systemd 看管；沒裝或起不來照舊 nohup；
    //! server 已經在跑（不管誰起的）一律不動。
    use super::*;

    fn fake(bin: &std::path::Path, name: &str, body: &str) {
        crate::testing::write_exec(bin.join(name), format!("#!/bin/sh\n{body}\n"));
    }

    /// `unit`：`systemctl --user cat herdr@test.service` 找得到；`start_ok`：start 成功（會把 server 起來）；
    /// `running`：一開始 server 就在跑。回傳 (stdout, 呼叫紀錄)。
    fn run(unit: bool, start_ok: bool, running: bool) -> (String, String) {
        let root = std::path::PathBuf::from(format!("/tmp/am-rl-{}", &crate::db::ulid()[18..]));
        let (bin, home, log) = (root.join("bin"), root.join("home"), root.join("log"));
        std::fs::create_dir_all(&bin).unwrap();
        let sock_dir = home.join(".config/herdr/sessions/test");
        std::fs::create_dir_all(&sock_dir).unwrap();
        let _sock = std::os::unix::net::UnixListener::bind(sock_dir.join("herdr.sock")).unwrap();
        let up = root.join("up");
        if running {
            std::fs::write(&up, "").unwrap();
        }
        let up = up.display();
        fake(
            &bin,
            "herdr",
            &format!(
                r#"echo "herdr $*" >> "$AM_LOG"
[ "$1" = session ] && [ "$2" = list ] && [ -f {up} ] && echo "test running"
[ "$3" = server ] && touch {up}
exit 0"#
            ),
        );
        fake(&bin, "uname", "echo Linux");
        fake(&bin, "stat", "exit 1");
        fake(
            &bin,
            "systemctl",
            &format!(
                r#"echo "systemctl $* XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR" >> "$AM_LOG"
case "$2" in
  cat) exit {cat} ;;
  start) [ {start} = 0 ] && touch {up}; [ {start} = 0 ] || echo "Failed to start herdr@test.service" >&2; exit {start} ;;
esac
exit 0"#,
                cat = if unit { 0 } else { 1 },
                start = if start_ok { 0 } else { 1 },
            ),
        );
        // nohup 分支是背景起的：真的睡一下，等待迴圈才不會在假 herdr 起來之前就跑完（最多 20 × 50ms）。
        fake(&bin, "sleep", "/bin/sleep 0.05");
        let cfg = HostCfg {
            name: "lx1".into(),
            ssh: "lx1.invalid".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "test".into(),
            remote_path: String::new(),
            shared_session: false,
        };
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(HostConn::remote_session_script(&cfg, false))
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("HOME", &home)
            .env("AM_LOG", &log)
            .output()
            .unwrap();
        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        std::fs::remove_dir_all(&root).ok();
        (String::from_utf8_lossy(&out.stdout).to_string(), calls)
    }

    #[test]
    fn an_installed_unit_owns_the_server() {
        let (out, calls) = run(true, true, false);
        assert!(out.contains("AM_MODE=systemd\n") && out.contains("AM_OK=1"), "{out}");
        assert!(calls.contains("systemctl --user start herdr@test.service"), "{calls}");
        // 沒有登入 session 的 ssh 也連得到 user bus。
        assert!(calls.contains("XDG_RUNTIME_DIR=/run/user/"), "{calls}");
        assert!(!calls.contains("--session test server"), "unit 起了就不能再 nohup 一顆：{calls}");
    }

    #[test]
    fn without_the_unit_or_when_it_fails_the_server_is_started_the_old_way() {
        let (out, calls) = run(false, true, false);
        assert!(out.contains("AM_MODE=nohup") && out.contains("AM_OK=1"), "{out}");
        assert!(!calls.contains("--user start"), "{calls}");
        assert!(calls.contains("herdr --session test server"), "{calls}");

        let (out, calls) = run(true, false, false);
        assert!(out.contains("AM_MODE=nohup-fallback systemctl: Failed to start herdr@test.service"), "{out}");
        assert!(out.contains("AM_OK=1"), "{out}");
        assert!(calls.contains("herdr --session test server"), "{calls}");
    }

    #[test]
    fn a_running_server_is_left_alone() {
        let (out, calls) = run(true, true, true);
        assert!(out.contains("AM_MODE=systemd-existing") && out.contains("AM_OK=1"), "{out}");
        assert!(!calls.contains("--user start") && !calls.contains("server stop") && !calls.contains("test server"), "{calls}");
    }
}
