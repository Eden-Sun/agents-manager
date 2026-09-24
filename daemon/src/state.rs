//! Shared daemon state: db pool, herdr client, per-bot locks, WS event bus.

use crate::config::{valid_id, ConfigStore, ID_RE, LOCAL_HOST};
use crate::herdr::HerdrClient;
use crate::hosts::{HostConn, HostManager};
use anyhow::Result;
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::collections::{HashMap, VecDeque};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};

pub const WS_RING: usize = 200;

#[derive(Debug, Clone, Serialize)]
pub struct WsEvent {
    pub seq: u64,
    #[serde(rename = "type")]
    pub kind: String,
    pub data: Value,
}

/// 內部的「這個回合不再 in-flight」通知。刻意**不是** WS 事件：WS 有 200 筆 ring 與 resync，
/// 對 UI 夠用，對不能漏任何一筆的訂閱者不行。所有離開 `in_flight` 的路徑都走 `lifecycle::emit_turn`。
#[derive(Debug, Clone, Serialize)]
pub struct TurnEvent {
    pub bot_id: String,
    pub turn_id: String,
    /// `in_flight` | `completed` | `completed_fallback` | `failed`.
    pub status: String,
    pub delivery: String,
}

impl TurnEvent {
    /// Whether this turn has finished (the only transition a scheduler acts on).
    #[allow(dead_code)]
    /// 回合真的結束了嗎。**`queued` 不算**：排隊中的 turn 還沒送出去，把它當成結束會讓交辦被
    /// 當作失敗結案（2026-09-16 AGM：notice 排進佇列卻推了 assignment_failed）。
    pub fn is_done(&self) -> bool {
        matches!(self.status.as_str(), "completed" | "completed_fallback" | "failed")
    }
}

