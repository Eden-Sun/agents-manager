//! Plain login-shell panes on a host, for the UI's host-shell panel (no agent, no run, no turn).
//!
//! **The registry is the whitelist.** Every endpoint except `open` 404s unless `(host, pane_id)`
//! is in [`Registry`], so keys can never reach an arbitrary pane. Memory only on purpose: after a
//! restart every old pane is a stranger.

use crate::db;
use crate::herdr::HerdrClient;
use crate::lifecycle::{LcError, LcResult};
use crate::state::App;
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Caps a stuck finger leaving a row of invisible panes behind.
pub const MAX_PER_HOST: usize = 8;

const SHELL_LABEL: &str = "shell";

#[derive(Debug, Clone, Serialize)]
pub struct HostShell {
    pub host: String,
    pub herdr_session: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub pane_id: String,
    pub cwd: String,
    pub created_at: String,
}

/// `App.host_shells`; deliberately not persisted (see module comment).
pub type Registry = Mutex<Vec<HostShell>>;

/// `App.pane_live`：`(host, pane_id)` → 上次即時複查的時間與結果。
pub type LiveCache = Mutex<std::collections::HashMap<(String, String), (std::time::Instant, LiveVerdict)>>;

/// 打字前即時複查的結果能重用多久。鍵盤同步是一鍵一個請求，每一鍵都跑 `ps`＋`lsof` 太慢；
/// 幾秒的窗口內剛開起來的 port 會晚一點才鎖住，換來打字不卡。
const LIVE_TTL: std::time::Duration = std::time::Duration::from_secs(3);

fn up<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(e.to_string())
}

/// The host's **manager** session only: the local `default` session is the user's own and the
/// daemon never puts things into it. A down host fails here, before a pane reaches the registry.
pub(crate) async fn client_for(app: &Arc<App>, host: &str) -> LcResult<(HerdrClient, String)> {
    if app.hosts.get(host).await.is_none() {
        return Err(LcError::NotFound("host".into()));
    }
    let session = app
        .session_for_host(host)
        .await
        .ok_or_else(|| LcError::Upstream(format!("host `{host}` has no Herdr session configured")))?;
    if !app.session_connected(host, &session).await {
        return Err(LcError::Upstream(format!("host `{host}` is not connected")));
    }
    let client = app
        .herdr_for_session(host, &session)
        .await
        .ok_or_else(|| LcError::Upstream(format!("Herdr session `{session}` for host `{host}` is not configured")))?;
    Ok((client, session))
}

/// A project on that host (where the jobs actually happen), else its `$HOME`.
async fn default_cwd(app: &Arc<App>, host: &str) -> LcResult<String> {
    let projects = db::live_projects(&app.db).await.map_err(up)?;
    if let Some(p) = projects.iter().find(|p| p.host == host) {
        return Ok(p.path.clone());
    }
    let conn = app.hosts.get(host).await.ok_or_else(|| LcError::NotFound("host".into()))?;
    conn.home().await.map_err(up)
}

/// A fresh workspace's root pane is already its own tab, so it is used as-is. It is deliberately
/// **not** written back to `projects.workspace_id`, or the next bot would land in a `shell` workspace.
async fn acquire_pane(
    app: &Arc<App>,
    client: &HerdrClient,
    host: &str,
    cwd: &str,
) -> LcResult<crate::herdr::PaneInfo> {
    let projects = db::live_projects(&app.db).await.map_err(up)?;
    for p in projects.iter().filter(|p| p.host == host) {
        let Some(ws) = p.workspace_id.as_deref().filter(|w| !w.trim().is_empty()) else { continue };
        if client.workspace_get(ws).await.map_err(up)?.is_some() {
            return client.tab_create(ws, cwd, SHELL_LABEL, json!({})).await.map_err(up);
        }
    }
    let (_, root) = client.workspace_create(cwd, SHELL_LABEL, json!({})).await.map_err(up)?;
    Ok(root)
}

