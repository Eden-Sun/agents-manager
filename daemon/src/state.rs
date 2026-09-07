//! Shared daemon state: db pool, herdr client, per-bot locks, WS event bus.

use crate::config::{ConfigStore, LOCAL_HOST};
use crate::herdr::HerdrClient;
use crate::hosts::{HostConn, HostManager};
use anyhow::Result;
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::collections::{HashMap, VecDeque};
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

/// SPEC-team §3: an internal "this turn is no longer in flight" notification.
///
/// Deliberately **not** a WS event: the WS bus is the front end's, with a 200-entry ring
/// buffer and a `resync` escape hatch, which is fine for a UI and useless for a scheduler
/// that must not miss a single turn. Everything that leaves `turns.status = 'in_flight'`
/// (hook match, terminal fallback, stall watchdog, stop, interrupt) goes through
/// `lifecycle::emit_turn`, so publishing there covers every path.
#[derive(Debug, Clone, Serialize)]
pub struct TurnEvent {
    pub bot_id: String,
    pub turn_id: String,
    /// `in_flight` | `completed` | `completed_fallback` | `failed`.
    pub status: String,
    pub delivery: String,
    pub team_id: Option<String>,
    pub team_event_id: Option<String>,
}

impl TurnEvent {
    /// Whether this turn has finished (the only transition a scheduler acts on).
    #[allow(dead_code)]
    pub fn is_done(&self) -> bool {
        self.status != "in_flight"
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
    /// Set only by `cargo dev` (`AM_DEV_LAN=1`): the daemon is bound to every interface for
    /// direct LAN/Tailscale/etc access, so the peer-address and Origin checks accept any peer,
    /// not just loopback. A plain `agents-managerd serve` never sets this and stays
    /// localhost-only.
    pub allow_lan: bool,
    pub connected: std::sync::atomic::AtomicBool,
    pub default_connected: std::sync::atomic::AtomicBool,

    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    bus: broadcast::Sender<WsEvent>,
    /// SPEC-team §3: internal turn-completion bus the team schedulers subscribe to.
    turn_bus: broadcast::Sender<TurnEvent>,
    seq: AtomicU64,
    ring: Mutex<VecDeque<WsEvent>>,
    /// (host, session, pane_id) -> per-run agent_status subscription task
    pub pane_watchers: Mutex<HashMap<(String, String, String), tokio::task::JoinHandle<()>>>,
    /// (host, session) -> global event subscription task
    pub global_watchers: Mutex<HashMap<(String, String), tokio::task::JoinHandle<()>>>,
    /// Serializes discovery/config projection against a default-session event and poll tick.
    pub default_sync_lock: Mutex<()>,
    /// run_id -> pending terminal-fallback timer
    pub fallback_timers: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    /// run_id -> prompt-stall watchdog (delivered but the agent never went `working`)
    pub stall_timers: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    /// run_id -> live-progress poller (streams the partial reply while a turn is in flight)
    pub progress_pollers: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    /// run_id -> when that run last emitted a `turn_progress` frame (API.md: at most 4/s per run).
    /// Kept on `App` rather than inside the poller task so re-arming a poller — a new turn on the
    /// same run — cannot restart the budget and burst.
    pub progress_emitted: Mutex<HashMap<String, std::time::Instant>>,
    /// v4.0: `GET /api/models` cache, key `<host>/<kind>` (10 min TTL).
    pub models_cache: Mutex<HashMap<String, (std::time::Instant, Value)>>,
    /// v4.0: quota per kind key (`codex`, `claude`, `claude:<identity>`).
    pub quotas: Mutex<std::collections::BTreeMap<String, crate::quota::Quota>>,
    /// v4.0: per-host CLI detection (`hosts[].tools`), refreshed on every (re)connect.
    pub tools: Mutex<HashMap<String, crate::tools::HostTools>>,
    /// v4.0: project id -> GitHub origin (`None` = checked, not GitHub).
    pub github: Mutex<HashMap<String, Option<crate::github::GithubInfo>>>,
    /// v4.0: `GET /projects/:id/issues` cache (2 min).
    pub issues_cache: Mutex<HashMap<String, (std::time::Instant, Value)>>,
    /// Submodules per project (`GET /projects/:id/submodules`), 2 min.
    pub submodules_cache: Mutex<HashMap<String, (std::time::Instant, Vec<crate::github::Submodule>)>>,
    /// In-flight GitHub device-flow logins, keyed by host name. Memory only; never persisted.
    pub gh_device: Mutex<HashMap<String, crate::gh_auth::DeviceSession>>,
    /// Plain shells this daemon opened on a host (`POST /hosts/:name/shells`). Memory only:
    /// it doubles as the whitelist for sending keys, and a restart must not inherit one.
    pub host_shells: crate::api::shell::Registry,
}

impl App {
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
            connected: std::sync::atomic::AtomicBool::new(false),
            default_connected: std::sync::atomic::AtomicBool::new(false),
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
            models_cache: Mutex::new(HashMap::new()),
            quotas: Mutex::new(std::collections::BTreeMap::new()),
            tools: Mutex::new(HashMap::new()),
            github: Mutex::new(HashMap::new()),
            issues_cache: Mutex::new(HashMap::new()),
            submodules_cache: Mutex::new(HashMap::new()),
            gh_device: Mutex::new(HashMap::new()),
            host_shells: Default::default(),
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

