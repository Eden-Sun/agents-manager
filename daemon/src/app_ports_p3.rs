//! P3（host／process）的 App 端接縫：把 `hosts`、`shared_host`、`memstat`、`memproc`… 要的窄介面用 `App` 實作出來。
//! `App` 只活在這個檔（composition 層），P3 的模組本身不再 `use crate::state::App`。順序與副作用跟搬走之前完全一樣。

use crate::hosts::{HostFence, HostHooks, HostInstance, HostManager, HostsAccess};
use crate::memstat::{BotRef, MemEnv};
use crate::shared_host::SharedHostEnv;
use crate::state::App;
use serde_json::{json, Value};
use std::future::Future;
use std::sync::Arc;

impl HostsAccess for App {
    fn hosts(&self) -> &HostManager {
        &self.hosts
    }
}

impl HostInstance for App {
    fn instance(&self) -> Option<String> {
        App::instance(self)
    }
}

impl HostHooks for App {
    fn is_shared_host(app: &Arc<Self>, host: &str) -> impl Future<Output = bool> + Send {
        let app = app.clone();
        let host = host.to_string();
        async move { crate::shared_host::is_shared(&app, &host).await }
    }

    fn host_changed(app: &Arc<Self>, fence: &HostFence) -> impl Future<Output = ()> + Send {
        let app = app.clone();
        let fence = fence.clone();
        async move {
            crate::state::emit_host_changed(&app, &fence).await;
        }
    }

    fn set_local_herdr_connected(&self, ok: bool) {
        self.connected.store(ok, std::sync::atomic::Ordering::SeqCst);
    }

    fn host_connected(app: Arc<Self>, host: String) -> impl Future<Output = ()> + Send {
        async move {
            let reconciled = match crate::runners::reconcile::reconcile_host(&app, &host).await {
                Ok(()) => true,
                Err(e) => {
                    tracing::error!(host = %host, error = ?e, "reconcile after connect failed");
                    false
                }
            };
            if reconciled {
                crate::lifecycle::relay_watch::rearm_host(&app, &host).await;
            }
            crate::runners::events::spawn_global_for_host(app.clone(), host.clone()).await;
            crate::runners::hookrecv::replay_host(&app, &host).await;
            crate::tools::spawn_detect(app.clone(), host.clone());
            // 刪除 handler 的一次性 ssh purge 若在送出前 daemon 就死了，這台的已刪 bot 目錄靠連上時再掃一次收掉（#349）。
            crate::runners::remote_purge::spawn_sweep(app.clone(), host.clone());
            // daemon 升級後，長跑的遠端 bot 手上還是舊 shim：連上（重連也一樣）就補版，背景做、不擋連線（issue #124）。
            crate::runners::shim_refresh::spawn_remote_refresh(app.clone(), host.clone());
            // 同一個道理的權限：#494 的收緊在「啟動 bot」那一趟，換版前就在跑的遠端 bot 要等重啟才收得到（issue #501）。
            crate::remote_perms::spawn_tighten(app.clone(), host.clone());
            // 開機那一輪跑的時候這台還沒連上，它的 autostart bot 因此從來沒被起過（review 2026-09-16）。
            // 對帳成功才跑、每台一生一次：重連不能把使用者停掉的 bot 再開起來（core 5）。
            app.wait_until_startup_ready().await;
            crate::runners::reconcile::autostart_after_reconcile(&app, &host, reconciled).await;
        }
    }

    fn forget_host_observations(app: &Arc<Self>, name: &str) -> impl Future<Output = ()> + Send {
        let app = app.clone();
        let name = name.to_string();
        async move { forget_host_observations(&app, &name).await }
    }

    fn host_removed(app: &Arc<Self>, name: &str) -> impl Future<Output = ()> + Send {
        let app = app.clone();
        let name = name.to_string();
        async move {
            forget_host_observations(&app, &name).await;
            app.emit("host_changed", json!({"name": name, "connected": false, "error": "removed"})).await;
            crate::state::emit_daemon_status(&app).await;
        }
    }
}

