use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, Result};
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use futures::future::BoxFuture;
use serde_json::{json, Value};
use tokio::sync::Mutex as AsyncMutex;

use crate::changelog;
use crate::state::App;
use crate::upstream_update::{
    codex_latest, grok_latest, herdr_latest, item_json, last_path, npm_latest, tick,
    Sources, GROK_STABLE_URL, HERDR_RELEASES_API, NPM_CLAUDE_LATEST, SWEEP,
};

const ATTEMPTS_FILE: &str = "auto-update-attempts.json";

pub fn attempts_path(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join(ATTEMPTS_FILE)
}

pub fn load_attempts(path: &std::path::Path) -> std::collections::HashMap<String, String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_attempts(path: &std::path::Path, attempts: &std::collections::HashMap<String, String>) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(attempts)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn has_attempted(path: &std::path::Path, host: &str, kind: &str, target: &str) -> bool {
    let map = load_attempts(path);
    map.get(&format!("{host}:{kind}")).map(String::as_str) == Some(target)
}

pub fn record_attempt(path: &std::path::Path, host: &str, kind: &str, target: &str) {
    let mut map = load_attempts(path);
    map.insert(format!("{host}:{kind}"), target.to_string());
    if let Err(e) = save_attempts(path, &map) {
        tracing::warn!(host, kind, target, error = %e, "auto_update: 寫不進 auto-update-attempts.json");
    }
}

pub fn clear_attempt(path: &std::path::Path, host: &str, kind: &str) {
    let mut map = load_attempts(path);
    if map.remove(&format!("{host}:{kind}")).is_some() {
        let _ = save_attempts(path, &map);
    }
}

pub async fn send_auto_update_failure(app: &Arc<App>, host: &str, kind: &str, reason: &str) {
    tracing::warn!(host, kind, reason, "auto_update: 自動安裝失敗");
    let text = format!("{host} 的 {kind} 自動安裝失敗：{reason}");
    let ev = json!({
        "kind": kind,
        "host": host,
        "has_update": true,
        "notify": "error",
        "error_kind": "auto_update",
        "text": text,
        "auto_update": true,
    });
    app.emit("upstream_update", ev).await;
}

async fn auto_install_one(app: &Arc<App>, host: &str, kind: &str, target: &str) {
    match kind {
        "claude" | "codex" => {
            let res = crate::cli_update::start(
                app,
                &axum::http::HeaderMap::new(),
                host,
                Some(kind),
                Some(target),
                Arc::new(crate::cli_update::Real),
            )
            .await;
            match res {
                Ok(_) => {
                    tracing::info!(host, kind, target, "auto_update: cli_update started");
                }
                Err(crate::lifecycle::LcError::Conflict(_)) => {
                    tracing::info!(host, kind, target, "auto_update: cli_update conflict, skipping");
                }
                Err(e) => {
                    send_auto_update_failure(app, host, kind, &format!("{e:?}")).await;
                }
            }
        }
        "agy" => {
            match crate::agy_install::install(app, host).await {
                Ok(done) => {
                    tracing::info!(host, to = %done.to, "auto_update: agy installed successfully");
                    crate::upstream_update::note_installed(app.as_ref(), "agy", host, &done.to).await;
                }
                Err(crate::agy_install::InstallError::Busy(_)) => {
                    tracing::info!(host, "auto_update: agy install busy, skipping");
                }
                Err(e) => {
                    send_auto_update_failure(app, host, "agy", &format!("{e:?}")).await;
                }
            }
        }
        "grok" => {
            let fence = match app.hosts.fence(host).await {
                Some(f) => f,
                None => {
                    send_auto_update_failure(app, host, "grok", "主機不存在或連線中斷").await;
                    return;
                }
            };
            let timeout = Duration::from_secs(300);
            let exec_res = if fence.conn().is_local() {
                let wrapped = "exec 2>&1\ngrok update";
                match crate::hosts::sh_local(wrapped, timeout).await {
                    Ok(Some(out)) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).to_string()),
                    Ok(Some(out)) => Err(format!("exit {}：{}", out.status, String::from_utf8_lossy(&out.stderr).trim())),
                    Ok(None) => Err("超過 300 秒逾時".to_string()),
                    Err(e) => Err(format!("{e:#}")),
                }
            } else {
                fence.conn().ssh_exec_path_timeout("grok update", timeout).await.map_err(|e| format!("{e:#}"))
            };

            match exec_res {
                Ok(_) => {
                    tracing::info!(host, "auto_update: grok update succeeded");
                    if let Ok(ht) = crate::tools::detect(app, host).await {
                        if let Some(t) = ht.tools.get("grok").and_then(|t| t.version.as_deref()) {
                            crate::upstream_update::note_installed(app.as_ref(), "grok", host, t).await;
                        }
                    }
                }
                Err(err) => {
                    send_auto_update_failure(app, host, "grok", &err).await;
                }
            }
        }
        _ => {}
    }
}