/// `POST /api/hosts/:name/shells`
pub async fn open(app: &Arc<App>, host: &str, cwd: Option<&str>) -> LcResult<HostShell> {
    let (client, session) = client_for(app, host).await?;
    // Counted before the pane is created, so a burst of clicks cannot race past the cap.
    let live = app.host_shells.lock().await.iter().filter(|s| s.host == host).count();
    if live >= MAX_PER_HOST {
        return Err(LcError::conflict("too_many_shells", json!({"host": host, "max": MAX_PER_HOST})));
    }
    let cwd = match cwd.map(str::trim).filter(|c| !c.is_empty()) {
        Some(c) => c.to_string(),
        None => default_cwd(app, host).await?,
    };
    let pane = acquire_pane(app, &client, host, &cwd).await?;
    let shell = HostShell {
        host: host.to_string(),
        herdr_session: session,
        workspace_id: pane.workspace_id.clone(),
        tab_id: pane.tab_id.clone(),
        pane_id: pane.pane_id.clone(),
        // What herdr actually opened: a missing path lands somewhere else.
        cwd: pane.cwd.clone().unwrap_or(cwd),
        created_at: db::now(),
    };
    app.host_shells.lock().await.push(shell.clone());
    tracing::info!(host, pane_id = %shell.pane_id, cwd = %shell.cwd, "opened a host shell");
    Ok(shell)
}

/// `GET /api/hosts/:name/shells` — sweeps out panes closed by hand in herdr.
pub async fn list(app: &Arc<App>, host: &str) -> LcResult<Vec<HostShell>> {
    let (client, _) = client_for(app, host).await?;
    let mine: Vec<HostShell> = app.host_shells.lock().await.iter().filter(|s| s.host == host).cloned().collect();
    let mut alive = Vec::with_capacity(mine.len());
    let mut dead = Vec::new();
    for s in mine {
        // An unreachable herdr is not evidence the pane died; let the next poll decide.
        match client.pane_get(&s.pane_id).await {
            Ok(None) => dead.push(s.pane_id.clone()),
            _ => alive.push(s),
        }
    }
    if !dead.is_empty() {
        app.host_shells.lock().await.retain(|s| s.host != host || !dead.contains(&s.pane_id));
    }
    Ok(alive)
}

/// 這一次操作要不要打字。看得到與打得進去是兩種權限：有 listen port 的 pane 只能看
/// ——送一個 Ctrl-C 給 dev server 就是把它殺掉（§6.5e）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    View,
    Type,
}

/// 這顆被 trace 的 pane 允不允許這次操作（SPEC §6.5e，2026-09-16 統整者裁示）。
///
/// **看 listen port，不看 `kind`**：`kind=service` 也包含跑著 vim／less／sudo／python 的 shell，
/// 那些必須打得進去，不然人卡在裡面出不來（web review H2）。只有真的在 listen 的（dev server 之類）只可看。
/// `kind` 只剩一個用途：擋掉不該在表裡的東西（agent pane 混進來也一律不給，§6.5.1／§6.9 的教訓）。
pub fn allowed(kind: &str, read_only: bool, access: Access) -> bool {
    match kind {
        "shell" | "service" => access == Access::View || !read_only,
        _ => false,
    }
}

/// 即時複查（review 2026-09-16 core 4）：表上的 kind／port 是掃描的快取，而掃描沒有定期跑的保證。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveVerdict {
    Typeable,
    /// 這顆 pane 的行程樹現在有 listen port。
    ReadOnly,
    /// herdr 說這顆 pane 裡現在有 agent（例如被 `herdr agent start --pane` 起了子 agent）。
    Agent,
    /// herdr 說這顆 pane 已經不在了。
    Gone,
}