/// 以主機名為鍵、描述「那台機器」的快取全部丟掉（#347）：偵測結果（`app.tools`，含身分與 herdr CLI 版本）、
/// 額度（`<host>/…`，連重啟快取列）、模型清單（`<host>/<kind>/<identity>`）、這個 daemon 在那台開的 shell 清單。
/// 移除主機與同名改設定都走這裡；新連線上線後由偵測／探測重新填。
async fn forget_host_observations(app: &(impl crate::api::shell::HostShells + crate::capabilities::Db + crate::capabilities::Emit + crate::github::GithubCache + crate::host_baseline::HostBaselineTable + crate::login_assist::LoginPanes + crate::login_assist::LoginReservations + crate::models::ModelsCache + crate::quota::QuotaStaleKeys + crate::quota::QuotaTables + crate::shim_refresh::RemoteShimStale + crate::tools::ToolsTable), name: &str) {
    let prefix = format!("{name}/");
    app.tools().lock().await.remove(name);
    app.host_baseline().lock().await.remove(name);
    app.remote_shim_stale().lock().await.remove(name);
    let removed: Vec<String> = {
        let mut quotas = app.quotas().lock().await;
        let removed = quotas.keys().filter(|k| k.starts_with(&prefix)).cloned().collect();
        quotas.retain(|k, _| !k.starts_with(&prefix));
        removed
    };
    for key in removed {
        crate::quota::forget(app, &key).await;
        // 前端的額度條認 `quota_updated`：不告訴它，那台機器（或換連線前的那條）的數字會掛到下一次輪詢（形狀同 `identity_kind::cleanup_host`）。
        app.emit("quota_updated", json!({"kind": key, "host": name, "quota": null})).await;
    }
    app.models_cache().lock().await.retain(|k, _| !k.starts_with(&prefix));
    app.host_shells().lock().await.retain(|s| s.host != name);
    crate::login_assist::forget_host(app, name);
    // GitHub origin 跟 tools／額度一樣是這台機器的觀測（#830）。改指或刪除時清掉，舊連線的掃描不能再寫回來。
    if let Ok(projects) = crate::db::live_projects(app.db()).await {
        let mut github = app.github().lock().await;
        for project in projects.into_iter().filter(|project| project.host == name) {
            github.remove(&project.id);
        }
    }
}

impl SharedHostEnv for App {
    fn host_flagged_shared(&self, host: &str) -> impl Future<Output = bool> + Send {
        let host = host.to_string();
        async move { self.cfg.get().await.hosts.iter().any(|h| h.name == host && h.shared_session) }
    }

    fn db_pool(&self) -> &sqlx::SqlitePool {
        &self.db
    }

    fn own_shell_panes(&self, host: &str) -> impl Future<Output = Vec<(String, String)>> + Send {
        let host = host.to_string();
        async move { self.host_shells.lock().await.iter().filter(|s| s.host == host).map(|s| (s.workspace_id.clone(), s.pane_id.clone())).collect() }
    }

    fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }
}

impl MemEnv for App {
    fn bot_ref(&self, bot_id: &str) -> impl Future<Output = Option<BotRef>> + Send {
        let bot_id = bot_id.to_string();
        async move {
            crate::db::bot(&self.db, &bot_id)
                .await
                .ok()
                .flatten()
                .map(|b| BotRef { deleted: b.deleted_at.is_some(), name: b.name, project_id: b.project_id })
        }
    }

    fn emit_mem_updated(&self, snapshot: Value) -> impl Future<Output = ()> + Send {
        async move { self.emit("mem_updated", snapshot).await }
    }
}