    /// Resolve a client for an explicit host/session pair. The local default session is a
    /// second socket, not another HostConn, and is intentionally never started by the daemon.
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

    /// Resolve the effective session for a bot. Remote bots always use their host's configured
    /// named session; only local imported bots can point at `default`.
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
        let host = crate::db::bot_host(&self.db, bot_id).await.unwrap_or_else(|_| LOCAL_HOST.to_string());
        let Some(session) = self.session_for_bot(&bot, &host).await else { return false };
        self.session_connected(&host, &session).await
    }

    /// Effective session for an existing run. New runs store it explicitly; old rows inherit
    /// their bot/project session so restarts remain compatible.
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

    /// SPEC-team §3: subscribe to turn completions. There is no ring buffer here — a
    /// subscriber that lags gets `RecvError::Lagged` and must re-read the DB.
    #[allow(dead_code)]
    pub fn subscribe_turns(&self) -> broadcast::Receiver<TurnEvent> {
        self.turn_bus.subscribe()
    }

    /// Publish a turn transition. Sending with no subscribers is not an error.
    pub fn publish_turn(&self, ev: TurnEvent) {
        let _ = self.turn_bus.send(ev);
    }

    /// How many team schedulers are currently listening (for tests / diagnostics).
    #[allow(dead_code)]
    pub fn turn_subscribers(&self) -> usize {
        self.turn_bus.receiver_count()
    }

    pub async fn emit(&self, kind: &str, data: Value) {
        let ev = WsEvent { seq: self.seq.fetch_add(1, Ordering::SeqCst) + 1, kind: kind.to_string(), data };
        {
            let mut ring = self.ring.lock().await;
            ring.push_back(ev.clone());
            while ring.len() > WS_RING {
                ring.pop_front();
            }
        }
        let _ = self.bus.send(ev);
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

    pub fn bot_dir(&self, bot_id: &str) -> PathBuf {
        self.data_dir.join("bots").join(bot_id)
    }

    pub async fn emit_bot_status(&self, bot_id: &str) {
        if let Ok(Some(bot)) = crate::db::bot(&self.db, bot_id).await {
            let run = crate::db::active_run(&self.db, bot_id).await.ok().flatten();
            let host = crate::db::bot_host(&self.db, bot_id).await.unwrap_or_else(|_| LOCAL_HOST.to_string());
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
            // Deprecated alias kept for older clients.
            "connected": herdr_connected,
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
    // Detach: the herdr server outlives the daemon.
    cmd.spawn()?;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        if client.ping().await.is_ok() {
            return Ok(client);
        }
    }
    anyhow::bail!("herdr session `{session}` did not come up within 10s")
}
