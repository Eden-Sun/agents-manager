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
use std::path::{Path, PathBuf};
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
/// 測試用的 ping 間隔（毫秒，0＝用 [`PING_INTERVAL`]）；只在 test／`test-hooks` 編譯，正式版固定 10 秒。
#[cfg(any(test, feature = "test-hooks"))]
static PING_INTERVAL_OVERRIDE_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(any(test, feature = "test-hooks"))]
pub fn set_ping_interval_for_test(d: Option<Duration>) {
    PING_INTERVAL_OVERRIDE_MS.store(d.map_or(0, |d| d.as_millis().max(1) as u64), Ordering::SeqCst);
}
fn ping_interval() -> Duration {
    #[cfg(any(test, feature = "test-hooks"))]
    {
        let ms = PING_INTERVAL_OVERRIDE_MS.load(std::sync::atomic::Ordering::SeqCst);
        if ms > 0 {
            return Duration::from_millis(ms);
        }
    }
    PING_INTERVAL
}
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
    // SAFETY: geteuid(2) has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    match instance {
        Some(slug) => PathBuf::from(format!("/tmp/agents-manager-{uid}-{slug}")),
        None => PathBuf::from(format!("/tmp/agents-manager-{uid}")),
    }
}

/// ssh 控制目錄（`/tmp/agents-manager-<uid>[-slug]`）：路徑可預測，所以只准是自己的、0700、非符號連結的真目錄。
/// 不存在就以 0700 建立；已存在但不合就回錯（不去 chmod 不是自己的目錄，別人預先佔位只會讓連線失敗，不會讓 socket 落進別人的目錄）。
pub fn ensure_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("建立 ssh 控制目錄 {}", path.display())),
    }
    let meta = std::fs::symlink_metadata(path).with_context(|| format!("檢查 ssh 控制目錄 {}", path.display()))?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        bail!("不安全的 ssh 控制目錄 {}：不是一般目錄（可能是符號連結）", path.display());
    }
    // SAFETY: geteuid(2) has no preconditions and cannot fail.
    let me = unsafe { libc::geteuid() };
    if meta.uid() != me {
        bail!("不安全的 ssh 控制目錄 {}：擁有者是 uid {}，不是目前的 uid {me}", path.display(), meta.uid());
    }
    if meta.mode() & 0o077 != 0 {
        bail!("不安全的 ssh 控制目錄 {}：權限 {:o} 允許其他人存取（需要 0700）", path.display(), meta.mode() & 0o777);
    }
    Ok(())
}

pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// 本機跑一段 `/bin/sh -c` 的完整結果。逾時時 `status` 是 `None`、`timed_out` 為真，但**逾時前已讀到的 stdout／stderr 都留著**：
/// 有些指令會先印出關鍵字再卡住等人（例如 `agy -p /usage` 印 `Authentication required` 後等人去開網址），
/// 只回「逾時」會讓呼叫端認不出它其實是沒登入（issue #870）。
pub struct LocalRun {
    pub status: Option<std::process::ExitStatus>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
}

/// 把管線讀進共用緩衝：被取消時已經讀到的部分還在。
fn drain_into<R>(pipe: Option<R>, buf: Arc<std::sync::Mutex<Vec<u8>>>) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let Some(mut pipe) = pipe else { return };
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(&chunk[..n]),
            }
        }
    })
}