/// `GET /api/mem/processes/pane` (SPEC §15.2). Any pane herdr knows is readable (no registration
/// check, unlike `shell::read`), but only the last `lines` visible rows as plain text — never keys or input.
pub async fn pane_preview(
    app: &(impl crate::capabilities::HerdrRoutes + crate::hosts::HostsAccess),
    host: &str,
    pane_id: &str,
    socket: Option<&str>,
    lines: u32,
) -> crate::lifecycle::LcResult<Value> {
    use crate::lifecycle::LcError;
    // Another local herdr session's socket: same user's socket, no wider than `herdr` in their shell.
    let client = match socket.filter(|s| !s.is_empty()) {
        Some(path) if host == crate::config::LOCAL_HOST => {
            if !std::path::Path::new(path).exists() {
                return Err(LcError::Upstream(format!("herdr socket `{path}` 不在了")));
            }
            crate::herdr::HerdrClient::new(path)
        }
        Some(_) => {
            return Err(LcError::Bad(
                "遠端主機只能讀它設定的那個 herdr session".into(),
            ))
        }
        None => crate::api::shell::client_for(app, host).await?.0,
    };
    let read = client
        .pane_read(pane_id, "visible", lines)
        .await
        .map_err(|e| LcError::Upstream(format!("{e:#}")))?;
    let (columns, rows) = match client.pane_size(pane_id).await {
        Ok(Some((w, h))) => (Some(w), Some(h)),
        _ => (None, None),
    };
    Ok(json!({
        "host": host, "pane_id": pane_id,
        "source": read.source, "text": read.text, "revision": read.revision, "truncated": read.truncated,
        "columns": columns, "rows": rows,
    }))
}


