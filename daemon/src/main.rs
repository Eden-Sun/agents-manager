//! agents-managerd — a local multi-agent manager on top of herdr.
//!
//! Subcommands:
//!   serve                       run the daemon (REST + WS + hook receiver)
//!   hook claude|codex ...       the tiny process agent CLIs invoke; always exits 0

mod build_info;
mod agent_relay;
mod api;
mod identity_kind;
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
mod fork;
mod gh_auth;
mod git_quick;
mod git_sh;
mod github;
mod group;
mod herdr;
mod herdr_shim;
mod hook_cmd;
mod hookrecv;
mod hosts;
mod lifecycle;
mod local_image;
mod memproc;
mod memstat;
mod mission;
mod models;
mod pane_identity;
mod projection;
mod quota;
mod quota_claude;
mod quota_grok;
mod read_marks;
mod reconcile;
mod state;
mod supervisor;
mod supervisor_evidence;
mod startup;
mod statusline_cmd;
#[cfg(test)]
mod testing;
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
        /// 這顆 hook 屬於哪顆 daemon 的資料目錄（daemon 啟動 bot 時寫進 hook.sh）。
        #[arg(long, default_value = "")]
        data_dir: String,
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
        /// 跟 hook 同一套 argv（`lifecycle::setup::hook_cmd_parts_for`）帶進來；statusline 不 spool，用不到。
        /// 不收的話 clap 直接報錯退出，claude 的狀態列整個不見、額度也不回報（2026-09-15 回歸，6e09a2e）。
        #[arg(long, default_value = "", hide = true)]
        data_dir: String,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Hook { provider, bot, token, port, data_dir, payload } => {
            let payload_arg = if provider == "codex" { payload.last().cloned() } else { None };
            hook_cmd::run(hook_cmd::HookArgs { provider, bot, token, port, data_dir, payload_arg });
            std::process::exit(0);
        }
        Cmd::Statusline { bot, token, port, data_dir: _ } => {
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

    // 資料目錄跟著設定檔走，而且**在建立或寫入任何檔案之前**先拿鎖：只換 --config 與 port 的
    // 「隔離測試」曾經打到正式 DB，連 ConfigStore::load 都會先寫一份預設 config（startup.rs）。
    let startup::Instance { dir, cfg_path, store, pool, lock, slug } =
        startup::open_instance(config_path, startup::LOCK_WAIT).await?;
    let _dir_lock = lock;
    startup::set_instance(slug.clone());
    let cfg = store.get().await;
    tracing::info!(config = %cfg_path.display(), data_dir = %dir.display(), instance = slug.as_deref().unwrap_or("default"), listen = %cfg.server.listen, session = %cfg.server.herdr_session, "starting agents-managerd");
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
    // LAN/Tailscale access; binding alone would still 403 non-local peers, `App::allow_lan` relaxes that.
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

    // §11.3: each remote host's supervisor reconciles on connect.
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

    // The daemon never spawns the user's default session; watching it is best-effort.
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

    tools::spawn_detect(app.clone(), config::LOCAL_HOST.to_string());
    // 這顆 binary 是哪一版、什麼時候起來的（`GET /api/supervisor` 的 `last_deploy`）。
    build_info::mark_started();
    tools::spawn_alias_poller(app.clone());
    quota::spawn_codex_poller(app.clone());
    memstat::spawn_poller(app.clone());
    quota_claude::spawn_claude_poller(app.clone());
    quota_grok::spawn_grok_poller(app.clone());
    github::spawn_detect_all(app.clone());
    // 協調者跟巡檢是同一顆總管的兩個角色：舊安裝把它放在自己的專案，開機時併回去（工作目錄仍然分開）。
    if let Err(e) = supervisor::responder::merge_into_manager_project(&app).await {
        tracing::warn!(error = ?e, "AGM 協調者併回巡檢的專案失敗，這一輪維持原樣");
    }
    supervisor::controller::respawn(&app).await;
    supervisor::health::spawn(app.clone());
    // Agent titles have no herdr event, so they are polled.
    events::spawn_title_poller(app.clone());
    tui_prompts::spawn_survey_watcher(app.clone());
    update_watch::spawn_update_watcher(app.clone());
    // SPEC §11.4.4: remote hook spools whose status event never arrived (one ssh per host, 30s).
    hookrecv::spawn_spool_scanner(app.clone());

    {
        // 這一輪只起得了**已經連上**的主機（實務上就是本機）：遠端要先 ssh／launchctl，量級是秒，
        // 而這段在毫秒內就跑完了。遠端那份由 `hosts.rs` 在連上並對帳之後再跑一次
        // （§6.1 第 6 步沒有「遠端除外」這個但書，以前卻從來沒發生過——review 2026-09-16）。
        let app2 = app.clone();
        tokio::spawn(async move { reconcile::autostart_connected(&app2, None).await });
    }

    let router = api::router(app.clone());
    let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("bind {addr}"))?;
    tracing::info!(%addr, "listening");
    // Restarting is how a restart window ends: nothing waits out the old lease (SPEC §18.10).
    supervisor::maintenance::release_restart_on_startup(&app).await;
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

#[allow(dead_code)]
/// LAN is the default for dev runs; only the packaged macOS app stays localhost-only. Security
/// boundary: an .app bundle can't be forgotten like an env var on the many manual restarts.
/// `AM_DEV_LAN=0|1` overrides either way.
fn dev_lan_default(exe: &std::path::Path) -> bool {
    match std::env::var("AM_DEV_LAN").as_deref() {
        Ok("1") => true,
        Ok("0") => false,
        _ => !in_app_bundle(exe),
    }
}

/// `scripts/package-dmg.sh` puts the daemon at `<app>.app/Contents/MacOS/agents-managerd`.
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