/// 問 herdr 這顆 pane 現在的樣子（本機另外對 listen port）。問不到回 `None`，由呼叫端退回表上的值。
/// 遠端不算 port（§6.5e：不為了它多開 ssh 往返），所以遠端只會是 Typeable／Agent／Gone。
pub(crate) async fn live_verdict(app: &Arc<App>, host: &str, pane_id: &str) -> Option<LiveVerdict> {
    let key = (host.to_string(), pane_id.to_string());
    if let Some((at, v)) = app.pane_live.lock().await.get(&key).copied() {
        if at.elapsed() < LIVE_TTL {
            return Some(v);
        }
    }
    let (client, _) = client_for(app, host).await.ok()?;
    let verdict = match client.pane_get(pane_id).await.ok()? {
        None => LiveVerdict::Gone,
        Some(p) if p.agent.as_deref().is_some_and(|a| !a.is_empty()) => LiveVerdict::Agent,
        Some(_) if host != crate::config::LOCAL_HOST => LiveVerdict::Typeable,
        Some(_) => {
            let dump = crate::memproc::dump(app, host).await.ok()?;
            let shell = client.pane_shell(pane_id).await.ok()?;
            let facts = crate::panes::facts_from(&shell, &dump, pane_id)?;
            let ports = crate::panes::listen_ports(host, &facts.pids).await;
            if ports.values().any(|p| !p.is_empty()) {
                LiveVerdict::ReadOnly
            } else {
                LiveVerdict::Typeable
            }
        }
    };
    app.pane_live.lock().await.insert(key, (std::time::Instant::now(), verdict));
    Some(verdict)
}

fn read_only_error(ports: &[u16]) -> LcError {
    let list = ports.iter().map(u16::to_string).collect::<Vec<_>>().join(", ");
    let message = if list.is_empty() {
        "這顆 pane 有 listen port，只能看不能打字".to_string()
    } else {
        format!("這顆 pane 在 listen {list}，只能看不能打字（送 Ctrl-C 就是把它關掉）")
    };
    LcError::Forbidden(json!({"error": "read_only_pane", "listen_ports": ports, "message": message}))
}

fn agent_pane_error() -> LcError {
    LcError::Forbidden(json!({"error": "agent_pane", "message": "這顆 pane 正在跑 agent，請走 bot 對話"}))
}

/// The whitelist check every endpoint below runs.
///
/// 兩份白名單（§6.5e，使用者 2026-09-16「這 shell pane 要在 menu 可點選進入」）：
/// 1. `app.host_shells`——daemon 自己開的，只在記憶體；
/// 2. `panes` 表——掃描認出來、已經綁到專案的 shell／service pane。選單裡點得到的就是這一批，
///    而且它活過重啟（記憶體那份不會）。
///
/// 安全界線沒有放寬：有 listen port 的唯讀，而且**正在跑 agent 的 pane 一律不給**
/// ——就算掃描在 agent 還沒被 herdr 認出來的空檔把它記成 shell，也不能讓按鍵繞過回合那條線。
/// 打字前再即時問一次 herdr（[`live_verdict`]）：表上的 port 可能是好幾分鐘前的。
async fn registered(app: &Arc<App>, host: &str, pane_id: &str, access: Access) -> LcResult<HostShell> {
    if let Some(s) = app.host_shells.lock().await.iter().find(|s| s.host == host && s.pane_id == pane_id).cloned() {
        return Ok(s);
    }
    type Row = (String, Option<String>, Option<String>, Option<String>, String, Option<String>);
    let row: Option<Row> =
        sqlx::query_as("SELECT kind, workspace_id, tab_id, cwd, first_seen, listen_ports FROM panes WHERE host=? AND pane_id=?")
            .bind(host)
            .bind(pane_id)
            .fetch_optional(&app.db)
            .await
            .map_err(up)?;
    let Some((kind, workspace_id, tab_id, cwd, first_seen, ports)) = row else {
        return Err(LcError::NotFound("shell".into()));
    };
    let ports = crate::panes::parse_ports(ports.as_deref());
    if !allowed(&kind, !ports.is_empty(), access) {
        return Err(read_only_error(&ports));
    }
    let session = app.session_for_host(host).await.unwrap_or_default();
    let runs = crate::db::active_runs_for_pane(&app.db, host, pane_id, &session, &session).await.map_err(up)?;
    if !runs.is_empty() {
        return Err(agent_pane_error());
    }
    if access == Access::Type {
        match live_verdict(app, host, pane_id).await {
            Some(LiveVerdict::Gone) => return Err(LcError::NotFound("shell".into())),
            Some(LiveVerdict::Agent) => return Err(agent_pane_error()),
            Some(LiveVerdict::ReadOnly) => return Err(read_only_error(&[])),
            Some(LiveVerdict::Typeable) | None => {}
        }
    }
    Ok(HostShell {
        host: host.to_string(),
        herdr_session: session,
        workspace_id: workspace_id.unwrap_or_default(),
        tab_id: tab_id.unwrap_or_default(),
        pane_id: pane_id.to_string(),
        cwd: cwd.unwrap_or_default(),
        created_at: first_seen,
    })
}