impl crate::host_baseline::BaselineEnv for App {
    fn baseline_cache(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, crate::host_baseline::BaselineReport>> {
        &self.host_baseline
    }

    fn local_herdr_connected(&self) -> bool {
        self.connected.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn push_ops_alert(&self, key: &str, payload: &Value) -> impl Future<Output = anyhow::Result<bool>> + Send {
        let key = key.to_string();
        let payload = payload.clone();
        async move { Ok(crate::supervisor::store::push_inbox(&self.db, &key, "ops_alert", None, None, None, &payload).await?.is_some()) }
    }
}

impl crate::tools::ToolsEnv for App {
    fn tools_cache(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, crate::tools::HostTools>> {
        &self.tools
    }

    fn config_identities(&self) -> impl Future<Output = Vec<crate::config::IdentityCfg>> + Send {
        async move { self.cfg.get().await.identities.clone() }
    }

    fn clear_login_prompt(&self, host: &str, name: &str) {
        crate::login_prompt::clear(self, host, name);
    }

    fn clear_login_prompt_and_push(app: &Arc<Self>, host: &str, name: &str) -> impl Future<Output = ()> + Send {
        let app = app.clone();
        let (host, name) = (host.to_string(), name.to_string());
        async move { crate::runners::login_prompt::clear_and_push(&app, &host, &name).await }
    }

    fn unpark_claude_identity(&self, host: &str, name: &str, env: &std::collections::BTreeMap<String, String>) {
        crate::quota_claude::unpark_identity(host, name, crate::quota::identity_shares_default("claude", env));
    }

    fn close_login_shell(app: &Arc<Self>, host: &str, pane_id: &str) -> impl Future<Output = ()> + Send {
        let app = app.clone();
        let (host, pane_id) = (host.to_string(), pane_id.to_string());
        async move {
            let _ = crate::api::shell::close(&app, &host, &pane_id).await;
        }
    }

    fn host_tools_installed(app: &Arc<Self>, host: &str) -> impl Future<Output = ()> + Send {
        let app = app.clone();
        let host = host.to_string();
        async move {
            let (app, host) = (&app, host.as_str());
    // 身分表剛更新：清掉 kind 不符的 identity 與它留下的 quota key（`identity_kind::cleanup_host`）。
    crate::identity_kind::cleanup_host(app, host).await;
    // 身分表齊了，重啟前停下的交辦這時才算得出正確的 quota key（每台主機每個行程只跑一次，review 2026-09-16 M3）。
    crate::supervisor::controller::backfill_quota_limits_once(app, host).await;
    // 排著的 prompt 自己記下的撞限（issue #108）：同一個時機、同一個理由。
    crate::lifecycle::quota_hold::backfill_once(app, host).await;
        }
    }
}

/// `POST /api/hosts/:name/tools/install` — goes through the ordinary prompt path (lock, idempotency).
pub async fn install_via_bot(
    app: &Arc<App>,
    host: &str,
    kind: &str,
    via_bot_id: &str,
) -> crate::lifecycle::LcResult<crate::lifecycle::PromptOut> {
    use crate::lifecycle::LcError;
    if !crate::config::valid_kind(kind) {
        return Err(LcError::Bad(format!("kind must be {}", crate::config::kinds_list())));
    }
    if app.hosts.get(host).await.is_none() {
        return Err(LcError::NotFound("host".into()));
    }
    let bot = crate::db::bot(&app.db, via_bot_id)
        .await
        .map_err(|e| LcError::Upstream(e.to_string()))?
        .filter(|b| b.deleted_at.is_none())
        .ok_or_else(|| LcError::NotFound("bot".into()))?;
    let project = crate::db::project(&app.db, &bot.project_id)
        .await
        .map_err(|e| LcError::Upstream(e.to_string()))?
        .filter(|p| p.deleted_at.is_none())
        .ok_or_else(|| LcError::NotFound("project".into()))?;
    if project.host != host {
        return Err(LcError::Bad(format!("bot `{}` lives on host `{}`, not `{host}`", bot.name, project.host)));
    }
    let text = crate::tools::install_prompt(kind).ok_or_else(|| LcError::Bad("unknown kind".into()))?;
    let crid = format!("tools-install:{kind}:{}", crate::db::ulid());
    crate::lifecycle::prompt(app, &bot.id, &text, &crid).await
}


impl crate::trust::TrustEnv for App {
    fn identity_env(app: &Arc<Self>, host: &str, name: &str) -> impl Future<Output = Option<std::collections::BTreeMap<String, String>>> + Send {
        let app = app.clone();
        let (host, name) = (host.to_string(), name.to_string());
        // `identity_for_host`, not `cfg.identities`: shell-discovered `ccN` (SPEC §16) aren't in config.toml.
        async move { crate::tools::identity_for_host(&app, &host, &name).await.map(|i| i.env) }
    }
}

// ───────── am-ports：HostRuntime／HerdrPort ─────────
//
// `SessionId` 在這裡＝herdr session 名稱（`App::session_for_host`／`herdr_for_session` 那個）。`HostFence`（host id＋世代）只描述
// 「哪一條連線」，所以每個動作一開始都先驗世代（`HostManager::conn_at_generation`）：舊世代一律 `Conflict`，不會送到換過的連線。

use am_core::{BotId, HostFence as PortFence, HostId, PaneReadSource, PortError, RunId, SessionId};

fn unavailable(e: impl std::fmt::Display) -> PortError {
    PortError::Unavailable(format!("{e:#}"))
}

fn failed(e: impl std::fmt::Display) -> PortError {
    PortError::Failed(format!("{e:#}"))
}

impl App {
    /// 驗 `fence` 還是這台的當前權威，回那條連線；舊世代 `Conflict`，沒這台 `NotFound`。
    async fn port_conn(&self, fence: &PortFence) -> Result<Arc<crate::hosts::HostConn>, PortError> {
        if let Some(conn) = self.hosts.conn_at_generation(&fence.host_id, fence.generation).await {
            return Ok(conn);
        }
        match self.hosts.get(&fence.host_id).await {
            None => Err(PortError::NotFound(format!("host `{}`", fence.host_id))),
            Some(_) => Err(PortError::Conflict(format!("host `{}` was reconnected or reconfigured; the fence is stale", fence.host_id))),
        }
    }

    /// 驗過世代、再找 `session` 對應的 herdr client。
    #[allow(dead_code)]
    async fn port_client(&self, fence: &PortFence, session: &SessionId) -> Result<crate::herdr::HerdrClient, PortError> {
        self.port_conn(fence).await?;
        self.herdr_for_session(&fence.host_id, session)
            .await
            .ok_or_else(|| PortError::NotFound(format!("herdr session `{session}` on host `{}`", fence.host_id)))
    }
}

/// App 對指定 pane 的讀／寫實作；trait adapter 透過這裡沿用既有 fence 與 herdr client 路徑。
#[allow(dead_code)]
impl App {
    pub(crate) async fn port_read_pane(
        &self,
        fence: &PortFence,
        session: &SessionId,
        pane_id: &str,
        source: PaneReadSource,
        lines: u32,
    ) -> Result<String, PortError> {
        let client = self.port_client(fence, session).await?;
        client.pane_read(pane_id, source.as_str(), lines).await.map(|read| read.text).map_err(unavailable)
    }

    pub(crate) async fn port_send_text_to_pane(&self, fence: &PortFence, session: &SessionId, pane_id: &str, text: &str) -> Result<(), PortError> {
        let client = self.port_client(fence, session).await?;
        client.pane_send_text(pane_id, text).await.map_err(unavailable)
    }
}

fn pane_less(op: &str) -> PortError {
    PortError::InvalidInput(format!("{op} 沒有指定 pane（session 本身不是 pane）；請使用帶 pane_id 的 pane read／send 方法"))
}

impl am_ports::HostRuntime for App {
    fn current_fence<'a>(&'a self, host: &'a HostId) -> impl Future<Output = Result<Option<PortFence>, PortError>> + Send + 'a {
        async move { Ok(self.hosts.current_generation(host).await.map(|generation| PortFence { host_id: host.clone(), generation })) }
    }

    fn session_for_bot<'a>(&'a self, bot: &'a BotId, host: &'a HostId) -> impl Future<Output = Result<Option<SessionId>, PortError>> + Send + 'a {
        async move {
            let Some(bot) = crate::db::bot(&self.db, bot).await.map_err(failed)? else { return Ok(None) };
            Ok(App::session_for_bot(self, &bot, host).await)
        }
    }

    fn session_for_run<'a>(&'a self, run: &'a RunId) -> impl Future<Output = Result<Option<SessionId>, PortError>> + Send + 'a {
        async move {
            let Some(run) = crate::db::run(&self.db, run).await.map_err(failed)? else { return Ok(None) };
            if let Some(session) = run.herdr_session.clone().filter(|s| !s.is_empty()) {
                return Ok(Some(session));
            }
            let host = crate::db::bot_host(&self.db, &run.bot_id).await.map_err(failed)?;
            Ok(self.session_for_host(&host).await)
        }
    }

    fn is_connected<'a>(&'a self, fence: &'a PortFence, session: &'a SessionId) -> impl Future<Output = Result<bool, PortError>> + Send + 'a {
        async move {
            self.port_conn(fence).await?;
            Ok(self.session_connected(&fence.host_id, session).await)
        }
    }
}