pub struct App {
    pub db: SqlitePool,
    /// The local herdr client. Prefer `herdr_for(host)` — SPEC §11.3.6.
    pub herdr: HerdrClient,
    /// The user's local Herdr `default` session. It is observed when present, never spawned.
    pub default_herdr: HerdrClient,
    /// `local` plus every configured `[[hosts]]` entry.
    pub hosts: HostManager,
    pub cfg: ConfigStore,
    pub data_dir: PathBuf,
    pub exe: PathBuf,
    pub port: u16,
    pub ui_token: String,
    pub herdr_session: String,
    /// True for every dev run of the daemon (see `main.rs::dev_lan_default`): it is bound to
    /// every interface for direct LAN/Tailscale/etc access, so the peer-address and Origin
    /// checks accept any peer, not just loopback. Only the packaged macOS app — or an explicit
    /// `AM_DEV_LAN=0` — stays localhost-only.
    pub allow_lan: bool,
    /// #472：`roles::classify` 連續失敗幾拍了（成功就歸零）。
    ///
    /// 記憶體、重啟重算——跟 incident 的門檻計時同一個原則（SPEC §18.9）。
    /// 它失敗的後果是「新進 inbox 事件分不到角色，對兩個通知者同時隱形」，
    /// 而那件事沒有任何其他偵測會發現，所以要自己數。
    /// #480：`judge::stuck` 這一輪看過哪些 turn（key＝turn_id）。冷卻期內不再看第二次。
    ///
    /// 兩個作用：候選有上限時不會永遠只看最舊那幾顆（第 11 顆會餓死），
    /// 而且已經問過的那幾顆不用每輪再付一次 herdr 讀畫面。記憶體、重啟重算。
    pub judge_stuck_seen: Mutex<HashMap<String, std::time::Instant>>,
    pub classify_failures: std::sync::atomic::AtomicU32,
    pub connected: std::sync::atomic::AtomicBool,
    pub default_connected: std::sync::atomic::AtomicBool,
    /// How "which account is this pid running under" gets answered (SPEC §16.6). Empty in a
    /// real daemon, which means `ps`; a test installs its own reader.
    pub proc_env: crate::pane_identity::ProcEnvHook,
    /// start_bot 的 preflight「這台主機上有沒有 agent 執行檔」怎麼去問。正式 daemon 是 `command -v`；
    /// 測試 build 預設答「有」，不吃跑測試那台機器的 PATH（issue #139）。
    pub kind_probe: crate::kind_probe::KindProbeHook,

    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    bus: broadcast::Sender<WsEvent>,
    /// Internal turn-completion bus (the supervisor controller subscribes).
    turn_bus: broadcast::Sender<TurnEvent>,
    seq: AtomicU64,
    ring: Mutex<VecDeque<WsEvent>>,
    /// (host, session, pane_id) -> per-run agent_status subscription task
    pub pane_watchers: Mutex<HashMap<(String, String, String), tokio::task::JoinHandle<()>>>,
    /// (host, session) -> global event subscription task
    pub global_watchers: Mutex<HashMap<(String, String), tokio::task::JoinHandle<()>>>,
    /// Serializes discovery/config projection against a default-session event and poll tick.
    pub default_sync_lock: Mutex<()>,
    /// run_id -> generation of the pending terminal-fallback timer
    pub fallback_timers: Mutex<HashMap<String, u64>>,
    /// run_id -> generation of the prompt-stall watchdog (delivered but the agent never went
    /// `working`)
    pub stall_timers: Mutex<HashMap<String, u64>>,
    /// run_id -> live-progress poller (streams the partial reply while a turn is in flight)
    pub progress_pollers: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    /// run_id -> 上次發 `turn_progress` 的時間（API.md：每個 run 每秒最多 4 幀）。放在 `App` 而不是
    /// poller 裡，否則同一個 run 換一回合就重開預算、一次爆量。
    pub progress_emitted: Mutex<HashMap<String, std::time::Instant>>,
    /// run_id -> 上次關滿意度問卷時的 pane revision：事件路徑與定期巡邏共用，避免按兩次。
    pub survey_revisions: Mutex<HashMap<String, u64>>,
    /// #427（#420 後續）：角色 bot（`patrol`／`responder`）當下為什麼不能用，key 是 role 字串。
    ///
    /// 故意**不進 DB**：跟 incident 的門檻計時同一個原則（SPEC §18.9「計時在記憶體，重啟重算」）。
    /// 寫進 DB 的「故障」會跟著重啟活過來，而重啟後第一拍就能重新判定——寧可晚 30 秒開，也不要讓
    /// 一顆已經修好的協調者因為陳舊的旗標被繼續當成壞的。空表＝還沒有結論，不是故障。
    pub role_faults: Mutex<HashMap<String, crate::supervisor::role_faults::RoleFault>>,
    /// v4.0: `GET /api/models` cache, key `<host>/<kind>` (10 min TTL).
    pub models_cache: Mutex<HashMap<String, (std::time::Instant, Value)>>,
    /// v4.0: quota per kind key (`codex`, `claude`, `claude:<identity>`).
    pub quotas: Mutex<std::collections::BTreeMap<String, crate::quota::Quota>>,
    /// 額度讀數是開機從 `quota_cache` 回填的，直到下一次探測前只供顯示，不能當成新證據。
    pub quota_stale: Mutex<std::collections::BTreeSet<String>>,
    /// v4.0: per-host CLI detection (`hosts[].tools`), refreshed on every (re)connect.
    pub tools: Mutex<HashMap<String, crate::tools::HostTools>>,
    /// Claude Code CHANGELOG.md 全文快取（`GET /api/changelog`，10 分鐘）。
    pub changelog: crate::changelog::ChangelogCache,
    /// v4.0: project id -> GitHub origin (`None` = checked, not GitHub).
    pub github: Mutex<HashMap<String, Option<crate::github::GithubInfo>>>,
    /// v4.0: `GET /projects/:id/issues` cache (2 min).
    pub issues_cache: Mutex<HashMap<String, (std::time::Instant, Value)>>,
    /// Submodules per project (`GET /projects/:id/submodules`), 2 min.
    pub submodules_cache: Mutex<HashMap<String, (std::time::Instant, Vec<crate::github::Submodule>)>>,
    /// In-flight GitHub device-flow logins, keyed by host name. Memory only; never persisted.
    pub gh_device: Mutex<HashMap<String, crate::gh_auth::DeviceSession>>,
    /// 這個 daemon 開過的主機 shell。只放記憶體：它同時是送鍵的白名單，重啟不該繼承。
    pub host_shells: crate::api::shell::Registry,
    /// 被 trace 的 pane 打字前的即時複查結果，幾秒內重用（`shell::live_verdict`）。
    pub pane_live: crate::api::shell::LiveCache,
    /// 預覽（`preview.rs`）的行程／port 查詢，測試換成假貨；正式是 herdr＋本機 TCP。
    #[cfg(test)]
    pub preview_env: crate::preview::EnvOverride,
    /// 行程 dump 與 listen port 的來源；正式是 `ps`／`lsof`，測試可換成決定性的假貨（`pane_probe`）。
    pub pane_probe: std::sync::Mutex<Arc<dyn crate::pane_probe::PaneProbe>>,
    /// 這顆 daemon 已經跑過 autostart 的主機（§6.1 第 6 步：每台主機一生一次，`reconcile::autostart_after_reconcile`）。
    pub autostarted_hosts: Mutex<std::collections::HashSet<String>>,
    /// hook 收件匣有新列時叫醒 worker（`hook_inbox`）。commit 完才 notify，所以 worker 一醒來
    /// 一定看得到那一列；沒有它就只剩輪詢，本機 hook 的處理延遲會從「幾毫秒」變成「幾秒」。
    pub hook_inbox_wake: tokio::sync::Notify,
    /// `serve` 起在非預設資料目錄時的實例名（`startup::instance`）。測試裡預設 `None`（正式實例）。
    instance: std::sync::RwLock<Option<String>>,
    /// issue #90：build scheduler 的「數名額、發／收名額」critical section。SQLite 本身也序列化寫入，
    /// 但這裡要的是「先數後寫」一起做完，不靠 SQL 的原子性猜實作細節。
    pub build_slot_lock: Mutex<()>,
    /// 這一輪開機的代號：寫進 DB 的東西（`lifecycle::quota_hold`）靠它分辨是不是這個行程自己寫的。
    pub boot_id: String,
}

