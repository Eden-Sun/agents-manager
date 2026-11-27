//! `serve` 子命令：開機流程、UI token、關機收尾（原本在 main.rs）。

use crate::*;
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;

/// UI token 是整個 API 的主憑證（`api::auth`、`cargo_shim` 直接讀這個檔，`outbox` 把它列進「就是憑證」
/// 的檔名黑名單），所以它只能是 0600。issue #512：以前是 `fs::write` 先建一個 0644 的 inode、寫進明文
/// token 之後才 chmod，而且 chmod 失敗被 `let _` 吞掉；既有檔走早退那條路時權限更是連看都不看。
/// 新檔走 `write_private`（0600 的暫存檔 + rename，新內容永遠不進舊 inode）；既有檔開機時修回 0600，
/// 修不動就 warn——不擋開機（token 本身是好的），但不能沉默。
fn load_or_create_ui_token(dir: &PathBuf) -> Result<String> {
    let path = dir.join("ui-token");
    if let Ok(s) = std::fs::read_to_string(&path) {
        let s = s.trim().to_string();
        if !s.is_empty() {
            tighten_ui_token(&path);
            return Ok(s);
        }
    }
    let tok = projection::new_token();
    lifecycle::setup::write_private(&path, tok.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    Ok(tok)
}

/// [`tighten_ui_token`] 這一趟做了什麼（測試看得到，`serve` 只拿它記 log）。
#[derive(Debug, PartialEq, Eq)]
enum Tightened {
    /// 已經是 0600。
    AlreadyPrivate,
    /// 讀不到權限（檔案剛被刪、掛載點沒了）：不猜，交給下一次開機。
    Unknown,
    /// 從 `mode` 改回 0600。
    Repaired(u32),
    /// 過寬但改不動——例如檔案是別人的、或掛在唯讀檔案系統上。
    Failed(u32),
}

/// 既有 ui-token 的權限比 0600 寬就修回來（備份還原、rsync、手動複製都會把它放寬）。
#[cfg(unix)]
fn tighten_ui_token(path: &std::path::Path) {
    fn chmod(p: &std::path::Path, perm: std::fs::Permissions) -> std::io::Result<()> {
        std::fs::set_permissions(p, perm)
    }
    let outcome = tighten_with(path, chmod);
    let owner = file_owner(path);
    match outcome {
        Tightened::AlreadyPrivate | Tightened::Unknown => {}
        Tightened::Repaired(mode) => {
            tracing::warn!(path = %path.display(), found = format!("{mode:o}"), uid = owner, "ui-token 權限過寬，已改回 0600")
        }
        Tightened::Failed(mode) => {
            tracing::warn!(path = %path.display(), found = format!("{mode:o}"), uid = owner, "ui-token 權限過寬且改不回 0600；這把 token 是整個 API 的憑證，請手動 chmod 600")
        }
    }
}

/// `chmod` 抽成參數：失敗那條分支同一個使用者沒辦法用真的檔案系統造出來
/// （chmod 自己的檔案不會失敗），只好從這裡注入（i92b review）。
#[cfg(unix)]
fn tighten_with(
    path: &std::path::Path,
    chmod: impl Fn(&std::path::Path, std::fs::Permissions) -> std::io::Result<()>,
) -> Tightened {
    use std::os::unix::fs::PermissionsExt;
    let Ok(mode) = std::fs::metadata(path).map(|m| m.permissions().mode() & 0o777) else {
        return Tightened::Unknown;
    };
    if mode == 0o600 {
        return Tightened::AlreadyPrivate;
    }
    match chmod(path, std::fs::Permissions::from_mode(0o600)) {
        Ok(()) => Tightened::Repaired(mode),
        Err(_) => Tightened::Failed(mode),
    }
}

#[cfg(unix)]
fn file_owner(path: &std::path::Path) -> String {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).map(|m| m.uid().to_string()).unwrap_or_else(|_| "?".into())
}

#[cfg(not(unix))]
fn tighten_ui_token(_path: &std::path::Path) {}