impl am_ports::HerdrPort for App {
    fn ping<'a>(&'a self, fence: &'a PortFence) -> impl Future<Output = Result<(), PortError>> + Send + 'a {
        async move {
            let conn = self.port_conn(fence).await?;
            conn.client.ping().await.map(|_| ()).map_err(unavailable)
        }
    }

    fn screen_text<'a>(&'a self, _fence: &'a PortFence, _session: &'a SessionId) -> impl Future<Output = Result<String, PortError>> + Send + 'a {
        async move { Err(pane_less("screen_text")) }
    }

    fn pane_read<'a>(
        &'a self,
        _fence: &'a PortFence,
        _session: &'a SessionId,
        _source: PaneReadSource,
        _lines: u32,
    ) -> impl Future<Output = Result<String, PortError>> + Send + 'a {
        async move { Err(pane_less("pane_read")) }
    }

    fn read_pane<'a>(
        &'a self,
        fence: &'a PortFence,
        session: &'a SessionId,
        pane_id: &'a str,
        source: PaneReadSource,
        lines: u32,
    ) -> impl Future<Output = Result<String, PortError>> + Send + 'a {
        async move { self.port_read_pane(fence, session, pane_id, source, lines).await }
    }

    fn send_text<'a>(&'a self, _fence: &'a PortFence, _session: &'a SessionId, _text: String) -> impl Future<Output = Result<(), PortError>> + Send + 'a {
        async move { Err(pane_less("send_text")) }
    }

    fn send_text_to_pane<'a>(
        &'a self,
        fence: &'a PortFence,
        session: &'a SessionId,
        pane_id: &'a str,
        text: String,
    ) -> impl Future<Output = Result<(), PortError>> + Send + 'a {
        async move { self.port_send_text_to_pane(fence, session, pane_id, &text).await }
    }
}