impl App {
    pub fn probe(&self) -> Arc<dyn crate::pane_probe::PaneProbe> {
        self.pane_probe.lock().unwrap().clone()
    }

    pub fn new(
        db: SqlitePool,
        herdr: HerdrClient,
        default_herdr: HerdrClient,
        cfg: ConfigStore,
        data_dir: PathBuf,
        exe: PathBuf,
        port: u16,
        ui_token: String,
        herdr_session: String,
        allow_lan: bool,
    ) -> Arc<Self> {
        let (bus, _) = broadcast::channel(1024);
        let (turn_bus, _) = broadcast::channel(1024);
        Arc::new(Self {
            boot_id: crate::db::ulid(),
            instance: std::sync::RwLock::new(crate::startup::instance()),
            db,
            hosts: HostManager::new(herdr.clone()),
            herdr,
            default_herdr,
            cfg,
            data_dir,
            exe,
            port,
            ui_token,
            herdr_session,
            allow_lan,
            judge_stuck_seen: Mutex::new(HashMap::new()),
            classify_failures: std::sync::atomic::AtomicU32::new(0),
            connected: std::sync::atomic::AtomicBool::new(false),
            default_connected: std::sync::atomic::AtomicBool::new(false),
            proc_env: Default::default(),
            kind_probe: Default::default(),
            locks: Mutex::new(HashMap::new()),
            bus,
            turn_bus,
            seq: AtomicU64::new(0),
            ring: Mutex::new(VecDeque::new()),
            pane_watchers: Mutex::new(HashMap::new()),
            global_watchers: Mutex::new(HashMap::new()),
            default_sync_lock: Mutex::new(()),
            fallback_timers: Mutex::new(HashMap::new()),
            stall_timers: Mutex::new(HashMap::new()),
            progress_pollers: Mutex::new(HashMap::new()),
            progress_emitted: Mutex::new(HashMap::new()),
            survey_revisions: Mutex::new(HashMap::new()),
            role_faults: Mutex::new(HashMap::new()),
            models_cache: Mutex::new(HashMap::new()),
            quotas: Mutex::new(std::collections::BTreeMap::new()),
            quota_stale: Mutex::new(std::collections::BTreeSet::new()),
            tools: Mutex::new(HashMap::new()),
            changelog: Default::default(),
            github: Mutex::new(HashMap::new()),
            issues_cache: Mutex::new(HashMap::new()),
            submodules_cache: Mutex::new(HashMap::new()),
            gh_device: Mutex::new(HashMap::new()),
            host_shells: Default::default(),
            #[cfg(test)]
            preview_env: Default::default(),
            pane_live: Default::default(),
            pane_probe: std::sync::Mutex::new(Arc::new(crate::pane_probe::Real)),
            autostarted_hosts: Default::default(),
            hook_inbox_wake: tokio::sync::Notify::new(),
            build_slot_lock: Mutex::new(()),
        })
    }