pub async fn tick_and_auto_install(app: &Arc<App>) -> Vec<Value> {
    let src = Live(app.clone());
    let path = last_path(app.as_ref());
    let auto_cfg = app.cfg.updates_fresh().await;
    let evs = tick(app.as_ref(), &app.upstream_watch, &src, &path, &auto_cfg.auto).await;
    for ev in &evs {
        crate::changelog_translate::kick_detected(app, ev);
    }
    run_auto_installs(app, &auto_cfg).await;
    evs
}

async fn run_auto_installs(app: &Arc<App>, auto_cfg: &am_config::UpdatesCfg) {
    let snapshot = app.upstream_watch.snapshot().await;
    let attempts_file = attempts_path(&app.data_dir);

    for status in snapshot {
        let kind = &status.kind;
        if !auto_cfg.is_auto(kind) {
            continue;
        }
        if !status.has_update {
            continue;
        }
        let Some(target) = status.target_version.as_deref().or(status.latest_version.as_deref()) else {
            continue;
        };

        for host_disk in &status.hosts {
            if !host_disk.behind {
                continue;
            }
            let host = &host_disk.host;

            // 被 kind 停用的主機不更新
            if crate::identity_prefs::kind_disabled(&app.db, host, kind).await {
                continue;
            }

            // 同一 (host, kind, target) 只嘗試一次
            if has_attempted(&attempts_file, host, kind, target) {
                continue;
            }

            // 記下嘗試記錄（即使失敗或重啟也不重試同一版）
            record_attempt(&attempts_file, host, kind, target);

            auto_install_one(app, host, kind, target).await;
        }
    }
}

/// Shared authoritative release discovery for the watcher and scheduled triage CLI.
/// Changelog publication can lag behind these sources.
pub async fn fetch_latest(kind: &str) -> Result<String> {
    if kind == "agy" {
        return crate::agy_install::latest_version(&crate::agy_install::Real).await
            .map_err(|e| anyhow!("讀不到官方 manifest：{e}"));
    }
    let url = match kind {
        "claude" => NPM_CLAUDE_LATEST,
        "codex" => changelog::feed_url("codex").expect("codex Releases source").0,
        "herdr" => HERDR_RELEASES_API,
        "grok" => GROK_STABLE_URL,
        _ => return Err(anyhow!("{kind} 沒有上游版本來源")),
    };
    let client = reqwest::Client::builder().user_agent("agents-manager")
        .connect_timeout(Duration::from_secs(10)).timeout(Duration::from_secs(20))
        .build().map_err(|e| anyhow!("http client: {e}"))?;
    let response = changelog::send_get_with_retry(&client, url, &format!("{kind} 上游版本"), &changelog::HTTP_RETRY_DELAYS).await?;
    if !response.status().is_success() { return Err(anyhow!("{kind} 上游版本回 HTTP {}", response.status())); }
    let raw = response.text().await.map_err(|e| anyhow!("讀 {kind} 上游版本回應失敗：{e}"))?;
    match kind {
        "claude" => npm_latest(&raw),
        "grok" => grok_latest(&raw),
        _ => herdr_latest(&raw), // GitHub Releases (also handles codex's rust-v tags).
    }
}

