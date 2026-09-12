//! agents-managerd — a local multi-agent manager on top of herdr.
//!
//! Subcommands:
//!   serve                       run the daemon (REST + WS + hook receiver)
//!   hook claude|codex ...       the tiny process agent CLIs invoke; always exits 0

mod agent_relay;
mod api;
mod assets;
mod attach;
mod bulk_restart;
mod changelog;
mod codex_live;
mod config;
mod capture;
mod default_session;
mod db;
mod events;
mod gh_auth;
mod git_quick;
mod github;
mod group;
mod herdr;
mod herdr_shim;
mod hook_cmd;
mod hookrecv;
mod hosts;
mod lifecycle;
mod memproc;
mod memstat;
mod models;
mod pane_identity;
mod projection;
mod quota;
mod quota_claude;
mod quota_grok;
mod reconcile;
mod state;
mod supervisor;
mod supervisor_evidence;
mod statusline_cmd;
mod team;
mod team_git;
mod team_sched;
mod tools;
mod trust;
mod tui_prompts;
mod turn_error;
mod update_watch;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "agents-managerd", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon.
    Serve {
        /// Override the config path (default ~/.config/agents-manager/config.toml)
        #[arg(long)]
        config: Option<PathBuf>,
        /// M1 aid: also subscribe to agent status for every pane already in the session.
        #[arg(long)]
        dev_watch_all_panes: bool,
    },
    /// Hook callback invoked by the agent CLI. Always exits 0 with empty stdout.
    Hook {
        /// claude | codex | grok (claude and grok deliver the payload on stdin, codex via argv)
        provider: String,
        #[arg(long)]
        bot: String,
        /// Optional; falls back to `$AM_HOOK_TOKEN` (preferred — keeps the token out of `ps`).
        #[arg(long, default_value = "")]
        token: String,
        #[arg(long, default_value_t = 7788)]
        port: u16,
        /// Codex passes the event JSON as the last argv element.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        payload: Vec<String>,
    },
    /// Claude Code statusLine command for daemon-started claude bots (v4.0): reports the
    /// rate limits to the daemon, then runs the user's own statusLine command. Always exits 0.
    Statusline {
        #[arg(long)]
        bot: String,
        /// Optional; falls back to `$AM_HOOK_TOKEN` (preferred — keeps the token out of `ps`).
        #[arg(long, default_value = "")]
        token: String,
        #[arg(long, default_value_t = 7788)]
        port: u16,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Hook { provider, bot, token, port, payload } => {
            let payload_arg = if provider == "codex" { payload.last().cloned() } else { None };
            hook_cmd::run(hook_cmd::HookArgs { provider, bot, token, port, payload_arg });
            std::process::exit(0);
        }
        Cmd::Statusline { bot, token, port } => {
            statusline_cmd::run(statusline_cmd::StatuslineArgs { bot, token, port });
            std::process::exit(0);
        }
        Cmd::Serve { config, dev_watch_all_panes } => {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            if let Err(e) = rt.block_on(serve(config, dev_watch_all_panes)) {
                eprintln!("fatal: {e:?}");
                std::process::exit(1);
            }
        }
    }
}

/// `~/.config/agents-manager`, or `AM_DATA_DIR` when set (a second daemon instance for
/// tests / verification; `hook_cmd.rs` honours the same variable for its spool).
fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("AM_DATA_DIR") {
        let d = PathBuf::from(d);
        if !d.as_os_str().is_empty() {
            return d;
        }
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".config/agents-manager")
}

fn load_or_create_ui_token(dir: &PathBuf) -> Result<String> {
    let path = dir.join("ui-token");
    if let Ok(s) = std::fs::read_to_string(&path) {
        let s = s.trim().to_string();
        if !s.is_empty() {
            return Ok(s);
        }
    }
    let tok = projection::new_token();
    std::fs::write(&path, &tok)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(tok)
}