#[cfg(test)]
mod port_tests {
    use super::*;
    use crate::testing::env;
    use am_ports::{HerdrPort, HostRuntime};

    fn remote_cfg(name: &str) -> crate::config::HostCfg {
        crate::config::HostCfg {
            shared_session: false,
            name: name.into(),
            ssh: format!("{name}.invalid"),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        }
    }

    #[tokio::test]
    async fn the_local_host_has_a_fence_and_an_unknown_host_has_none() {
        let e = env().await;
        let local = e.app.current_fence(&"local".to_string()).await.unwrap().expect("本機一直都在");
        assert_eq!(local.host_id, "local");
        assert!(e.app.current_fence(&"nowhere".to_string()).await.unwrap().is_none());
        assert_eq!(e.app.current_fence(&"local".to_string()).await.unwrap(), Some(local), "不重連世代不變");
    }

    /// 同名主機設定換掉＝換了一條連線：舊 fence 的操作要 `Conflict`，不能送到新連線上（即使新連線的重連計數剛好一樣）。
    #[tokio::test]
    async fn a_fence_from_a_replaced_connection_is_stale_even_when_the_reconnect_counter_matches() {
        let e = env().await;
        let host = "port-fence".to_string();
        e.app.hosts.insert_remote_for_test(remote_cfg(&host)).await;
        let old = e.app.current_fence(&host).await.unwrap().unwrap();
        e.app.hosts.replace_remote_for_test(&e.app, remote_cfg(&host)).await;
        let new = e.app.current_fence(&host).await.unwrap().unwrap();
        assert_ne!(old.generation, new.generation, "換過連線的世代不能撞號");
        let session = "agents-manager".to_string();
        for err in [
            e.app.ping(&old).await.unwrap_err(),
            e.app.is_connected(&old, &session).await.unwrap_err(),
        ] {
            assert!(matches!(err, PortError::Conflict(_)), "{err:?}");
        }
        assert!(e.app.is_connected(&new, &session).await.is_ok(), "新世代照常");
        let gone = PortFence { host_id: "nowhere".into(), generation: 0 };
        assert!(matches!(e.app.ping(&gone).await.unwrap_err(), PortError::NotFound(_)));
    }

    #[tokio::test]
    async fn pane_less_reads_and_writes_are_refused_instead_of_guessing_a_pane() {
        let e = env().await;
        let fence = e.app.current_fence(&"local".to_string()).await.unwrap().unwrap();
        let session = e.app.herdr_session.clone();
        assert!(matches!(e.app.screen_text(&fence, &session).await.unwrap_err(), PortError::InvalidInput(_)));
        assert!(matches!(e.app.pane_read(&fence, &session, PaneReadSource::Visible, 10).await.unwrap_err(), PortError::InvalidInput(_)));
        assert!(matches!(e.app.send_text(&fence, &session, "x".into()).await.unwrap_err(), PortError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn sessions_resolve_from_the_bot_or_the_run_and_a_missing_one_is_none() {
        let e = env().await;
        let bot = crate::testing::claude_bot(&e.app, &e.project_id, "port-session").await;
        let session = HostRuntime::session_for_bot(&*e.app, &bot.id, &"local".to_string()).await.unwrap();
        assert_eq!(session.as_deref(), Some(e.app.herdr_session.as_str()));
        assert_eq!(HostRuntime::session_for_bot(&*e.app, &"no-such-bot".to_string(), &"local".to_string()).await.unwrap(), None);
        assert_eq!(HostRuntime::session_for_run(&*e.app, &"no-such-run".to_string()).await.unwrap(), None);
    }
}
