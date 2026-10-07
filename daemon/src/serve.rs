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
    // 包一層具名 fn：直接傳泛型的 `std::fs::set_permissions` 會被實例化成某個固定 lifetime，
    // 滿足不了 `for<'a> Fn(&'a Path, ..)`。
    fn chmod(p: &std::path::Path, perm: std::fs::Permissions) -> std::io::Result<()> {
        std::fs::set_permissions(p, perm)
    }
    let outcome = tighten_with(path, chmod);
    // uid 一起記：過寬的 token 誰讀得到，看檔案是誰的最直接（i92b review）。
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
                // Install signal handlers after initialization; during boot the API returns 503.
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
    lifecycle::recover_live_apply_debts(&app).await;
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
    app.hosts.apply_config(&app, &host_cfg).await;

    // §6.1.3 reconcile, §6.1.4 event connections, §6.1.5 spool replay, §6.1.6 autostart.
    let local_reconciled = match crate::runners::reconcile::reconcile_host(&app, config::LOCAL_HOST).await {
        Ok(()) => true,
        Err(e) => {
            tracing::error!(error = ?e, "initial reconcile failed");
            false
        }
    };
    // Runs are adopted by now, so any Turn that outlived the restart can get its poller back.
    crate::runners::reconcile::rearm_progress(&app).await;
    lifecycle::relay_watch::rearm_host(&app, config::LOCAL_HOST).await;
    // #564：上一顆 daemon 沒收尾的 codex 安裝，等那台的安裝鎖放掉再收尾（不重跑安裝）。
    cli_update::recover_at_startup(&app).await;
    // #61: directories of bots deleted before every deletion path purged them.
    lifecycle::purge_deleted_bot_dirs(&app).await;
    // shim 只在 bot 啟動時寫，長跑的 bot 會抱著舊版好幾天（2026-09-18 的 shim 巢狀死鎖就是這樣
    // 在修正上線後還在發生）。開機就地換成這顆 binary 帶的版本，不必重啟任何 pane。
    runners::shim_refresh::refresh_at_startup(&app).await;
    // grok 的全域 hook 檔若指到別的資料目錄／已刪的腳本（測試或別顆 daemon 寫的），開機就修回來；沒有檔就不建。
    match lifecycle::grok_hook::heal_at_startup(&app, None) {
        Ok(true) => tracing::info!("repaired the grok hooks file at startup"),
        Ok(false) => {}
        Err(e) => tracing::warn!(error = ?e, "could not check the grok hooks file at startup"),
    }
    // #88: attachments whose save() died mid-write or mid-finalize before this restart.
    attach::reconcile_orphans(&app).await;
    // 上傳了卻從沒送出的附件（`ready`、沒有訊息引用）依保留期清掉；開機一次、之後每 6 小時。
    attach::spawn_sweep(app.clone());
    // 預覽（§6.12）：pane 還在不在、port 有沒有在 listen，對回 `bot_previews`。
    preview::reconcile_all(&app).await;
    crate::runners::events::spawn_global(app.clone()).await;

    // The daemon never spawns the user's default session; watching it is best-effort.
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

    hookrecv::replay_host(&app, config::LOCAL_HOST).await;

    tools::spawn_detect(app.clone(), config::LOCAL_HOST.to_string());
    // 這顆 binary 是哪一版、什麼時候**上線**的（`GET /api/supervisor` 的 `last_deploy`）。
    build_info::mark_started(&app.data_dir);
    tools::spawn_alias_poller(app.clone());
    host_baseline::spawn_poller(app.clone());
    crate::runners::herdr_version::spawn_poller(app.clone());
    crate::runners::remote_purge::spawn_poller(app.clone());
    // 連上那趟收權限失敗的主機（#501）：欠著的每 5 分鐘補跑一次，不然一台不重連的主機就一直是 0755（#501 複看）。
    remote_perms::spawn_poller(app.clone());
    // #406：回收區不能只在開機收，常駐好幾天會一路長。
    bot_trash::spawn_gc(app.clone());
    crate::runners::quota::spawn_codex_poller(app.clone());
    memstat::spawn_poller(app.clone());
    crate::runners::quota_claude::spawn_claude_poller(app.clone());
    runners::quota_grok::spawn_grok_poller(app.clone());
    runners::quota_agy::spawn_agy_poller(app.clone());
    runners::quota_agy::spawn_agy_login_watcher(app.clone());
    runners::github::spawn_detect_all(app.clone());
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
    crate::runners::events::spawn_title_poller(app.clone());
    runners::tui_prompts::spawn_survey_watcher(app.clone());
    lifecycle::spawn_stuck_turn_sweeper(app.clone());
    crate::runners::update_watch::spawn_update_watcher(app.clone());
    // issue #707：claude／codex 上游有新版、磁碟上還沒有（跟上面的「重啟套用」分開）。
    crate::runners::upstream_update::spawn(app.clone());
    // SPEC §11.4.4: remote hook spools whose status event never arrived (one ssh per host, 30s).
    runners::hook_inbox::spawn_worker(app.clone());
    hookrecv::spawn_spool_scanner(app.clone());
    // §6.5e：pane 裡開始跑 dev server 沒有任何 herdr 事件，對帳又不定期跑；表上的 kind／port 靠這個跟上。
    panes::spawn_scanner(app.clone());
    // issue #90：名額持有者沒續約（掛了、被砍）就收回，不必等下一個人來要才發現。
    crate::runners::build_scheduler::spawn_sweeper(app.clone());

    // Restarting is how a restart window ends: nothing waits out the old lease (SPEC §18.10).
    supervisor::maintenance::release_restart_on_startup(&app).await;
    // 上一顆 daemon 開的 herdr 維護窗口：沒到期就重新排截止，過期就當場收尾（§6.5.2）。
    crate::runners::herdr_maintenance::arm_on_startup(&app).await;
    // 上一顆 daemon 等換版窗口等到一半（或就是這次換版）：讀回來，換好了／回滾了要告訴使用者（SPEC §18.10）。
    deploy_wait::startup(&app).await;
    deploy_wait::spawn_ticker(&app);
    app.set_startup_ready(true);
    {
        // 這一輪只起本機：遠端一律由 `hosts.rs` 在那台連上並對帳成功之後跑。
        // API 先 ready，bot 啟動時寫入的 hooks／relay 才不會撞上 startup 503。
        let app2 = app.clone();
        tokio::spawn(async move { crate::runners::reconcile::autostart_after_reconcile(&app2, config::LOCAL_HOST, local_reconciled).await });
    }
    tracing::info!(%addr, "listening");
    // 分享入口（SPEC §20）：`[share] listen` 有設才開，獨立的 port 與 router，跟上面的管理 API 完全分開。
    share::portal::spawn_listener(&app, addr.port()).await;
    share::keep_share_outboxes(&app).await;
    let _ = enable_shutdown.send(());
    // 連線收完後才等受監督的迴圈，再關 SSH masters。
    let serve_result = server.await.context("HTTP server task panicked")?;
    finish_serve(&app, serve_result).await?;
    Ok(())
}