async fn serve(config_path: Option<PathBuf>, dev_watch_all_panes: bool) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,agents_managerd=debug")),
        )
        .with_target(false)
        .init();

    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;
    let cfg_path = config_path.unwrap_or_else(|| dir.join("config.toml"));
    let store = config::ConfigStore::load(cfg_path.clone()).await?;
    let cfg = store.get().await;
    tracing::info!(config = %cfg_path.display(), listen = %cfg.server.listen, session = %cfg.server.herdr_session, "starting agents-managerd");

    let pool = db::open(&dir.join("agents-manager.sqlite3")).await?;
    projection::project_config(&store, &pool).await.context("project config into sqlite")?;

    let herdr_client = state::ensure_session(&cfg.server.herdr_session, &dir).await?;
    let pong = herdr_client.ping().await?;
    if pong.protocol != herdr::EXPECTED_PROTOCOL {
        tracing::warn!(got = pong.protocol, expected = herdr::EXPECTED_PROTOCOL, "unexpected herdr protocol version");
    }
    tracing::info!(version = %pong.version, protocol = pong.protocol, "herdr ping ok");
    let default_herdr = herdr::HerdrClient::new(herdr::HerdrClient::session_socket(default_session::SESSION));

    let mut addr: std::net::SocketAddr = cfg.server.listen.parse().context("parse server.listen")?;
    let exe = std::env::current_exe()?;
    // Bind every interface instead of just loopback, so a phone or another machine on the
    // LAN/Tailscale reaches this daemon directly. Paired with `App::allow_lan` relaxing the
    // peer and Origin checks below — binding alone would still 403 everything non-local.
    let dev_lan = dev_lan_default(&exe);
    if dev_lan {
        addr.set_ip(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
    }
    let ui_token = load_or_create_ui_token(&dir)?;
    let app = state::App::new(
        pool,
        herdr_client,
        default_herdr,
        store,
        dir.clone(),
        exe,
        addr.port(),
        ui_token,
        cfg.server.herdr_session.clone(),
        dev_lan,
    );
    app.connected.store(true, std::sync::atomic::Ordering::SeqCst);
    if let Some(local) = app.hosts.get("local").await {
        local.connected.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    // §11.3: bring up every configured remote host (each supervisor reconciles on connect).
    app.hosts.apply_config(&app, &cfg.hosts).await;

    // §6.1.3 reconcile, §6.1.4 event connections, §6.1.5 spool replay, §6.1.6 autostart.
    if let Err(e) = reconcile::reconcile_host(&app, config::LOCAL_HOST).await {
        tracing::error!(error = ?e, "initial reconcile failed");
    }
    // Runs are adopted by now, so any Turn that outlived the restart can get its poller back.
    reconcile::rearm_progress(&app).await;
    // #61: directories of bots deleted before every deletion path purged them.
    lifecycle::purge_deleted_bot_dirs(&app).await;
    events::spawn_global(app.clone()).await;

    // Observe the user's default session when it is separate from the manager's named session.
    // A default session is never spawned by the daemon; the event subscription and poller are
    // both best-effort until the user has one running.
    if app.herdr_session != default_session::SESSION {
        if let Err(e) = default_session::sync(&app).await {
            tracing::debug!(session = default_session::SESSION, error = ?e, "initial default session sync skipped");
        }
        events::spawn_global_for_session(app.clone(), config::LOCAL_HOST.to_string(), default_session::SESSION.to_string()).await;
    }
    default_session::spawn_poller(app.clone());

    if dev_watch_all_panes {
        if let Ok(panes) = app.herdr.pane_list(None).await {
            for p in panes {
                events::watch_pane(&app, config::LOCAL_HOST, &p.pane_id).await;
            }
            tracing::info!("dev: watching agent status for all existing panes");
        }
    }

    hookrecv::replay_host(&app, config::LOCAL_HOST).await;

    // v4.0: local CLI detection, codex quota poller (5 min), GitHub origin detection.
    tools::spawn_detect(app.clone(), config::LOCAL_HOST.to_string());
    tools::spawn_alias_poller(app.clone());
    quota::spawn_codex_poller(app.clone());
    memstat::spawn_poller(app.clone());
    quota_claude::spawn_claude_poller(app.clone());
    quota_grok::spawn_grok_poller(app.clone());
    github::spawn_detect_all(app.clone());
    // SPEC-team §7.5: bring back a scheduler for every team that is not in a terminal phase.
    team::respawn_schedulers(&app).await;
    // AGM: pick the supervisor's open assignments and undelivered results back up.
    supervisor::controller::respawn(&app).await;
    supervisor::health::spawn(app.clone());
    // Agent titles (what each agent calls itself) — no herdr event for it, so it polls.
    events::spawn_title_poller(app.clone());
    tui_prompts::spawn_survey_watcher(app.clone());
    update_watch::spawn_update_watcher(app.clone());
    // SPEC §11.4.4: remote hook spools whose status event never arrived (one ssh per host, 30s).
    hookrecv::spawn_spool_scanner(app.clone());

    {
        let app2 = app.clone();
        tokio::spawn(async move {
            for bot in db::live_bots(&app2.db).await.unwrap_or_default() {
                let host = db::bot_host(&app2.db, &bot.id).await.unwrap_or_else(|_| config::LOCAL_HOST.to_string());
                if !app2.host_connected(&host).await {
                    tracing::info!(bot = %bot.name, host, "autostart skipped: host not connected");
                    continue;
                }
                if bot.autostart == 1 && db::active_run(&app2.db, &bot.id).await.ok().flatten().is_none() {
                    tracing::info!(bot = %bot.name, "autostart");
                    if let Err(e) = lifecycle::start_bot(&app2, &bot.id).await {
                        tracing::error!(bot = %bot.name, error = ?e, "autostart failed");
                    }
                }
            }
        });
    }

    let router = api::router(app.clone());
    let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("bind {addr}"))?;
    tracing::info!(%addr, "listening");
    // SPEC §11.3.5: close every ssh master on the way out; remote herdr servers stay alive.
    let shutdown_app = app.clone();
    axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>())
        .with_graceful_shutdown(async move {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
            match term.as_mut() {
                Some(t) => tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = t.recv() => {} },
                None => {
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
            tracing::info!("shutting down; closing ssh masters");
            shutdown_app.hosts.shutdown().await;
        })
        .await?;
    app.hosts.shutdown().await;
    Ok(())
}