/// `GET /api/hosts/:name/shells/:pane_id/terminal` — same shape as `GET /bots/:id/terminal`.
pub async fn read(app: &Arc<App>, host: &str, pane_id: &str, source: &str, lines: u32) -> LcResult<Value> {
    let shell = registered(app, host, pane_id, Access::View).await?;
    if !["visible", "recent", "recent_unwrapped", "detection"].contains(&source) {
        return Err(LcError::Bad("bad source".into()));
    }
    let (client, _) = client_for(app, host).await?;
    let read = client.pane_read(pane_id, source, lines).await.map_err(up)?;
    // Best effort, as in `get_terminal`: a snapshot is still worth returning without geometry.
    let (columns, rows) = match client.pane_size(pane_id).await {
        Ok(Some((w, h))) => (Some(w), Some(h)),
        _ => (None, None),
    };
    Ok(json!({
        "host": host, "pane_id": pane_id, "cwd": shell.cwd,
        "source": read.source, "text": read.text, "revision": read.revision, "truncated": read.truncated,
        "columns": columns, "rows": rows,
    }))
}

/// `POST /api/hosts/:name/shells/:pane_id/text`. Enter is a **separate** `pane.send_keys`: a `\n`
/// in `pane.send_text` is a pasted line break to herdr. Empty `text` + `enter` = "just press Enter".
pub async fn send_text(app: &Arc<App>, host: &str, pane_id: &str, text: &str, enter: bool) -> LcResult<()> {
    registered(app, host, pane_id, Access::Type).await?;
    let (client, _) = client_for(app, host).await?;
    if !text.is_empty() {
        client.pane_send_text(pane_id, text).await.map_err(up)?;
    }
    if enter {
        client.pane_send_keys(pane_id, &["enter"]).await.map_err(up)?;
    }
    Ok(())
}

/// `POST /api/hosts/:name/shells/:pane_id/keys` — names go to herdr verbatim, like `POST /bots/:id/keys`.
pub async fn send_keys(app: &Arc<App>, host: &str, pane_id: &str, keys: &[String]) -> LcResult<()> {
    registered(app, host, pane_id, Access::Type).await?;
    if keys.is_empty() {
        return Err(LcError::Bad("keys must not be empty".into()));
    }
    let (client, _) = client_for(app, host).await?;
    let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    client.pane_send_keys(pane_id, &refs).await.map_err(up)
}