pub async fn serve(config_path: Option<PathBuf>, dev_watch_all_panes: bool) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,agents_managerd=debug")),
        )
        .with_target(false)
        .init();

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
    let dev_lan = dev_lan_default(&exe);
    if dev_lan {
        addr.set_ip(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
    }
    let host_cfg = cfg.hosts.clone();
    let (listener, app) = startup::bind_then(addr, |listener| async move {
        let ui_token = load_or_create_ui_token(&dir)?;
        let service_tokens = service_auth::load_or_create(&dir)?;
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
        *app.service_tokens.write().expect("service token lock") = service_tokens;
        app.connected.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(local) = app.hosts.get("local").await {
            local.connected.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        Ok::<_, anyhow::Error>((listener, app))
    }).await?;

    app.set_startup_ready(false);
    let router = api::router(app.clone());
    let shutdown_app = app.clone();
    let (enable_shutdown, wait_to_enable_shutdown) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>())
            .with_graceful_shutdown(async move {
                let _ = wait_to_enable_shutdown.await;
                let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
                match term.as_mut() {
                    Some(t) => tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = t.recv() => {} },
                    None => {
                        let _ = tokio::signal::ctrl_c().await;
                    }
                }
                tracing::info!("shutting down; draining daemon background loops");
                shutdown_app.shutdown.cancel();
                shutdown_app.background_tasks.close();
            })
            .await
    });
    tracing::info!(%addr, "HTTP listener bound; startup requests receive 503");
    crate::api_bot::startup_gate::barrier(&app).await;
    lifecycle::recover_live_apply_debts(&app).await;
    if let Err(e) = judge::settle_interrupted(&app).await {
        tracing::warn!(error = ?e, "judge shadow 的占位列收不掉");
    }
    if let Err(e) = quota::load_cache(&app).await {
        tracing::warn!(error = ?e, "quota cache load failed; waiting for the first probe");
    }

    lifecycle::restart_hold::adopt_open_intents(&app).await;
    match crate::primary_keep_warm::rearm_windows(&app.db).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(n, "re-armed keep-warm windows after restart"),
        Err(e) => tracing::warn!(error = ?e, "could not re-arm keep-warm windows; a keep-warm reply may count as activity once"),
    }

    app.hosts.apply_config(&app, &host_cfg).await;
    for h in host_cfg.iter().filter(|h| h.name != config::LOCAL_HOST) {
        match db::rebase_active_run_host_epoch(&app.db, &h.name, am_base::hosts::target_epoch(h)).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(host = %h.name, n, "rebased active runs onto the host target fingerprint"),
            Err(e) => tracing::warn!(host = %h.name, error = ?e, "could not rebase active run host fingerprints"),
        }
    }

    let local_reconciled = match crate::runners::reconcile::reconcile_host(&app, config::LOCAL_HOST).await {
        Ok(()) => true,
        Err(e) => {
            tracing::error!(error = ?e, "initial reconcile failed");
            false
        }
    };
    crate::runners::reconcile::rearm_progress(&app).await;
    match lifecycle::late_proof::recover_at_startup(&app).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(n, "re-armed late delivery proof watchers after restart"),
        Err(e) => tracing::warn!(error = ?e, "could not restore late delivery proof watchers"),
    }
    let sweep_app = app.clone();
    app.background_tasks.spawn(async move {
        let mut interval = tokio::time::interval(lifecycle::late_proof::SWEEP_EVERY);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = sweep_app.shutdown.cancelled() => break,
                _ = interval.tick() => {
                    if let Err(e) = lifecycle::late_proof::sweep(&sweep_app).await {
                        tracing::warn!(error = ?e, "late delivery proof cleanup failed; a later sweep will retry");
                    }
                }
            }
        }
    });
    lifecycle::relay_watch::rearm_host(&app, config::LOCAL_HOST).await;
    cli_update::recover_at_startup(&app).await;
    update_chain::recover_at_startup(&app).await;
    lifecycle::purge_deleted_bot_dirs(&app).await;
    runners::shim_refresh::refresh_at_startup(&app).await;
    match lifecycle::grok_hook::heal_at_startup(&app, None) {
        Ok(true) => tracing::info!("repaired the grok hooks file at startup"),
        Ok(false) => {}
        Err(e) => tracing::warn!(error = ?e, "could not check the grok hooks file at startup"),
    }
    match lifecycle::agy_hook::heal_at_startup(&app) {
        Ok(true) => tracing::info!("repaired the agy hooks at startup"),
        Ok(false) => {}
        Err(e) => tracing::warn!(error = ?e, "could not check the agy hooks at startup"),
    }
    attach::reconcile_orphans(&app).await;
    attach::spawn_sweep(app.clone());
    crate::lifecycle::agy_images_collect::spawn(app.clone());
    preview::reconcile_all(&app).await;
    crate::runners::events::spawn_global(app.clone()).await;

    if app.herdr_session != default_session::SESSION {
        if let Err(e) = default_session::sync(&app).await {
            tracing::debug!(session = default_session::SESSION, error = ?e, "initial default session sync skipped");
        }
        crate::runners::events::spawn_global_for_session(app.clone(), config::LOCAL_HOST.to_string(), default_session::SESSION.to_string()).await;
    }
    runners::default_session::spawn_poller(app.clone());

    if dev_watch_all_panes {
        if let Ok(panes) = app.herdr.pane_list(None).await {
            for p in panes {
                crate::runners::events::watch_pane(&app, config::LOCAL_HOST, &p.pane_id).await;
            }
            tracing::info!("dev: watching agent status for all existing panes");
        }
    }

    runners::hookrecv::replay_host(&app, config::LOCAL_HOST).await;

    tools::spawn_detect(app.clone(), config::LOCAL_HOST.to_string());
    build_info::mark_started(&app.data_dir);
    tools::spawn_alias_poller(app.clone());
    host_baseline::spawn_poller(app.clone());
    crate::runners::herdr_version::spawn_poller(app.clone());
    crate::runners::remote_purge::spawn_poller(app.clone());
    crate::schedules::spawn_poller(app.clone());
    crate::runners::autostart_revive::spawn_pane_gone_sweeper(app.clone());
    remote_perms::spawn_poller(app.clone());
    bot_trash::spawn_gc(app.clone());
    crate::runners::quota::spawn_codex_poller(app.clone());
    crate::codex_reset::spawn(app.clone());
    memstat::spawn_poller(app.clone());
    runners::db_pool::spawn(app.clone());
    runners::db_optimize::spawn(app.clone());
    runners::background_stuck::spawn(app.clone());
    crate::runners::quota_claude::spawn_claude_poller(app.clone());
    runners::quota_grok::spawn_grok_poller(app.clone());
    runners::quota_agy::spawn_agy_poller(app.clone());
    runners::quota_agy::spawn_agy_login_watcher(app.clone());
    runners::quota_agy::spawn_agy_identity_poller(app.clone());
    runners::github::spawn_detect_all(app.clone());
    if let Err(e) = supervisor::responder::merge_into_manager_project(&app).await {
        tracing::warn!(error = ?e, "AGM 協調者併回巡檢的專案失敗，這一輪維持原樣");
    }
    match supervisor::setup::strip_remote_control_args(&app).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(n, "removed --remote-control from supervisor bot args; restart them to apply"),
        Err(e) => tracing::warn!(error = ?e, "could not remove --remote-control from supervisor bot args"),
    }
    supervisor::cli_refresh::refresh_on_startup(&app).await;
    crate::runners::supervisor::runtime::respawn(&app).await;
    crate::runners::supervisor::health::spawn(app.clone());
    crate::webpush::spawn_incident_notifier(app.clone());
    crate::api_bot::spawn_retention_ticker(app.clone());
    crate::api_bot::spawn_sweeper(app.clone());
    crate::runners::events::spawn_title_poller(app.clone());
    crate::webpush::spawn_main_notifier(app.clone());
    crate::message_pretranslate::spawn(app.clone());
    crate::webpush::spawn_extra_notifiers(app.clone());
    crate::runners::events::spawn_status_repair(app.clone());
    runners::tui_prompts::spawn_survey_watcher(app.clone());
    lifecycle::spawn_stuck_turn_sweeper(app.clone());
    crate::runners::update_watch::spawn_update_watcher(app.clone());
    crate::runners::upstream_update::spawn(app.clone());
    crate::herdr_integration::spawn(app.clone());
    crate::claude_session_watch::spawn(app.clone());
    runners::hook_inbox::spawn_worker(app.clone());
    runners::hookrecv::spawn_spool_scanner(app.clone());
    panes::spawn_scanner(app.clone());
    crate::runners::build_scheduler::spawn_sweeper(app.clone());
    crate::runners::build_counts::spawn_push(app.clone());

    supervisor::maintenance::release_restart_on_startup(&app).await;
    crate::runners::herdr_maintenance::arm_on_startup(&app).await;
    deploy_wait::startup(&app).await;
    deploy_wait::spawn_ticker(&app);
    app.set_startup_ready(true);
    {
        let app2 = app.clone();
        tokio::spawn(async move { crate::runners::reconcile::autostart_after_reconcile(&app2, config::LOCAL_HOST, local_reconciled).await });
    }
    tracing::info!(%addr, "listening");
    share::portal::spawn_listener(&app, addr.port()).await;
    crate::runners::share_budget::spawn_ticker(&app);
    crate::runners::reset_credit_expiry::spawn_ticker(&app);
    crate::claude_console_balance::spawn_ticker(&app);
    crate::openai_console_balance::spawn_ticker(&app);
    let _ = enable_shutdown.send(());
    {
        let app2 = app.clone();
        crate::background_loop::spawn_named(&app.background_tasks, "share outbox keeper", async move {
            tokio::select! {
                _ = app2.shutdown.cancelled() => {}
                _ = share::keep_share_outboxes(&app2) => {}
            }
        });
    }
    let serve_result = match join_server_within_shutdown(server, app.shutdown.clone(), HTTP_SHUTDOWN_WAIT)
        .await
        .context("HTTP server task panicked")?
    {
        Some(result) => result,
        None => Ok(()),
    };
    finish_serve(&app, serve_result).await?;
    Ok(())
}