pub struct Live(pub Arc<App>);

impl Sources for Live {
    fn upstream<'a>(&'a self, kind: &'a str) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            if kind == "codex" {
                return codex_latest(&changelog::fetch_changelog(&self.0, "codex").await?);
            }
            fetch_latest(kind).await
        })
    }

    fn hosts<'a>(&'a self, kind: &'a str) -> BoxFuture<'a, Vec<String>> {
        Box::pin(async move {
            // 先拿主機清單再鎖 tools：不要拿著 tools 鎖去等別的鎖。
            let conns = self.0.hosts.list().await;
            let mut disabled_hosts = std::collections::HashSet::new();
            if kind != "herdr" {
                for c in &conns {
                    if crate::identity_prefs::kind_disabled(&self.0.db, &c.name, kind).await {
                        disabled_hosts.insert(c.name.clone());
                    }
                }
            }
            let tools = self.0.tools.lock().await;
            let mut out = Vec::new();
            for c in conns {
                if disabled_hosts.contains(&c.name) {
                    continue;
                }
                let installed = match tools.get(&c.name) {
                    Some(t) if kind == "herdr" => t.herdr_cli.is_some(),
                    Some(t) => t.tools.get(kind).is_some_and(|t| t.installed),
                    None => false,
                };
                if installed {
                    out.push(c.name.clone());
                }
            }
            out
        })
    }

    fn installed<'a>(&'a self, host: &'a str, kind: &'a str) -> BoxFuture<'a, Result<String>> {
        if kind == "herdr" {
            return Box::pin(async move {
                let cli = self.0.tools.lock().await.get(host).and_then(|t| t.herdr_cli.clone()).ok_or_else(|| anyhow!("讀不到 `herdr --version`"))?;
                // 同機常有兩份 herdr（launchd 跑的 server 與 login shell／shim 的 CLI 是不同檔，#1752）：
                // 只看 CLI 會把「CLI 新、server 舊」當成已是最新。server 只在連著時才有可信的版本。
                let server = match self.0.hosts.get(host).await {
                    Some(conn) => {
                        let connected = if conn.is_local() { self.0.connected.load(std::sync::atomic::Ordering::SeqCst) } else { conn.is_connected() };
                        connected.then(|| conn.client.last_pong()).flatten().map(|p| p.version)
                    }
                    None => None,
                };
                Ok(older_herdr_version(cli, server.as_deref()))
            });
        }
        Box::pin(changelog::installed_version(&self.0, host, kind))
    }
}

/// server 與 CLI 版本不同時取較舊的那份；讀不到 server 或看不懂版本就用 CLI 的。
fn older_herdr_version(cli: String, server: Option<&str>) -> String {
    let cli_v = changelog::cli_version_string(&cli).and_then(|v| changelog::parse_version(&v));
    let server_v = server.and_then(changelog::cli_version_string).and_then(|v| changelog::parse_version(&v));
    match (cli_v, server_v) {
        (Some(c), Some(sv)) if sv < c => server.map(str::to_string).unwrap_or(cli),
        _ => cli,
    }
}

#[cfg(test)]
mod herdr_installed_tests {
    use super::older_herdr_version;

    /// #1752：server 0.9.4／CLI 0.9.3 與反過來，都以較舊的那份為「磁碟版本」，畫面才會繼續提示更新。
    #[test]
    fn the_older_of_server_and_cli_is_the_installed_version() {
        assert_eq!(older_herdr_version("herdr 0.9.3".into(), Some("0.9.4")), "herdr 0.9.3");
        assert_eq!(older_herdr_version("herdr 0.9.4".into(), Some("0.9.3")), "0.9.3");
        assert_eq!(older_herdr_version("herdr 0.9.4".into(), Some("herdr 0.9.4")), "herdr 0.9.4");
        assert_eq!(older_herdr_version("herdr 0.9.4".into(), None), "herdr 0.9.4");
        assert_eq!(older_herdr_version("herdr 0.9.4".into(), Some("garbage")), "herdr 0.9.4");
    }
}