async fn finish_serve<E>(app: &Arc<state::App>, serve_result: Result<(), E>) -> Result<(), E> {
    app.shutdown.cancel();
    app.background_tasks.close();
    app.background_tasks.wait().await;
    app.hosts.shutdown().await;
    serve_result
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

        // 備份還原／rsync 把它放寬：開機時修回來，token 不變。
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let again = load_or_create_ui_token(&dir).unwrap();
        assert_eq!(again, fresh, "既有 token 不可以被換掉");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600, "過寬的要被修回 0600");

        // 空檔（寫到一半死掉）會重新產生，而且新檔照樣 0600，不沿用舊 inode 的權限。
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
        // 具名 fn 而不是 closure：closure 推不出 `for<'a> Fn(&'a Path, ..)`，會撞
        // 「implementation of `Fn` is not general enough」。
        fn boom(_: &Path, _: std::fs::Permissions) -> std::io::Result<()> {
            Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "nope"))
        }

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(tighten_with(&path, boom), Tightened::Failed(0o644), "改不動要說改不動");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "tok", "修權限不可以動到內容");

        // 已經是 0600 就根本不叫 chmod（叫了就會炸在 boom 上）。
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(tighten_with(&path, boom), Tightened::AlreadyPrivate);

        // 檔案不見了：不猜、不報錯。
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