/// 關機時等被追蹤的背景工作的上限（#1463）。會卡住的（等 bot 鎖、遠端 ssh）逾時就放棄，記下名字、繼續關機，
/// 不靠 systemd 的 TimeoutStopSec 強殺。被放棄的工作都能安全中斷：下次開機由 reconcile 與 spool 補齊（SPEC §6.1、§4.4）。
const BACKGROUND_SHUTDOWN_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// 關機時 HTTP server 等進行中請求的上限（#2036）。請求卡在 DB 池等連線（死結）時，`with_graceful_shutdown` 會一直等，
/// 關機就卡住；逾時就 abort，照常進 `finish_serve`。
const HTTP_SHUTDOWN_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

/// 等 HTTP server 結束。關機 token 取消後最多再等 `wait`，仍沒結束就 abort 並回 `Ok(None)`（#2036）。
/// abort 之後的 `JoinError::Cancelled` 視為正常關機；其他 JoinError（panic）照原樣回傳。
async fn join_server_within_shutdown<T: Send + 'static>(
    mut server: tokio::task::JoinHandle<T>,
    shutdown: tokio_util::sync::CancellationToken,
    wait: std::time::Duration,
) -> Result<Option<T>, tokio::task::JoinError> {
    let abort = server.abort_handle();
    tokio::select! {
        res = &mut server => return res.map(Some),
        () = async {
            shutdown.cancelled().await;
            tokio::time::sleep(wait).await;
        } => {}
    }
    tracing::warn!(?wait, "shutdown: in-flight HTTP requests did not finish within the wait limit; aborting");
    abort.abort();
    match server.await {
        Ok(value) => Ok(Some(value)),
        Err(e) if e.is_cancelled() => Ok(None),
        Err(e) => Err(e),
    }
}