static TICK_MUTEX: OnceLock<AsyncMutex<()>> = OnceLock::new();

pub(crate) fn tick_mutex() -> &'static AsyncMutex<()> {
    TICK_MUTEX.get_or_init(|| AsyncMutex::new(()))
}

/// 重算快照用的來源：沿用 Watch 既有的快取上游版本，避免重新抓取網路。
pub struct CachedLive(pub Live);

impl Sources for CachedLive {
    fn upstream<'a>(&'a self, kind: &'a str) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            // 看 Watch 有沒有快取的 upstream：若有，直接沿用（不重抓網路）
            if let Some((_, v, _)) = self.0.0.upstream_watch.upstream.lock().await.get(kind) {
                return Ok(v.clone());
            }
            // 若快取無紀錄但既有快照有 latest_version，沿用快照版本
            if let Some(status) = self.0.0.upstream_watch.snapshot().await.into_iter().find(|s| s.kind == kind) {
                if let Some(latest) = status.latest_version {
                    return Ok(latest);
                }
            }
            Err(anyhow!("{kind} 尚未快取上游版本"))
        })
    }

    fn hosts<'a>(&'a self, kind: &'a str) -> BoxFuture<'a, Vec<String>> {
        self.0.hosts(kind)
    }

    fn installed<'a>(&'a self, host: &'a str, kind: &'a str) -> BoxFuture<'a, Result<String>> {
        self.0.installed(host, kind)
    }
}

pub fn spawn(app: Arc<App>) {
    // 開機先等工具探測跑完（第一輪延後 60 秒），不然第一輪一台主機都沒有。
    crate::background_loop::spawn_periodic(&app, "upstream update watcher", SWEEP, Duration::from_secs(60), |app| async move {
        tick_and_auto_install(&app).await;
    });
}

/// 停用／啟用 kind 後手動重算一次 upstream update 快照並廣播。
pub async fn refresh_now(app: &Arc<App>) {
    let _guard = tick_mutex().lock().await;
    let auto_cfg = app.cfg.updates_fresh().await;
    let src = CachedLive(Live(app.clone()));
    let path = last_path(app.as_ref());
    for ev in tick(app.as_ref(), &app.upstream_watch, &src, &path, &auto_cfg.auto).await {
        crate::changelog_translate::kick_detected(app, &ev);
    }
}

pub fn routes() -> Router<Arc<App>> {
    Router::new().route("/upstream-updates", get(get_status))
}

/// `GET /api/upstream-updates`：最近一輪的快照，不觸發抓取。第一輪還沒跑完是空陣列。
async fn get_status(State(app): State<Arc<App>>) -> Json<Value> {
    let items: Vec<Value> = app.upstream_watch.snapshot().await.iter().map(item_json).collect();
    Json(json!({ "items": items }))
}

#[cfg(test)]
mod disabled_kind_tests {
    use super::*;