// Keep Arc<App> in scope for the type checker in all builds.
#[allow(dead_code)]
/// LAN access is the default for every *dev* run of the daemon, and only the packaged macOS
/// app stays localhost-only.
///
/// It used to be the other way round — off unless `cargo dev` set `AM_DEV_LAN=1` — but the
/// binary is restarted by hand and by other agents dozens of times a day (`cargo build
/// --release && ./target/release/agents-managerd serve`), and every restart that forgot the
/// variable silently dropped the phone and the other machines off `:7788`. A forgotten
/// environment variable is not a security boundary; being *inside an .app bundle* is one the
/// launcher cannot forget, and the shipped app is the only build a non-developer runs.
///
/// `AM_DEV_LAN` still overrides in both directions: `=0` forces loopback for a dev binary,
/// `=1` opens up a bundled one.
fn dev_lan_default(exe: &std::path::Path) -> bool {
    match std::env::var("AM_DEV_LAN").as_deref() {
        Ok("1") => true,
        Ok("0") => false,
        _ => !in_app_bundle(exe),
    }
}

/// `scripts/package-dmg.sh` puts the daemon at `<app>.app/Contents/MacOS/agents-managerd`;
/// nothing else in this repo runs it from such a path.
fn in_app_bundle(exe: &std::path::Path) -> bool {
    let mut dirs = exe.ancestors().skip(1);
    dirs.next().is_some_and(|d| d.file_name().is_some_and(|n| n == "MacOS"))
        && dirs.next().is_some_and(|d| d.file_name().is_some_and(|n| n == "Contents"))
}

fn _assert_send(_: &Arc<state::App>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn bundled_daemon_is_localhost_only() {
        assert!(in_app_bundle(Path::new("/Applications/AG Man.app/Contents/MacOS/agents-managerd")));
    }

    #[test]
    fn dev_binaries_are_not_bundled() {
        for p in [
            "/Users/x/project/agents-manager/target/release/agents-managerd",
            "/Users/x/project/agents-manager/target/debug/agents-managerd",
            "/usr/local/bin/agents-managerd",
            "/Users/x/MacOS/agents-managerd",
        ] {
            assert!(!in_app_bundle(Path::new(p)), "{p} should not look bundled");
        }
    }
}