async fn finish_serve<E>(app: &Arc<state::App>, serve_result: Result<(), E>) -> Result<(), E> {
    app.shutdown.cancel();
    app.background_tasks.close();
    if tokio::time::timeout(BACKGROUND_SHUTDOWN_WAIT, app.background_tasks.wait()).await.is_err() {
        let left = crate::background_loop::unfinished_names();
        tracing::warn!(?left, "shutdown: background tasks still running after the wait limit; giving up on them and continuing");
    }
    app.hosts.shutdown().await;
    serve_result
}

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

#[cfg(test)]
mod tests {
    /// #2036：HTTP server 在關機時卡住（請求不結束）→ 等滿上限後 abort，回 `Ok(None)`，不掛住關機。
    #[tokio::test(start_paused = true)]
    async fn http_server_that_never_drains_is_aborted_after_the_shutdown_wait() {
        let shutdown = tokio_util::sync::CancellationToken::new();
        let server = tokio::spawn(std::future::pending::<std::io::Result<()>>());
        shutdown.cancel();
        let out = tokio::time::timeout(std::time::Duration::from_secs(120), join_server_within_shutdown(server, shutdown, HTTP_SHUTDOWN_WAIT)).await;
        assert!(out.is_ok(), "關機不能等一個永遠不結束的 HTTP 請求（#2036）");
        assert!(matches!(out.unwrap(), Ok(None)), "逾時 abort 後要當成正常關機");
    }