    /// 停用的 CLI（kind_prefs disabled）在 upstream_update 的 hosts 篩選排除，但 herdr 不受影響；啟用後重新列出。
    #[tokio::test]
    async fn disabled_kind_is_excluded_from_upstream_hosts() {
        let env = crate::testing::env().await;
        let mut tools = std::collections::BTreeMap::new();
        tools.insert("grok".to_string(), crate::tools::ToolInfo {
            installed: true,
            path: Some("/usr/bin/grok".into()),
            version: Some("1.0.77".into()),
            logged_in: Some(true),
        });
        let ht = crate::tools::HostTools {
            tools,
            identities: Default::default(),
            shell_identities: vec![],
            utc_offset_secs: None,
            herdr_cli: Some("herdr 0.9.3".into()),
            checked_at: crate::db::now(),
        };
        env.app.tools.lock().await.insert("local".into(), ht);

        let live = Live(env.app.clone());

        // 預設未停用：grok 與 herdr 都在 hosts 清單
        let grok_hosts = live.hosts("grok").await;
        assert!(grok_hosts.contains(&"local".to_string()), "預設應含 local: {grok_hosts:?}");
        let herdr_hosts = live.hosts("herdr").await;
        assert!(herdr_hosts.contains(&"local".to_string()), "herdr 應含 local: {herdr_hosts:?}");

        // 停用 local 的 grok
        crate::identity_prefs::put_kind_disabled(
            State(env.app.clone()),
            axum::extract::Path("grok".into()),
            Json(crate::identity_prefs::KindDisableIn {
                host: Some("local".into()),
                disabled: true,
            }),
        )
        .await
        .unwrap();

        // 停用後：grok 排除 local，herdr 依然包含 local
        let grok_hosts = live.hosts("grok").await;
        assert!(!grok_hosts.contains(&"local".to_string()), "停用後不應含 local: {grok_hosts:?}");
        let herdr_hosts = live.hosts("herdr").await;
        assert!(herdr_hosts.contains(&"local".to_string()), "herdr 不受影響: {herdr_hosts:?}");

        // 重新啟用 grok
        crate::identity_prefs::put_kind_disabled(
            State(env.app.clone()),
            axum::extract::Path("grok".into()),
            Json(crate::identity_prefs::KindDisableIn {
                host: Some("local".into()),
                disabled: false,
            }),
        )
        .await
        .unwrap();

        // 啟用後：grok 重新出現
        let grok_hosts = live.hosts("grok").await;
        assert!(grok_hosts.contains(&"local".to_string()), "啟用後重新含 local: {grok_hosts:?}");
    }

    /// CachedLive 沿用 Watch 快取的上游版本，避免重新抓取網路。
    #[tokio::test]
    async fn cached_live_sources_uses_watch_upstream_without_network() {
        let env = crate::testing::env().await;
        // Watch.upstream 有快取時，CachedLive 直接回傳快取版本
        env.app.upstream_watch.upstream.lock().await.insert(
            "grok".into(),
            (std::time::Instant::now(), "1.0.88".into(), crate::db::now()),
        );
        let src = CachedLive(Live(env.app.clone()));
        let ver = src.upstream("grok").await.unwrap();
        assert_eq!(ver, "1.0.88");

        // Watch.upstream 無快取但 snapshot 有時，沿用 snapshot 版本
        let status = crate::upstream_update::build_status("codex", &Ok("0.99.0".into()), &[], None);
        crate::upstream_update::set_snapshot_for_test(&env.app.upstream_watch, status).await;
        let ver2 = src.upstream("codex").await.unwrap();
        assert_eq!(ver2, "0.99.0");

        // 兩者皆無時回傳 Err，不連網
        assert!(src.upstream("claude").await.is_err());
    }
}

#[cfg(test)]
mod auto_update_tests {
    use super::*;

    #[test]
    fn attempts_tracking_only_tries_once_until_new_target_or_cleared() {
        let dir = crate::testing::scratch_dir("auto-update-test");
        let path = attempts_path(&dir);

        assert!(!has_attempted(&path, "local", "codex", "0.158.0"));
        record_attempt(&path, "local", "codex", "0.158.0");
        assert!(has_attempted(&path, "local", "codex", "0.158.0"));

        // 新版本不視為已嘗試
        assert!(!has_attempted(&path, "local", "codex", "0.159.0"));

        // 其他主機或其他 kind 不受影響
        assert!(!has_attempted(&path, "remote1", "codex", "0.158.0"));
        assert!(!has_attempted(&path, "local", "claude", "0.158.0"));

        // 手動更新清除後，可以重新嘗試
        clear_attempt(&path, "local", "codex");
        assert!(!has_attempted(&path, "local", "codex", "0.158.0"));
    }
}