/// `DELETE /api/hosts/:name/shells/:pane_id`. Idempotent: an unregistered pane is success; only an
/// unresolvable host errors, since then we cannot tell whether anything was left behind.
pub async fn close(app: &Arc<App>, host: &str, pane_id: &str) -> LcResult<()> {
    let (client, _) = client_for(app, host).await?;
    let Some(shell) = app.host_shells.lock().await.iter().find(|s| s.host == host && s.pane_id == pane_id).cloned()
    else {
        return Ok(());
    };
    crate::lifecycle::close_pane_and_tab(&client, Some(&shell.workspace_id), Some(&shell.tab_id), pane_id).await;
    app.host_shells.lock().await.retain(|s| s.host != host || s.pane_id != pane_id);
    tracing::info!(host, pane_id, "closed a host shell");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whitelist must key on **both** halves: a pane_id is only unique within a host.
    #[test]
    fn a_shell_is_only_recognised_on_the_host_it_was_opened_on() {
        let rows = vec![
            HostShell {
                host: "local".into(),
                herdr_session: "agents-manager".into(),
                workspace_id: "w1".into(),
                tab_id: "w1:t2".into(),
                pane_id: "w1:p2".into(),
                cwd: "/tmp".into(),
                created_at: "2026-09-07T00:00:00Z".into(),
            },
            HostShell {
                host: "m4p".into(),
                herdr_session: "agents-manager".into(),
                workspace_id: "w3".into(),
                tab_id: "w3:t1".into(),
                pane_id: "w3:p1".into(),
                cwd: "/Users/m4p".into(),
                created_at: "2026-09-07T00:00:00Z".into(),
            },
        ];
        let found = |host: &str, pane: &str| rows.iter().any(|s| s.host == host && s.pane_id == pane);

        assert!(found("local", "w1:p2"));
        assert!(found("m4p", "w3:p1"));
        // The same pane_id on the wrong host must not match…
        assert!(!found("m4p", "w1:p2"));
        // …and neither must a pane the daemon never opened, which is every bot pane.
        assert!(!found("local", "w1:p1"));
    }

    async fn app() -> Arc<App> {
        let dir = std::env::temp_dir().join(format!("am-shell-{}", crate::db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = crate::db::open(&dir.join("t.sqlite3")).await.unwrap();
        let cfg = crate::config::ConfigStore::load(dir.join("config.toml")).await.unwrap();
        let client = HerdrClient::new(dir.join("absent.sock"));
        App::new(db, client.clone(), client, cfg, dir.clone(), dir.join("daemon"), 7799, "test".into(), "test".into(), false)
    }

    async fn tracked(app: &Arc<App>, pane_id: &str, kind: &str) {
        let now = crate::db::now();
        sqlx::query(
            "INSERT INTO panes (pane_id, host, workspace_id, tab_id, cwd, kind, last_output_at, first_seen, last_seen)
             VALUES (?, 'local', 'w1', 'w1:t1', '/tmp/proj', ?, ?, ?, ?)",
        )
        .bind(pane_id)
        .bind(kind)
        .bind(&now)
        .bind(&now)
        .bind(&now)
        .execute(&app.db)
        .await
        .unwrap();
    }

    /// 被 trace 的 shell pane 要點得進去——**而且活過重啟**：記憶體那份白名單重啟就空了，
    /// 這一條走的是 `panes` 表（§6.5e，使用者 2026-09-16「這 shell pane 要在 menu 可點選進入」）。
    /// 唯讀看 listen port：跑著 vim 的（kind=service、沒有 port）要打得進去，不然人出不來（web review H2）。
    #[tokio::test]
    async fn a_tracked_pane_is_typeable_unless_it_listens_on_a_port() {
        let app = app().await;
        tracked(&app, "w1:pS", "shell").await;
        tracked(&app, "w1:pVim", "service").await;
        tracked(&app, "w1:pV", "service").await;
        sqlx::query("UPDATE panes SET listen_ports='3010' WHERE pane_id='w1:pV'").execute(&app.db).await.unwrap();

        let got = registered(&app, "local", "w1:pS", Access::Type).await.expect("shell 可以打字");
        assert_eq!((got.cwd.as_str(), got.workspace_id.as_str()), ("/tmp/proj", "w1"));
        assert!(registered(&app, "local", "w1:pVim", Access::Type).await.is_ok(), "跑著 vim 的 service 要打得進去");
        assert!(registered(&app, "local", "w1:pV", Access::View).await.is_ok(), "dev server 看得到");
        match registered(&app, "local", "w1:pV", Access::Type).await {
            Err(LcError::Forbidden(body)) => {
                assert_eq!(body["error"], "read_only_pane");
                assert_eq!(body["listen_ports"], json!([3010]));
            }
            other => panic!("dev server 不能打字：{:?}", other.map(|s| s.pane_id)),
        }
        // 沒被 trace 的 pane 仍然是陌生人。
        assert!(matches!(registered(&app, "local", "w1:pX", Access::View).await, Err(LcError::NotFound(_))));
        // 主機不對也不算（pane id 只在單一主機內唯一）。
        assert!(matches!(registered(&app, "m4p", "w1:pS", Access::View).await, Err(LcError::NotFound(_))));
        std::fs::remove_dir_all(&app.data_dir).ok();
    }

    /// 掃描在 agent 還沒被 herdr 認出來的空檔可能把 bot 的 pane 記成 shell。那一刻也不能讓按鍵
    /// 繞過回合那條線：有 active run 的 pane 一律擋掉（§6.5.1／§6.9 的教訓）。
    #[tokio::test]
    async fn a_pane_with_a_live_run_is_never_typeable_even_if_it_got_recorded_as_a_shell() {
        let app = app().await;
        let now = crate::db::now();
        sqlx::query("INSERT INTO projects (id,path,label,host,created_at) VALUES ('p','/tmp/proj','p','local',?)").bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('b','p','b','claude','t',?)").bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO runs (id, bot_id, state, agent_status, pane_id, started_at) VALUES ('r','b','running','idle','w1:pA',?)").bind(&now).execute(&app.db).await.unwrap();
        tracked(&app, "w1:pA", "shell").await;

        for access in [Access::View, Access::Type] {
            assert!(matches!(registered(&app, "local", "w1:pA", access).await, Err(LcError::Forbidden(_))), "{access:?}");
        }
        std::fs::remove_dir_all(&app.data_dir).ok();
    }

    /// review 2026-09-16 core 4：打字前即時問 herdr。表上記成 shell，但裡面現在有 agent、或 pane 已經不在，
    /// 都不能照表放行；問不到才退回表上的 port。
    #[tokio::test]
    async fn typing_rechecks_the_pane_live() {
        let env = crate::testing::env().await;
        let app = &env.app;
        let (_, pane) = app.herdr.workspace_create("/tmp", "w", json!({})).await.unwrap();
        let now = crate::db::now();
        for id in [pane.pane_id.as_str(), "ws-9:pGone"] {
            sqlx::query(
                "INSERT INTO panes (pane_id, host, workspace_id, tab_id, kind, last_output_at, first_seen, last_seen)
                 VALUES (?, 'local', ?, ?, 'shell', ?, ?, ?)",
            )
            .bind(id)
            .bind(&pane.workspace_id)
            .bind(&pane.tab_id)
            .bind(&now)
            .bind(&now)
            .bind(&now)
            .execute(&app.db)
            .await
            .unwrap();
        }
        assert!(registered(app, "local", &pane.pane_id, Access::Type).await.is_ok(), "mock 沒有 shell_pid：問不到 port，照表放行");
        assert!(matches!(registered(app, "local", "ws-9:pGone", Access::Type).await, Err(LcError::NotFound(_))));

        env.herdr.set_agent("kid", &pane.pane_id, false);
        app.pane_live.lock().await.clear();
        match registered(app, "local", &pane.pane_id, Access::Type).await {
            Err(LcError::Forbidden(body)) => assert_eq!(body["error"], "agent_pane"),
            other => panic!("pane 裡現在有 agent：{:?}", other.map(|s| s.pane_id)),
        }
        // 只看畫面不需要即時問（面板每 0.25 秒讀一次）。
        assert!(registered(app, "local", &pane.pane_id, Access::View).await.is_ok());
    }

    /// 選單點得進去的那一批（`panes` 表）：有 listen port 的只可看，其餘可看可打字；表裡不該有的 kind 一律不給。
    /// 送一個 Ctrl-C 給 dev server 就是把它殺掉（§6.5e）。
    #[test]
    fn a_listening_pane_can_be_watched_but_never_typed_into() {
        for kind in ["shell", "service"] {
            assert!(allowed(kind, false, Access::View) && allowed(kind, false, Access::Type), "{kind}");
            assert!(allowed(kind, true, Access::View), "{kind}");
            assert!(!allowed(kind, true, Access::Type), "{kind}：dev server 收到按鍵就沒了");
        }
        for kind in ["agent", "", "anything"] {
            for ro in [false, true] {
                assert!(!allowed(kind, ro, Access::View) && !allowed(kind, ro, Access::Type), "{kind}");
            }
        }
    }
}
