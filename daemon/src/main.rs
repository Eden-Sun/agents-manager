//! agents-managerd — a local multi-agent manager on top of herdr.
//!
//! Subcommands:
//!   serve                       run the daemon (REST + WS + hook receiver)
//!   hook claude|codex ...       the tiny process agent CLIs invoke; always exits 0

mod bot_trash;
mod build_info;
mod build_scheduler;
mod cargo_shim;
mod agent_relay;
mod api;
mod pending_question;
mod identity_kind;
mod intents;
mod restart_intents;
mod delete_intents;
mod launch_rev;
mod promote_intents;
mod assets;
mod attach;
mod bulk_restart;
mod changelog;
mod claude_review;
mod codex_update;
mod child_alerts;
mod child_reconcile_safety;
mod child_retire;
mod codex_live;
mod config;
mod config_audit;
mod capture;
mod dangerous_rm;
mod default_session;
mod db;
mod due_actions;
mod events;
mod fork;
mod fork_ops;
mod remote_purge;
mod remote_perms;
mod remote_trash;
mod promote;
mod gh_auth;
mod git_quick;
mod git_sh;
mod github;
mod group;
mod herdr;
mod herdr_shim;
mod herdr_maintenance;
mod herdr_update;
mod herdr_version;
mod hook_cmd;
mod hook_inbox;
mod judge;
mod hookrecv;
mod hosts;
mod kind_probe;
mod lifecycle;
mod local_image;
mod local_sh;
mod memproc;
mod memstat;
mod outbox;
mod mission;
mod models;
mod pane_identity;
mod preview;
mod preview_bind;
mod primary_order;
mod pane_probe;
mod shim_path;
mod shim_refresh;
mod panes;
mod private_files;
mod projection;
mod quota;
mod quota_claude;
mod release_triage;
mod quota_grok;
mod read_marks;
mod rewind;
mod remote_cargo;
mod remote_health;
mod reconcile;
mod relay_auth;
mod spawn_hints;
mod state;
mod supervisor;
mod supervisor_owned;
mod supervisor_evidence;
mod startup;
mod statusline_cmd;
#[cfg(test)]
mod testing;
#[cfg(test)]
mod timestamp_compat_tests;
mod tools;
mod trust;
mod trusted_open;
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
    /// herdr 有沒有新版、對我們有沒有影響（issue #66）：純函式，不碰網路、不執行任何指令，也不會
    /// 觸發升級。三個輸入都由呼叫端（`scripts/ops/herdr-update-kick.sh`）自己去問；這裡只保證版本
    /// 比較是數值比較（不是字串比較，`457dd14` 那個 `0.9.0` 判成比 `0.10.0` 新的坑），CHANGELOG
    /// 段落擷取沒抓漏。JSON 印到 stdout；exit code：0＝比較成功（不論有沒有更新），2＝版本號看不懂。
    HerdrUpdateCheck {
        /// `herdr --version` 讀到的本機版本。
        #[arg(long)]
        installed: String,
        /// GitHub release／Homebrew 查到的最新穩定版。
        #[arg(long)]
        latest: String,
        /// herdr 的 CHANGELOG 全文所在檔案。
        #[arg(long)]
        changelog_file: PathBuf,
        /// 上次真的派過工的版本（`herdr-update.last` 記的那個），沒有就省略。
        #[arg(long)]
        last_notified: Option<String>,
    },
    /// 上游新版分診（issue #204）：抓 claude／codex 的 changelog，把 `(帳本已分診的最大版本, 最新正式版]`
    /// 每一版切成逐條 entry、用 `release_triage/rules.toml` 分桶、記進帳本，JSON 印到 stdout（`--json`；
    /// 契約 `{kind,from,to,pending:[{version,kept,unmatched,dropped_count}]}`）。抓不到 feed 時 exit 1，
    /// 這一輪不做（不當成沒有新版）。第一次跑只記磁碟版本當基準、不回 pending。
    ReleaseTriageCheck {
        /// claude | codex
        #[arg(long)]
        kind: String,
        /// 補歷史：從這一版（不含）起算，忽略帳本裡的最大版本。
        #[arg(long)]
        since: Option<String>,
        /// 輸出 JSON（目前唯一格式，旗標留給 kick 腳本明示）。
        #[arg(long)]
        json: bool,
        /// 指定「磁碟上的版本」，省得第一次跑時去問 login shell（測試用）。
        #[arg(long, hide = true)]
        installed: Option<String>,
        /// 帳本所在的 SQLite（預設 `AM_DATA_DIR`／預設資料目錄底下的 agents-manager.sqlite3）。
        #[arg(long, hide = true)]
        db: Option<PathBuf>,
        /// 不抓網路，直接讀這個 feed 檔（測試用）。
        #[arg(long, hide = true)]
        feed_file: Option<PathBuf>,
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
    /// issue #104：cargo shim 的本機 helper；讀設定／密碼後把 verification 整個丟到外部 SSH 主機。
    RemoteCargo {
        #[arg(long)]
        config: PathBuf,
        /// 沒給就從 `--config` 推（issue #417：`scripts/check.sh` 會清掉 `AM_DATA_DIR`）。
        #[arg(long)]
        data_dir: Option<PathBuf>,
        #[arg(long)]
        cwd: PathBuf,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        cargo_args: Vec<String>,
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
        Cmd::RemoteCargo { config, data_dir, cwd, cargo_args } => {
            std::process::exit(remote_cargo::run_cli(&config, data_dir.as_deref(), &cwd, &cargo_args));
        }
        Cmd::HerdrUpdateCheck { installed, latest, changelog_file, last_notified } => {
            let md = std::fs::read_to_string(&changelog_file).unwrap_or_else(|e| {
                eprintln!("讀不了 {}: {e}", changelog_file.display());
                std::process::exit(2);
            });
            let Some(report) = herdr_update::build_report(&installed, &latest, &md) else {
                eprintln!("看不懂版本號：installed=`{installed}` latest=`{latest}`");
                std::process::exit(2);
            };
            let should_notify = herdr_update::should_notify(&report, last_notified.as_deref());
            let brief = should_notify.then(|| herdr_update::render_agm_brief(&report));
            println!(
                "{}",
                serde_json::json!({
                    "installed_version": report.installed_version,
                    "latest_version": report.latest_version,
                    "has_update": report.has_update,
                    "should_notify": should_notify,
                    "brief": brief,
                })
            );
            std::process::exit(0);
        }
        Cmd::ReleaseTriageCheck { kind, since, json: _, installed, db, feed_file } => {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            match rt.block_on(release_triage::run_check(release_triage::CheckArgs { kind, since, installed, db, feed_file })) {
                Ok(report) => {
                    println!("{}", serde_json::to_string(&report).expect("serialize CheckReport"));
                    std::process::exit(0);
                }
                Err(e) => {
                    // 結構化錯誤放 stdout（kick 讀 stdout），exit 1 照舊：不是「沒有新版」。
                    println!("{}", serde_json::json!({"error": format!("{e:#}")}));
                    eprintln!("release-triage-check: {e:#}");
                    std::process::exit(1);
                }
            }
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
        startup::open_instance(config_path, startup::env_dir()?, startup::LOCK_WAIT).await?;
    let _dir_lock = lock;
    startup::set_instance(slug.clone());
    let cfg = store.get().await;
    tracing::info!(config = %cfg_path.display(), data_dir = %dir.display(), instance = slug.as_deref().unwrap_or("default"), listen = %cfg.server.listen, session = %cfg.server.herdr_session, "starting agents-managerd");
    projection::project_config_at_startup(&store, &pool, projection::bulk_delete_allowed_by_env())
        .await
        .context("project config into sqlite")?;

    let herdr_client = state::ensure_session(&cfg.server.herdr_session, &dir).await?;
    let pong = herdr_client.ping().await?;
    if !herdr::protocol_supported(pong.protocol) {
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
    // #392：先把上一次額度讀數放回記憶體，API 開始服務時就能畫出 stale 的量表；新的探測回來後由 quota::set 蓋掉。
    // 上一輪在「占位」與「補答案」之間被收掉時留下的 pending 列，收成終態（issue #481／i339 review）。
    if let Err(e) = judge::settle_interrupted(&app).await {
        tracing::warn!(error = ?e, "judge shadow 的占位列收不掉");
    }
    if let Err(e) = quota::load_cache(&app).await {
        tracing::warn!(error = ?e, "quota cache load failed; waiting for the first probe");
    }

    // #378: 被打斷的重啟在 recovery 補完之前，對帳收尾舊 run 不能把它排著的派工當孤兒撤掉。
    lifecycle::restart_hold::adopt_open_intents(&app).await;

    // §11.3: each remote host's supervisor reconciles on connect.
    app.hosts.apply_config(&app, &cfg.hosts).await;

    // §6.1.3 reconcile, §6.1.4 event connections, §6.1.5 spool replay, §6.1.6 autostart.
    let local_reconciled = match reconcile::reconcile_host(&app, config::LOCAL_HOST).await {
        Ok(()) => true,
        Err(e) => {
            tracing::error!(error = ?e, "initial reconcile failed");
            false
        }
    };
    // Runs are adopted by now, so any Turn that outlived the restart can get its poller back.
    reconcile::rearm_progress(&app).await;
    // #61: directories of bots deleted before every deletion path purged them.
    lifecycle::purge_deleted_bot_dirs(&app).await;
    // shim 只在 bot 啟動時寫，長跑的 bot 會抱著舊版好幾天（2026-09-18 的 shim 巢狀死鎖就是這樣
    // 在修正上線後還在發生）。開機就地換成這顆 binary 帶的版本，不必重啟任何 pane。
    shim_refresh::refresh_at_startup(&app.data_dir);
    // #88: attachments whose save() died mid-write or mid-finalize before this restart.
    attach::reconcile_orphans(&app).await;
    // 預覽（§6.12）：pane 還在不在、port 有沒有在 listen，對回 `bot_previews`。
    preview::reconcile_all(&app).await;
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
    // 這顆 binary 是哪一版、什麼時候**上線**的（`GET /api/supervisor` 的 `last_deploy`）。
    build_info::mark_started(&app.data_dir);
    tools::spawn_alias_poller(app.clone());
    herdr_version::spawn_poller(app.clone());
    remote_purge::spawn_poller(app.clone());
    // 連上那趟收權限失敗的主機（#501）：欠著的每 5 分鐘補跑一次，不然一台不重連的主機就一直是 0755（#501 複看）。
    remote_perms::spawn_poller(app.clone());
    // #406：回收區不能只在開機收，常駐好幾天會一路長。
    bot_trash::spawn_gc(app.clone());
    quota::spawn_codex_poller(app.clone());
    memstat::spawn_poller(app.clone());
    quota_claude::spawn_claude_poller(app.clone());
    quota_grok::spawn_grok_poller(app.clone());
    github::spawn_detect_all(app.clone());
    // 協調者跟巡檢是同一顆總管的兩個角色：舊安裝把它放在自己的專案，開機時併回去（工作目錄仍然分開）。
    if let Err(e) = supervisor::responder::merge_into_manager_project(&app).await {
        tracing::warn!(error = ?e, "AGM 協調者併回巡檢的專案失敗，這一輪維持原樣");
    }
    // 換版後已安裝的 bin/agm 跟著換（只動 bin/agm；寫不進去記 warn＋inbox，不擋開機，SPEC §18.2a）。
    supervisor::cli_refresh::refresh_on_startup(&app).await;
    supervisor::controller::respawn(&app).await;
    supervisor::health::spawn(app.clone());
    // 交辦改狀態時補推 mission_updated，任務卡才跟得上（review3 c1 M5）。
    mission::relay::spawn(app.clone());
    // Agent titles have no herdr event, so they are polled.
    events::spawn_title_poller(app.clone());
    tui_prompts::spawn_survey_watcher(app.clone());
    lifecycle::spawn_stuck_turn_sweeper(app.clone());
    update_watch::spawn_update_watcher(app.clone());
    // SPEC §11.4.4: remote hook spools whose status event never arrived (one ssh per host, 30s).
    hook_inbox::spawn_worker(app.clone());
    hookrecv::spawn_spool_scanner(app.clone());
    // §6.5e：pane 裡開始跑 dev server 沒有任何 herdr 事件，對帳又不定期跑；表上的 kind／port 靠這個跟上。
    panes::spawn_scanner(app.clone());
    // issue #90：名額持有者沒續約（掛了、被砍）就收回，不必等下一個人來要才發現。
    build_scheduler::spawn_sweeper(app.clone());

    {
        // 這一輪只起本機：遠端一律由 `hosts.rs` 在那台連上並對帳成功之後跑（§6.1 第 6 步沒有「遠端除外」這個但書）。
        // 以前這裡是「全部已連上的主機」，剛好先連上、但對帳失敗的遠端會被這一輪照樣起 bot（review 2026-09-16 core 5）。
        let app2 = app.clone();
        tokio::spawn(async move { reconcile::autostart_after_reconcile(&app2, config::LOCAL_HOST, local_reconciled).await });
    }

    let router = api::router(app.clone());
    let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("bind {addr}"))?;
    tracing::info!(%addr, "listening");
    // Restarting is how a restart window ends: nothing waits out the old lease (SPEC §18.10).
    supervisor::maintenance::release_restart_on_startup(&app).await;
    // 上一顆 daemon 開的 herdr 維護窗口：沒到期就重新排截止，過期就當場收尾（§6.5.2）。
    herdr_maintenance::arm_on_startup(&app).await;
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