/// 逾時時整個行程群組都砍掉（卡住的 `git merge`、pinentry 不能繼續佔著 `index.lock`），並回傳逾時前已讀到的輸出。
pub async fn sh_local_capture(script: &str, timeout: Duration) -> Result<LocalRun> {
    let mut cmd = tokio::process::Command::new("/bin/sh");
    cmd.arg("-c").arg(script).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.process_group(0);
    cmd.kill_on_drop(true);
    let mut child = cmd.spawn().context("spawn /bin/sh")?;
    let pid = child.id();
    let out_buf = Arc::new(std::sync::Mutex::new(Vec::new()));
    let err_buf = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut read_out = drain_into(child.stdout.take(), out_buf.clone());
    let mut read_err = drain_into(child.stderr.take(), err_buf.clone());
    let finished = tokio::select! {
        r = async {
            let status = child.wait().await.context("wait /bin/sh")?;
            let _ = (&mut read_out).await;
            let _ = (&mut read_err).await;
            Ok::<_, anyhow::Error>(status)
        } => Some(r?),
        _ = tokio::time::sleep(timeout) => None,
    };
    let status = match finished {
        Some(status) => Some(status),
        None => {
            if let Some(pid) = pid {
                let _ = tokio::process::Command::new("/bin/kill").args(["-9", "--", &format!("-{pid}")]).status().await;
            }
            let _ = child.kill().await;
            // 群組被砍之後管線裡可能還有沒讀完的幾個位元組；給一小段時間排空，不等孫行程握著管線不放的情況。
            let _ = tokio::time::timeout(Duration::from_millis(300), async {
                let _ = (&mut read_out).await;
                let _ = (&mut read_err).await;
            })
            .await;
            read_out.abort();
            read_err.abort();
            None
        }
    };
    let take = |b: &Arc<std::sync::Mutex<Vec<u8>>>| std::mem::take(&mut *b.lock().unwrap_or_else(|e| e.into_inner()));
    Ok(LocalRun { timed_out: status.is_none(), status, stdout: take(&out_buf), stderr: take(&err_buf) })
}