    /// The herdr client for a host (`"local"` = this machine). `None` = unknown host.
    pub async fn herdr_for(&self, host: &str) -> Option<HerdrClient> {
        self.hosts.client(host).await
    }

    /// The configured session for a host. A bot may override the local host with `default`.
    pub async fn session_for_host(&self, host: &str) -> Option<String> {
        if host == LOCAL_HOST {
            Some(self.herdr_session.clone())
        } else {
            self.hosts.get(host).await.and_then(|c| c.cfg.as_ref().map(|cfg| cfg.herdr_session.clone()))
        }
    }

    /// 指定 host/session 的 client。本機 default session 是另一條 socket，daemon 不會去啟動它。
    pub async fn herdr_for_session(&self, host: &str, session: &str) -> Option<HerdrClient> {
        if host == LOCAL_HOST {
            if session == "default" {
                return Some(self.default_herdr.clone());
            }
            return (session == self.herdr_session).then(|| self.herdr.clone());
        }
        let conn = self.hosts.get(host).await?;
        let expected = conn.cfg.as_ref()?.herdr_session.as_str();
        (expected == session).then(|| conn.client.clone())
    }

    /// bot 實際用的 session：遠端一律是該 host 設定的 named session，只有本機採用的 bot 可指向 `default`。
    pub async fn session_for_bot(&self, bot: &crate::db::Bot, host: &str) -> Option<String> {
        if host == LOCAL_HOST {
            Some(bot.herdr_session.clone().unwrap_or_else(|| self.herdr_session.clone()))
        } else {
            self.session_for_host(host).await
        }
    }

    pub async fn session_connected(&self, host: &str, session: &str) -> bool {
        if host == LOCAL_HOST {
            if session == "default" {
                return self.default_connected.load(Ordering::SeqCst);
            }
            return session == self.herdr_session && self.connected.load(Ordering::SeqCst);
        }
        let Some(conn) = self.hosts.get(host).await else { return false };
        conn.cfg.as_ref().map(|cfg| cfg.herdr_session == session).unwrap_or(false) && conn.is_connected()
    }

    pub async fn bot_connected(&self, bot_id: &str) -> bool {
        let Ok(Some(bot)) = crate::db::bot(&self.db, bot_id).await else { return false };
        // 讀不到 host＝不知道，不當成 local 去比對本機 herdr（#243）。
        let Ok(host) = crate::db::bot_host(&self.db, bot_id).await else { return false };
        let Some(session) = self.session_for_bot(&bot, &host).await else { return false };
        self.session_connected(&host, &session).await
    }

    /// 既有 run 的 session：新 run 會存，沒存的沿用 bot／專案的設定。
    pub async fn session_for_run(&self, run: &crate::db::Run) -> Option<String> {
        if let Some(session) = run.herdr_session.clone().filter(|s| !s.is_empty()) {
            return Some(session);
        }
        let bot = crate::db::bot(&self.db, &run.bot_id).await.ok()??;
        let host = crate::db::bot_host(&self.db, &run.bot_id).await.ok()?;
        self.session_for_bot(&bot, &host).await
    }