    /// #2036 對照組：上限內結束的請求照常收完，結果原樣回傳，不被 abort。
    #[tokio::test(start_paused = true)]
    async fn http_server_that_drains_within_the_shutdown_wait_is_not_aborted() {
        let shutdown = tokio_util::sync::CancellationToken::new();
        let server = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            Ok::<(), std::io::Error>(())
        });
        shutdown.cancel();
        let out = join_server_within_shutdown(server, shutdown, HTTP_SHUTDOWN_WAIT).await;
        assert!(matches!(out, Ok(Some(Ok(())))), "上限內結束的請求要照常回傳");
    }

    /// #1286：signal handler 不能排在遠端 ssh 後面：`enable_shutdown.send` 要早於 `keep_share_outboxes`，
    /// 而且後者不能在 `serve()` 裡被 `.await`（遠端慢的主機要好幾分鐘）。
    #[test]
    fn signal_handlers_are_enabled_before_the_remote_share_keep_pass() {
        let src = include_str!("serve.rs");
        let body = &src[..src.find("\nmod tests {").expect("tests module")];
        let enable = body.find("enable_shutdown.send(())").expect("enable_shutdown.send");
        let keep = body.find("keep_share_outboxes(").expect("keep_share_outboxes");
        assert!(enable < keep, "enable_shutdown.send 要在 keep_share_outboxes 之前");
        assert!(!body.contains("keep_share_outboxes(&app).await;"), "不能在 serve() 裡 inline await");
    }
    use super::*;
    use std::path::Path;

    #[tokio::test]
    async fn an_early_server_error_cancels_and_joins_supervisor_background_work() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let task_starts = starts.clone();
        let shutdown = app.shutdown.clone();
        crate::background_loop::spawn_restartable(&app, "shutdown test loop", move || {
            let starts = task_starts.clone();
            let shutdown = shutdown.clone();
            async move {
                starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                shutdown.cancelled().await;
            }
        });
        assert!(crate::testing::eventually!(starts.load(std::sync::atomic::Ordering::SeqCst) == 1));

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            finish_serve(&app, Err(std::io::Error::other("injected accept failure"))),
        )
        .await;
        assert!(result.is_ok(), "server failure must cancel background tasks before waiting for them");
        assert!(result.unwrap().is_err(), "the original server failure must still be returned");
        assert_eq!(app.background_tasks.len(), 0, "shutdown must join tracked work");
    }

    /// #1463：關機的等待有上限（預設 30 秒）。一個永遠不結束的背景任務（例如卡在 bot 鎖或遠端 ssh 上）不能讓優雅關機不結束，
    /// 否則只能靠 systemd 的 TimeoutStopSec 強殺。用虛擬時間：這裡等的是邏輯時間，不靠牆鐘。
    #[tokio::test]
    async fn a_background_task_that_never_finishes_does_not_hold_up_graceful_shutdown() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        tokio::time::pause();
        app.background_tasks.spawn(std::future::pending::<()>());

        let out = tokio::time::timeout(std::time::Duration::from_secs(120), finish_serve(&app, Ok::<(), std::io::Error>(()))).await;
        assert!(out.is_ok(), "關機不能等一個永遠不結束的背景任務（#1463）");
        assert!(out.unwrap().is_ok(), "關機的結果照原樣回傳");
    }

    /// #1463 對照組：上限內結束的背景任務要照常跑完，不被截斷。
    #[tokio::test]
    async fn a_background_task_that_finishes_within_the_limit_is_not_cut_short() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        tokio::time::pause();
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = done.clone();
        app.background_tasks.spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(20)).await;
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        finish_serve(&app, Ok::<(), std::io::Error>(())).await.unwrap();
        assert!(done.load(std::sync::atomic::Ordering::SeqCst), "上限內結束的任務要跑完，不能被截斷");
    }

    /// #1463：關機放棄的工作要記得名字。逾時 log 用的是同一份名單（`unfinished_names`）。
    #[tokio::test]
    async fn a_background_task_given_up_at_shutdown_is_named_in_the_list_the_log_uses() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        tokio::time::pause();
        crate::background_loop::spawn_named(&app.background_tasks, "finish-serve-test-stuck", std::future::pending::<()>());

        finish_serve(&app, Ok::<(), std::io::Error>(())).await.unwrap();
        assert!(crate::background_loop::unfinished_names().contains(&"finish-serve-test-stuck"), "放棄的工作要列得出名字");
    }

    /// issue #512：新建的 ui-token 一開始就要是 0600（不留 `write` 與 `chmod` 之間那段 0644 的窗口），
    /// 既有的過寬檔案開機時要被修回來，而且內容不能被換掉（token 本身是好的）。
    #[cfg(unix)]
    #[test]
    fn the_ui_token_is_owner_only_when_created_and_repaired_when_found_too_open() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-ui-token-{}", db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ui-token");

        let fresh = load_or_create_ui_token(&dir).unwrap();
        assert!(!fresh.is_empty());
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600, "新建的就要是 0600");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let again = load_or_create_ui_token(&dir).unwrap();
        assert_eq!(again, fresh, "既有 token 不可以被換掉");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600, "過寬的要被修回 0600");

        std::fs::write(&path, "   \n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        let regenerated = load_or_create_ui_token(&dir).unwrap();
        assert_ne!(regenerated, fresh);
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// i92b review：chmod 失敗那條分支同一個使用者沒辦法用真的檔案系統造出來（chmod 自己的檔案
    /// 不會失敗），注入一個一定失敗的 chmod 來驗——重點是**不 panic、不換 token、如實回報 Failed**。
    #[cfg(unix)]
    #[test]
    fn a_chmod_that_cannot_run_is_reported_not_swallowed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-ui-token-chmod-{}", db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ui-token");
        std::fs::write(&path, "tok").unwrap();
        fn boom(_: &Path, _: std::fs::Permissions) -> std::io::Result<()> {
            Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "nope"))
        }

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(tighten_with(&path, boom), Tightened::Failed(0o644), "改不動要說改不動");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "tok", "修權限不可以動到內容");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(tighten_with(&path, boom), Tightened::AlreadyPrivate);

        std::fs::remove_file(&path).unwrap();
        assert_eq!(tighten_with(&path, boom), Tightened::Unknown);

        std::fs::remove_dir_all(&dir).ok();
    }

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