/// `None` = timed out. The whole process group is killed so a hung `git merge` (pinentry,
/// stuck remote) cannot keep holding `index.lock` after the caller gave up.
pub async fn sh_local(script: &str, timeout: Duration) -> Result<Option<std::process::Output>> {
    let run = sh_local_capture(script, timeout).await?;
    Ok(run.status.map(|status| std::process::Output { status, stdout: run.stdout, stderr: run.stderr }))
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
    pub supervisor: Mutex<Option<tokio::task::JoinHandle<()>>>,
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

    pub fn remote(cfg: HostCfg, instance: Option<String>) -> Arc<Self> {
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

    #[cfg(any(test, feature = "test-hooks"))]
    fn remote_with_client_for_test(cfg: HostCfg, instance: Option<String>, client: HerdrClient) -> Arc<Self> {
        let mut conn = Self::remote(cfg, instance);
        Arc::get_mut(&mut conn).expect("test remote connection is uniquely owned").client = client;
        conn
    }

    pub fn is_local(&self) -> bool {
        self.cfg.is_none()
    }

    /// Test seam: 放一個假的 ssh master 行程（`run_connected` 看它活著就當 master 還在）。
    #[cfg(any(test, feature = "test-hooks"))]
    pub async fn set_master_for_test(&self, child: tokio::process::Child) {
        *self.master.lock().await = Some(child);
    }

    /// Test seam: what an explicit reconnect does to superseded observations.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn bump_generation_for_test(&self) {
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

    pub fn mark_up(&self) {
        *self.down_since.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// 只記第一次：重試失敗不會把斷線起點往後推。
    pub fn mark_down(&self) {
        self.down_since.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(crate::db::now);
    }

    pub async fn error_string(&self) -> Option<String> {
        self.error.lock().await.clone()
    }

    fn ctl_path(&self) -> PathBuf {
        short_dir(self.instance.as_deref()).join(format!("{}.ctl", self.name))
    }

    pub fn ssh_args(&self) -> Vec<String> {
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
        #[cfg(any(test, feature = "test-hooks"))]
        if let Some(d) = ssh_delay_for(&self.name) {
            tokio::time::sleep(d).await;
        }
        #[cfg(any(test, feature = "test-hooks"))]
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

    pub fn instance(&self) -> Option<&str> {
        self.instance.as_deref()
    }

    /// For payloads like a gh token: script in argv, bytes on stdin (same quoting as `ssh_put`).
    /// Failures report stderr only — stdout may be sensitive.
    pub async fn ssh_exec_stdin(&self, script: &str, data: &[u8], timeout: Duration) -> Result<String> {
        #[cfg(any(test, feature = "test-hooks"))]
        if let Some(d) = ssh_delay_for(&self.name) {
            tokio::time::sleep(d).await;
        }
        #[cfg(any(test, feature = "test-hooks"))]
        if let Some(f) = ssh_fake_io_for(&self.name) {
            let out = f(script, data)?;
            return Ok(String::from_utf8_lossy(&out).to_string());
        }
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

    pub async fn ssh_stream(&self, script: &str, stdin: &[u8]) -> Result<SshStream> {
        #[cfg(any(test, feature = "test-hooks"))]
        if let Some(d) = ssh_delay_for(&self.name) {
            tokio::time::sleep(d).await;
        }
        #[cfg(any(test, feature = "test-hooks"))]
        if let Some(f) = ssh_fake_io_for(&self.name) {
            let data = f(script, stdin)?;
            if let Some((chunk_size, delay)) = ssh_stream_chunk_delay_for(&self.name) {
                return Ok(SshStream::from_reader(ChunkDelayedReader {
                    data,
                    pos: 0,
                    chunk_size,
                    delay,
                    sleep: None,
                }));
            }
            return Ok(SshStream::from_reader(std::io::Cursor::new(data)));
        }
        let Some(cfg) = &self.cfg else { bail!("ssh_stream called on the local host") };
        let remote = format!("/bin/sh -c {}", sh_quote(script));
        let mut cmd = tokio::process::Command::new("ssh");
        cmd.args(self.ssh_args()).arg(&cfg.ssh).arg(&remote);
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd.kill_on_drop(true);
        let mut child = cmd.spawn().with_context(|| format!("spawn ssh {}", cfg.ssh))?;
        if let Some(mut sin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt as _;
            if !stdin.is_empty() {
                sin.write_all(stdin).await.context("write stdin to ssh")?;
            }
            sin.shutdown().await.ok();
        }
        let stdout = child.stdout.take().ok_or_else(|| anyhow::anyhow!("no stdout from ssh"))?;
        Ok(SshStream::from_child(child, stdout))
    }

    /// SPEC §11.2 `remote_path` 的原始設定（組遠端受限 bot 的 PATH 用，見 `share::cage::remote_cage_path`）。
    pub fn remote_path(&self) -> String {
        self.cfg.as_ref().map(|c| c.remote_path.clone()).unwrap_or_default()
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
    pub fn remote_session_script(cfg: &HostCfg, shared: bool) -> String {
        let sess = &cfg.herdr_session;
        let shared = u8::from(shared);
        let q = sh_quote(sess);
        let script = format!(
            r#"printf 'AM_HOME=%s\n' "$HOME"
# #887：暫存檔與 log 一律只有自己讀得到；server 本身要用回原本的 umask（見 nohup 分支）。
AM_UMASK=$(umask); umask 077
S={q}
SHARED={shared}
SOCK="$HOME/.config/herdr/sessions/$S/herdr.sock"
LABEL="dev.agents-manager.herdr-$S"
mkdir -p "$HOME/.config/agents-manager"
LOG="$HOME/.config/agents-manager/herdr-$S.log"
# 錯誤輸出暫存：mktemp 的隨機檔名（不用 /tmp 底下可預測的固定檔名）；建不起來就丟掉。
new_err() {{ ERR=$(mktemp "${{TMPDIR:-/tmp}}/am-err.XXXXXX") || ERR=/dev/null; }}
drop_err() {{ [ "$ERR" = /dev/null ] || rm -f "$ERR"; }}
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
  else
    new_err
    if systemctl --user start "herdr@$S.service" 2>"$ERR"; then
      printf 'AM_MODE=systemd\n'
    else
      printf 'AM_MODE=nohup-fallback systemctl: %s\n' "$(tr '\n' ' ' <"$ERR")"
      MODE=nohup
    fi
    drop_err
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
    XML_LOG=$(xml_escape "$LOG")
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
  <key>StandardOutPath</key><string>$XML_LOG</string>
  <key>StandardErrorPath</key><string>$XML_LOG</string>
</dict></plist>
AM_PLIST
    if chmod 600 "$PL" 2>/dev/null; then
      if running; then
        # A server started the old way (nohup) is still up: stop it so launchd owns the next one.
        herdr --session "$S" server stop >/dev/null 2>&1 || true
        sleep 1
      fi
      new_err
      if launchctl bootstrap "gui/$(id -u)" "$PL" 2>"$ERR"; then
        printf 'AM_MODE=launchd\n'
      else
        printf 'AM_MODE=nohup-fallback launchctl: %s\n' "$(tr '\n' ' ' <"$ERR")"
        MODE=nohup
      fi
      drop_err
    else
      printf 'AM_MODE=nohup-fallback launchd plist permissions\n'
      MODE=nohup
    fi
  fi
fi
if [ "$MODE" = nohup ] && ! running; then
  # `nohup` refuses to detach when stderr is not a console on some macOS builds,
  # so ignore SIGHUP in a subshell instead.
  # log 先在 umask 077 下建好（0600）；server 自己用回原本的 umask，不然它底下每個 pane 的 agent 都會繼承 077。
  : >"$LOG"
  ( trap '' HUP; umask "$AM_UMASK"; herdr --session "$S" server </dev/null >"$LOG" 2>&1 & )
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
        ensure_private_dir(&dir)?;
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
pub async fn run_script_over_stdin(cmd: tokio::process::Command, script: &str, timeout: Duration, target: &str) -> Result<String> {
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
pub async fn run_with_stdin(
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

/// 連上之後到斷線為止（SPEC §11.3.4）：標成連線中、推 `host_changed`，**另起一個 task** 跑 [`HostHooks::host_connected`]
/// （對帳、事件訂閱、autostart… 可能很久，甚至卡在遠端不回話），自己立刻開始 ping／檢查 ssh master——
/// 以前是同步等 post-connect 跑完才開始 ping，那段時間連線死了也發現不了（#888）。
/// 斷線、ping 失敗、世代變了、supervisor 被 abort 都會 abort 那個 task。回 `true`＝世代變了（被 reconnect／換設定取代，呼叫端直接結束 supervisor），
/// `false`＝斷線了，呼叫端照退避重連。
pub async fn run_connected<H: HostHooks>(app: &Arc<H>, conn: &Arc<HostConn>, fence: &HostFence, generation: u64) -> bool {
    conn.mark_up();
    conn.connected.store(true, Ordering::SeqCst);
    *conn.error.lock().await = None;
    H::host_changed(app, fence).await;
    // supervisor 被 abort（reconnect／換設定／移除）時這個 future 直接被丟掉：guard 一起收掉 post-connect task，不留孤兒。
    let post = AbortOnDrop(tokio::spawn(H::host_connected(app.clone(), conn.name.clone())));

    loop {
        tokio::time::sleep(ping_interval()).await;
        if conn.generation.load(Ordering::SeqCst) != generation {
            return true;
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
    drop(post);
    conn.mark_down();
    conn.connected.store(false, Ordering::SeqCst);
    conn.kill_master().await;
    H::host_changed(app, fence).await;
    false
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
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
                    if run_connected(&app, &conn, &fence, generation).await {
                        return;
                    }
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

/// SSH 串流 stdout 的 AsyncRead 包裝；drop 時 kill 行程（kill_on_drop）。
pub struct SshStream {
    inner: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
    _child: Option<tokio::process::Child>,
}

impl SshStream {
    pub fn from_child(child: tokio::process::Child, stdout: tokio::process::ChildStdout) -> Self {
        Self {
            inner: Box::pin(stdout),
            _child: Some(child),
        }
    }

    pub fn from_reader(reader: impl tokio::io::AsyncRead + Send + 'static) -> Self {
        Self {
            inner: Box::pin(reader),
            _child: None,
        }
    }
}

impl tokio::io::AsyncRead for SshStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

/// Test seam: a fake "ssh" per host name, so remote-side effects (`rm -rf` of a bot dir …) can be observed without a network.
#[cfg(any(test, feature = "test-hooks"))]
type SshFake = Arc<dyn Fn(&str) -> Result<String> + Send + Sync>;
#[cfg(any(test, feature = "test-hooks"))]
static SSH_FAKES: std::sync::Mutex<Vec<(String, SshFake)>> = std::sync::Mutex::new(Vec::new());
#[cfg(any(test, feature = "test-hooks"))]
pub fn set_ssh_fake(host: &str, f: impl Fn(&str) -> Result<String> + Send + Sync + 'static) {
    let mut v = SSH_FAKES.lock().unwrap();
    v.retain(|(h, _)| h != host);
    v.push((host.to_string(), Arc::new(f)));
}
#[cfg(any(test, feature = "test-hooks"))]
fn ssh_fake_for(host: &str) -> Option<SshFake> {
    SSH_FAKES.lock().unwrap().iter().find(|(h, _)| h == host).map(|(_, f)| f.clone())
}

#[cfg(any(test, feature = "test-hooks"))]
type SshFakeIo = Arc<dyn Fn(&str, &[u8]) -> Result<Vec<u8>> + Send + Sync>;
#[cfg(any(test, feature = "test-hooks"))]
static SSH_FAKE_IOS: std::sync::Mutex<Vec<(String, SshFakeIo)>> = std::sync::Mutex::new(Vec::new());
#[cfg(any(test, feature = "test-hooks"))]
pub fn set_ssh_fake_io(host: &str, f: impl Fn(&str, &[u8]) -> Result<Vec<u8>> + Send + Sync + 'static) {
    let mut v = SSH_FAKE_IOS.lock().unwrap();
    v.retain(|(h, _)| h != host);
    v.push((host.to_string(), Arc::new(f)));
}
#[cfg(any(test, feature = "test-hooks"))]
pub fn clear_ssh_fake_io(host: &str) {
    let mut v = SSH_FAKE_IOS.lock().unwrap();
    v.retain(|(h, _)| h != host);
}
#[cfg(any(test, feature = "test-hooks"))]
fn ssh_fake_io_for(host: &str) -> Option<SshFakeIo> {
    SSH_FAKE_IOS.lock().unwrap().iter().find(|(h, _)| h == host).map(|(_, f)| f.clone())
}

#[cfg(any(test, feature = "test-hooks"))]
static SSH_STREAM_CHUNK_DELAYS: std::sync::Mutex<Vec<(String, (usize, Duration))>> = std::sync::Mutex::new(Vec::new());
#[cfg(any(test, feature = "test-hooks"))]
pub fn set_ssh_stream_chunk_delay(host: &str, chunk_size: usize, delay: Duration) {
    let mut v = SSH_STREAM_CHUNK_DELAYS.lock().unwrap();
    v.retain(|(h, _)| h != host);
    v.push((host.to_string(), (chunk_size, delay)));
}
#[cfg(any(test, feature = "test-hooks"))]
fn ssh_stream_chunk_delay_for(host: &str) -> Option<(usize, Duration)> {
    SSH_STREAM_CHUNK_DELAYS.lock().unwrap().iter().find(|(h, _)| h == host).map(|(_, d)| *d)
}

#[cfg(any(test, feature = "test-hooks"))]
struct ChunkDelayedReader {
    data: Vec<u8>,
    pos: usize,
    chunk_size: usize,
    delay: Duration,
    sleep: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
}

#[cfg(any(test, feature = "test-hooks"))]
impl tokio::io::AsyncRead for ChunkDelayedReader {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.pos >= self.data.len() {
            return std::task::Poll::Ready(Ok(()));
        }
        if self.delay > Duration::ZERO {
            if self.sleep.is_none() {
                self.sleep = Some(Box::pin(tokio::time::sleep(self.delay)));
            }
            if let Some(sleep) = self.sleep.as_mut() {
                if sleep.as_mut().poll(cx).is_pending() {
                    return std::task::Poll::Pending;
                }
            }
            self.sleep = None;
        }
        let end = (self.pos + self.chunk_size.max(1)).min(self.data.len());
        let chunk = &self.data[self.pos..end];
        let to_write = chunk.len().min(buf.remaining());
        buf.put_slice(&chunk[..to_write]);
        self.pos += to_write;
        std::task::Poll::Ready(Ok(()))
    }
}

/// Test seam: make every ssh leg to this host take that long *asynchronously* — a host that accepts the
/// connection and then says nothing. Pair it with [`set_ssh_fake`] for what the reply would have been.
#[cfg(any(test, feature = "test-hooks"))]
static SSH_DELAYS: std::sync::Mutex<Vec<(String, Duration)>> = std::sync::Mutex::new(Vec::new());
#[cfg(any(test, feature = "test-hooks"))]
pub fn set_ssh_delay(host: &str, d: Duration) {
    let mut v = SSH_DELAYS.lock().unwrap();
    v.retain(|(h, _)| h != host);
    v.push((host.to_string(), d));
}
#[cfg(any(test, feature = "test-hooks"))]
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
pub struct HostAuthorityKey {
    conn: std::sync::Weak<HostConn>,
    generation: u64,
}

impl HostAuthorityKey {
    pub fn matches(&self, fence: &HostFence) -> bool {
        self.generation == fence.generation && self.conn.ptr_eq(&Arc::downgrade(&fence.conn))
    }
}

impl HostFence {
    pub fn conn(&self) -> &Arc<HostConn> {
        &self.conn
    }

    /// 這一個 fence 綁的主機世代（`replace`／改設定／重連會換代）。issue #1035：run 啟動時記下它，admission 比對。
    pub fn generation(&self) -> u64 {
        self.generation
    }

    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn authority_gate_for_test(&self) -> &tokio::sync::RwLock<()> {
        &self.conn.authority_gate
    }

    /// Tickets from the same connection generation represent the same host authority.
    pub fn same_authority(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.conn, &other.conn) && self.generation == other.generation
    }

    pub fn authority_key(&self) -> HostAuthorityKey {
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
pub async fn home_for_fence(fence: &HostFence) -> Result<String> {
    let conn = fence.conn();
    if conn.is_local() {
        return Ok(dirs::home_dir().map(|p| p.display().to_string()).unwrap_or_else(|| "/tmp".into()));
    }
    conn.home().await.with_context(|| format!("remote HOME for host `{}` is unreadable", conn.name))
}

pub struct HostManager {
    pub conns: Mutex<HashMap<String, Arc<HostConn>>>,
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
    pub async fn current_generation(&self, name: &str) -> Option<u64> {
        let conn = self.get(name).await?;
        (!conn.retiring.load(Ordering::SeqCst)).then(|| conn.generation.load(Ordering::SeqCst))
    }

    /// 世代還對得上才回連線：重連／改設定之後舊世代拿到 `None`，操作不會被送到換過的那條連線上。
    pub async fn conn_at_generation(&self, name: &str, generation: u64) -> Option<Arc<HostConn>> {
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
    pub async fn fence_for_generation(&self, conn: &Arc<HostConn>, generation: u64) -> Option<HostFence> {
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
    #[cfg(any(test, feature = "test-hooks"))]
    pub async fn insert_remote_for_test(&self, cfg: HostCfg) -> Arc<HostConn> {
        let conn = HostConn::remote(cfg, None);
        self.insert_remote_conn_for_test(conn).await
    }

    /// Test seam: like [`Self::insert_remote_for_test`], with a caller-provided fake Herdr socket.
    #[cfg(any(test, feature = "test-hooks"))]
    pub async fn insert_remote_with_client_for_test(&self, cfg: HostCfg, client: HerdrClient) -> Arc<HostConn> {
        let conn = HostConn::remote_with_client_for_test(cfg, None, client);
        self.insert_remote_conn_for_test(conn).await
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub async fn insert_remote_conn_for_test(&self, conn: Arc<HostConn>) -> Arc<HostConn> {
        if let Some(old) = self.get(&conn.name).await {
            let _authority = old.authority_gate.write().await;
            self.conns.lock().await.insert(conn.name.clone(), conn.clone());
        } else {
            self.conns.lock().await.insert(conn.name.clone(), conn.clone());
        }
        conn
    }

    /// Test seam: [`Self::apply_config`] 換掉同名連線的那一步（含快取失效），只是不起 supervisor、不真的連 ssh。
    #[cfg(any(test, feature = "test-hooks"))]
    pub async fn replace_remote_for_test<H: HostHooks>(&self, app: &Arc<H>, cfg: HostCfg) -> Arc<HostConn> {
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
    #[cfg(any(test, feature = "test-hooks"))]
    pub async fn replace_remote_with_client_for_test<H: HostHooks>(
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

pub fn cfg_differs(a: &HostCfg, b: &HostCfg) -> bool {
    a.ssh != b.ssh
        || a.ssh_port != b.ssh_port
        || a.ssh_opts != b.ssh_opts
        || a.herdr_session != b.herdr_session
        || a.remote_path != b.remote_path
}

/// 一次最多列幾個子目錄（本機與遠端同一個數字）；超過就截斷並回 `truncated:true`。
pub const DIR_LIST_LIMIT: usize = 2000;

/// 遠端目錄列舉要跑的 sh（純字串，可以拿到本機直接跑來驗）。

/// 輸出靠 `AM_*` 開頭的行解析，所以：目前路徑或目錄名字含控制字元（換行、tab、CR…）的一律不列——否則一個叫
/// `a\nAM_PATH=/etc` 的目錄就能偽造「目前路徑」；憑證與金鑰目錄（`~/.ssh`、`~/.gnupg`、`~/.aws`、`~/.kube`、daemon 與各 CLI 的設定目錄）
/// 不給進；子目錄最多 [`DIR_LIST_LIMIT`] 個。
pub fn dir_list_script(path: Option<&str>, hidden: bool) -> String {
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
pub fn parse_dir_listing(out: &str, hidden: bool) -> Result<serde_json::Value> {
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

/// 遠端「新資料夾」的結果。
#[derive(Debug, PartialEq, Eq)]
pub enum MakeDirOutcome {
    /// 建好了，`pwd -P` 解開後的完整路徑。
    Created(String),
    /// 同名的東西（資料夾、檔案、符號連結）已經在那裡：什麼都沒動。
    Exists,
}

/// 在 `parent` 底下新建 `name`（單一一段、已過 `bot_input::check_new_dir_name`）的 sh。規則跟 [`dir_list_script`] 同一份：
/// `parent` 解開後在憑證／設定目錄底下、或新路徑本身就是那些目錄 → `forbidden directory`；路徑含控制字元 → 錯。
/// `mkdir`（不 `-p`）自己判斷「已經有了」：先 `-e/-L` 看一眼只是為了給乾淨的 `AM_EXISTS`，真正擋重複的是 `mkdir` 失敗後的再看一次。
pub fn make_dir_script(parent: &str, name: &str) -> String {
    let target = match parent.trim() {
        "" | "~" => "\"$HOME\"".to_string(),
        // Tail is user input: must be quoted; only `"$HOME"` stays outside.
        p if p.starts_with("~/") => format!("\"$HOME\"/{}", sh_quote(&p[2..])),
        p => sh_quote(p),
    };
    let name = sh_quote(name);
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
if [ "$P" = / ]; then N=/{name}; else N="$P"/{name}; fi
case "$N" in "$H"/.ssh|"$H"/.gnupg|"$H"/.aws|"$H"/.kube|"$H"/.config/agents-manager|"$H"/.claude|"$H"/.claude-*|"$H"/.codex|"$H"/.grok) printf 'AM_ERR=forbidden directory\n'; exit 0;; esac
if [ -e {name} ] || [ -L {name} ]; then printf 'AM_EXISTS=1\n'; exit 0; fi
if ! mkdir -- {name} 2>/dev/null; then
  if [ -e {name} ] || [ -L {name} ]; then printf 'AM_EXISTS=1\n'; else printf 'AM_ERR=cannot create directory (permission denied or read-only)\n'; fi
  exit 0
fi
printf 'AM_PATH=%s\n' "$(cd -- {name} && pwd -P)"
"#
    )
}

/// [`make_dir_script`] 的輸出。
pub fn parse_make_dir(out: &str) -> Result<MakeDirOutcome> {
    let mut path: Option<String> = None;
    for line in out.lines() {
        if line == "AM_EXISTS=1" {
            return Ok(MakeDirOutcome::Exists);
        } else if let Some(e) = line.strip_prefix("AM_ERR=") {
            bail!("{}", e.trim());
        } else if let Some(v) = line.strip_prefix("AM_PATH=") {
            path = Some(v.trim_end().to_string());
        }
    }
    match path.filter(|p| !p.is_empty()) {
        Some(p) => Ok(MakeDirOutcome::Created(p)),
        None => bail!("remote mkdir produced no path:\n{}", out.trim()),
    }
}

/// `POST /api/fs/dirs` 的遠端那一半（§11.5）。
pub async fn remote_make_dir(conn: &HostConn, parent: &str, name: &str) -> Result<MakeDirOutcome> {
    let out = conn.ssh_exec(&make_dir_script(parent, name)).await?;
    parse_make_dir(&out)
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