    pub async fn herdr_for_run(&self, run: &crate::db::Run) -> Option<HerdrClient> {
        let host = crate::db::bot_host(&self.db, &run.bot_id).await.ok()?;
        let session = self.session_for_run(run).await?;
        self.herdr_for_session(&host, &session).await
    }

    /// Whether the host a bot lives on is currently usable (drives `lamp`).
    pub async fn host_connected(&self, host: &str) -> bool {
        if host == LOCAL_HOST {
            return self.connected.load(Ordering::SeqCst);
        }
        match self.hosts.get(host).await {
            Some(c) => c.is_connected(),
            None => false,
        }
    }

    /// Per-bot mutex. start / stop / prompt / hook matching / spool replay / reconcile all take it.
    pub async fn bot_lock(&self, bot_id: &str) -> Arc<Mutex<()>> {
        let mut g = self.locks.lock().await;
        g.entry(bot_id.to_string()).or_insert_with(|| Arc::new(Mutex::new(()))).clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<WsEvent> {
        self.bus.subscribe()
    }

    /// 訂閱回合結束。沒有 ring buffer：落後的訂閱者拿到 `RecvError::Lagged`，自己回頭讀 DB。
    #[allow(dead_code)]
    pub fn subscribe_turns(&self) -> broadcast::Receiver<TurnEvent> {
        self.turn_bus.subscribe()
    }

    /// Publish a turn transition. Sending with no subscribers is not an error.
    pub fn publish_turn(&self, ev: TurnEvent) {
        let _ = self.turn_bus.send(ev);
    }

    /// How many subscribers are currently listening (for tests / diagnostics).
    #[allow(dead_code)]
    pub fn turn_subscribers(&self) -> usize {
        self.turn_bus.receiver_count()
    }

    pub async fn emit(&self, kind: &str, data: Value) {
        // seq 在 ring 鎖**內**取，ring 順序＝seq 順序。在鎖外編號的話兩個並行 emit 可能先推 6 再推 5，
        // 斷線後帶 `since=6` 重連的客戶端就永遠收不到 5（review 2026-09-12 f）。
        {
            let mut ring = self.ring.lock().await;
            let ev = WsEvent { seq: self.seq.fetch_add(1, Ordering::SeqCst) + 1, kind: kind.to_string(), data };
            ring.push_back(ev.clone());
            while ring.len() > WS_RING {
                ring.pop_front();
            }
            let _ = self.bus.send(ev);
        }
    }

    pub fn current_seq(&self) -> u64 {
        self.seq.load(Ordering::SeqCst)
    }

    /// Backlog since `since` (exclusive). `None` = cannot satisfy, client must resync.
    pub async fn backlog(&self, since: u64) -> Option<Vec<WsEvent>> {
        let ring = self.ring.lock().await;
        let oldest = ring.front().map(|e| e.seq);
        let newest = self.current_seq();
        if since > newest {
            return None; // seq went backwards (daemon restarted)
        }
        match oldest {
            None => Some(vec![]),
            Some(o) if since + 1 >= o => Some(ring.iter().filter(|e| e.seq > since).cloned().collect()),
            Some(_) => None,
        }
    }

    /// 非預設資料目錄＝隔離實例。它不能認領既有 pane：那些 pane 的 hook 指向別顆 daemon 的
    /// 資料目錄，收編只會讓兩顆 daemon 互相吃對方的 spool（sol 複審二輪）。
    pub fn isolated(&self) -> bool {
        self.instance().is_some()
    }

    /// `None` = 正式實例。hook 腳本、grok dispatcher、遠端根目錄與 pane 的 `AM_INSTANCE` 都照這個分。
    pub fn instance(&self) -> Option<String> {
        self.instance.read().map(|g| g.clone()).unwrap_or(None)
    }

    #[cfg(test)]
    pub fn set_instance(&self, slug: Option<String>) {
        if let Ok(mut g) = self.instance.write() {
            *g = slug;
        }
    }

    pub fn bot_dir(&self, bot_id: &str) -> Result<PathBuf> {
        if !valid_id(bot_id) {
            anyhow::bail!("invalid bot id `{bot_id}` (must match {ID_RE})");
        }
        Ok(self.data_dir.join("bots").join(bot_id))
    }

    pub async fn emit_bot_status(&self, bot_id: &str) {
        if let Ok(Some(bot)) = crate::db::bot(&self.db, bot_id).await {
            let run = crate::db::active_run(&self.db, bot_id).await.ok().flatten();
            // 讀不到 host 就不發：發 `host:"local"` 會讓前端把遠端 bot 顯示成本機（#243）；DB 好了下一次事件會補。
            let Ok(host) = crate::db::bot_host(&self.db, bot_id).await else {
                tracing::warn!(bot_id, "bot_status not emitted: bot host unreadable");
                return;
            };
            self.emit(
                "bot_status",
                json!({
                    "bot_id": bot.id,
                    "run": run,
                    "host": host,
                    "herdr_session": bot.herdr_session,
                    "connected": self.bot_connected(bot_id).await,
                }),
            )
            .await;
        }
    }
}

/// `hosts` map for `daemon_status` / `GET /api/state`.
pub async fn hosts_json(app: &Arc<App>) -> Value {
    let mut m = serde_json::Map::new();
    for c in app.hosts.list().await {
        let connected = if c.is_local() { app.connected.load(Ordering::SeqCst) } else { c.is_connected() };
        m.insert(c.name.clone(), json!({"connected": connected, "error": c.error_string().await}));
    }
    Value::Object(m)
}

pub async fn emit_daemon_status(app: &Arc<App>) {
    let herdr_connected = app.connected.load(Ordering::SeqCst);
    app.emit(
        "daemon_status",
        json!({
            "herdr_connected": herdr_connected,
            "default_connected": app.default_connected.load(Ordering::SeqCst),
            "hosts": hosts_json(app).await,
        }),
    )
    .await;
}

/// Update the observed user's default-session link without touching the manager's configured
/// named-session link. The UI uses this separate flag for imported default-session bots.
pub async fn set_default_connected(app: &Arc<App>, connected: bool) {
    if app.default_connected.swap(connected, Ordering::SeqCst) != connected {
        emit_daemon_status(app).await;
    }
}

/// Push `host_changed` plus a refreshed `daemon_status`, and re-lamp that host's bots.
pub async fn emit_host_changed(app: &Arc<App>, conn: &HostConn) {
    let connected = if conn.is_local() { app.connected.load(Ordering::SeqCst) } else { conn.is_connected() };
    // Carry the detection cache along: `spawn_detect` emits this event precisely because the
    // tools / identity answers just changed, and the UI has no other push for them.
    let detected = app.tools.lock().await.get(&conn.name).cloned();
    let mut ev = serde_json::Map::new();
    ev.insert("name".into(), json!(conn.name));
    ev.insert("connected".into(), json!(connected));
    ev.insert("error".into(), json!(conn.error_string().await));
    ev.insert("herdr".into(), crate::herdr_version::for_host(conn, connected, detected.as_ref()));
    // Absent, not null: a client treats a present-but-empty `tools` as "nothing installed".
    if let Some(d) = detected {
        ev.insert("tools".into(), json!(d.tools));
        ev.insert("identities".into(), json!(d.identities));
        ev.insert("shell_identities".into(), json!(d.shell_identities));
        ev.insert("tools_checked_at".into(), json!(d.checked_at));
    }
    app.emit("host_changed", Value::Object(ev)).await;
    emit_daemon_status(app).await;
    for b in crate::db::live_bots_on_host(&app.db, &conn.name).await.unwrap_or_default() {
        app.emit_bot_status(&b.id).await;
    }
}

/// 丟掉 `Child` 不會 wait：行程結束後在 daemon 存活期間留 zombie（#287）。另起 thread 等它，結束就收掉。
pub fn reap_in_background(mut child: std::process::Child) {
    std::thread::spawn(move || {
        if let Err(error) = child.wait() {
            tracing::warn!(?error, "failed waiting for spawned child");
        }
    });
}

/// Ensure the named herdr session's socket is reachable, spawning a headless server if not.
pub async fn ensure_session(session: &str, log_dir: &PathBuf) -> Result<HerdrClient> {
    let sock = HerdrClient::session_socket(session);
    let client = HerdrClient::new(sock.clone());
    if client.ping().await.is_ok() {
        return Ok(client);
    }
    if session == "default" {
        anyhow::bail!("herdr default session is not running; refusing to start the user's session")
    }
    tracing::info!(session, socket = %sock.display(), "herdr socket not reachable; spawning server");
    std::fs::create_dir_all(log_dir).ok();
    let log = std::fs::OpenOptions::new().create(true).append(true).open(log_dir.join("herdr-server.log"))?;
    let errlog = log.try_clone()?;
    let mut cmd = std::process::Command::new("herdr");
    cmd.args(["--session", session, "server"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(errlog));
    // Detach: the herdr server outlives the daemon and is not part of its terminal's
    // foreground process group.
    #[cfg(unix)]
    cmd.process_group(0);
    reap_in_background(cmd.spawn()?);
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        if client.ping().await.is_ok() {
            return Ok(client);
        }
    }
    anyhow::bail!("herdr session `{session}` did not come up within 10s")
}

#[cfg(test)]
mod ws_seq_tests {
    //! WS `seq` vs. the replay ring (review 2026-09-12 f).
    use serde_json::json;

