//! Shared daemon state: db pool, herdr client, per-bot locks, WS event bus.

use crate::config::ConfigStore;
use crate::herdr::HerdrClient;
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

pub struct App {
    pub db: SqlitePool,
    pub herdr: HerdrClient,
    pub cfg: ConfigStore,
    pub data_dir: PathBuf,
    pub exe: PathBuf,
    pub port: u16,
    pub ui_token: String,
    pub herdr_session: String,
    pub connected: std::sync::atomic::AtomicBool,

    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    bus: broadcast::Sender<WsEvent>,
    seq: AtomicU64,
    ring: Mutex<VecDeque<WsEvent>>,
    /// pane_id -> per-run agent_status subscription task
    pub pane_watchers: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    /// run_id -> pending terminal-fallback timer
    pub fallback_timers: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
}

impl App {
    pub fn new(
        db: SqlitePool,
        herdr: HerdrClient,
        cfg: ConfigStore,
        data_dir: PathBuf,
        exe: PathBuf,
        port: u16,
        ui_token: String,
        herdr_session: String,
    ) -> Arc<Self> {
        let (bus, _) = broadcast::channel(1024);
        Arc::new(Self {
            db,
            herdr,
            cfg,
            data_dir,
            exe,
            port,
            ui_token,
            herdr_session,
            connected: std::sync::atomic::AtomicBool::new(false),
            locks: Mutex::new(HashMap::new()),
            bus,
            seq: AtomicU64::new(0),
            ring: Mutex::new(VecDeque::new()),
            pane_watchers: Mutex::new(HashMap::new()),
            fallback_timers: Mutex::new(HashMap::new()),
        })
    }

    /// Per-bot mutex. start / stop / prompt / hook matching / spool replay / reconcile all take it.
    pub async fn bot_lock(&self, bot_id: &str) -> Arc<Mutex<()>> {
        let mut g = self.locks.lock().await;
        g.entry(bot_id.to_string()).or_insert_with(|| Arc::new(Mutex::new(()))).clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<WsEvent> {
        self.bus.subscribe()
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
            self.emit(
                "bot_status",
                json!({
                    "bot_id": bot.id,
                    "run": run,
                    "connected": self.connected.load(Ordering::SeqCst),
                }),
            )
            .await;
        }
    }
}

/// Ensure the named herdr session's socket is reachable, spawning a headless server if not.
pub async fn ensure_session(session: &str, log_dir: &PathBuf) -> Result<HerdrClient> {
    let sock = HerdrClient::session_socket(session);
    let client = HerdrClient::new(sock.clone());
    if client.ping().await.is_ok() {
        return Ok(client);
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