    /// Many emits at once: the ring must hold them in seq order with no gaps, because a
    /// reconnecting client asks for everything after the highest seq it saw. Numbered outside
    /// the ring lock, 6 could land in the ring before 5 and a client that saw 6 never got 5.
    #[tokio::test]
    async fn concurrent_emits_land_in_the_ring_in_seq_order() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let mut tasks = Vec::new();
        for i in 0..200 {
            let app = app.clone();
            tasks.push(tokio::spawn(async move { app.emit("test_event", json!({"i": i})).await }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        let ring = app.backlog(0).await.expect("everything is still in the ring");
        assert_eq!(ring.len(), 200);
        let seqs: Vec<u64> = ring.iter().map(|e| e.seq).collect();
        let mut sorted = seqs.clone();
        sorted.sort_unstable();
        assert_eq!(seqs, sorted, "ring order is seq order");
        assert_eq!(seqs.first().copied(), Some(1));
        assert_eq!(seqs.last().copied(), Some(200));
        assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "no gaps: {seqs:?}");
        assert_eq!(app.current_seq(), 200);
    }
}

/// #243：讀不到 bot 的 host 不能當成本機。
#[cfg(test)]
mod host_unreadable_tests {
    use crate::testing as tt;

    #[tokio::test]
    async fn an_unreadable_host_is_neither_connected_nor_announced_as_local() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
        let mut rx = app.subscribe();
        tt::make_table_unreadable(&app, "projects").await;
        assert!(!app.bot_connected(&bot.id).await);
        app.emit_bot_status(&bot.id).await;
        tt::make_table_readable(&app, "projects").await;
        while let Ok(ev) = rx.try_recv() {
            assert_ne!(ev.kind, "bot_status", "不知道 host 就不能發（會被當成 host=local）：{:?}", ev.data);
        }
        app.emit_bot_status(&bot.id).await;
        assert_eq!(rx.try_recv().unwrap().data["host"], "local", "讀得到才發");
    }
}

#[cfg(test)]
mod reap_tests {
    /// #287：detach 出去的行程結束後不能留 zombie。`ps -o stat=` 對 zombie 印 `Z`，收掉之後 ps 找不到這個 pid。
    #[tokio::test]
    async fn a_detached_child_is_reaped_after_it_exits() {
        let child = std::process::Command::new("/bin/sh").args(["-c", "exit 0"]).spawn().unwrap();
        let pid = child.id();
        super::reap_in_background(child);
        let gone = crate::testing::eventually!(!std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
            .unwrap_or(false));
        assert!(gone, "結束的子行程沒被 wait，留成 zombie");
    }
}
