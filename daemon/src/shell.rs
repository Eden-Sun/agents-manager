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

/// 問 herdr 這顆 pane 現在的樣子（本機另外對 listen port）。問不到回 `None`——呼叫端**不放行**（比照 GC「讀不到就不關」）。
/// 遠端不算 port（§6.5e：不為了它多開 ssh 往返），所以遠端只會是 Typeable／Agent／Gone；遠端的 Typeable 還要再看表上的事實。
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
            let probe = app.probe();
            let dump = probe.dump(app, host).await.ok()?;
            let shell = client.pane_shell(pane_id).await.ok()?;
            let facts = crate::panes::facts_from(&shell, &dump, pane_id)?;
            let ports = probe.listen_ports(host, &facts.pids).await?;
            if !ports.is_empty() {
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

/// 打字前的即時複查讀不到（herdr 沒回、`ps` 失敗、herdr 沒報 shell pid）：不放行，請人稍後再試（AGM 2026-09-16 驗收退回）。
fn state_unknown_error() -> LcError {
    LcError::conflict(
        "pane_state_unknown",
        json!({"retryable": true, "message": "無法確認這顆 pane 現在的狀態（是不是在 listen、裡面有沒有 agent），請稍後再試"}),
    )
}

/// 打字前的判定（純函式，可測）。`live` 是 [`live_verdict`] 的結果；遠端的 herdr 回答不含 port，
/// 所以遠端退回表上已知的事實：`kind='service'` 或記過 listen port 就唯讀（AGM 2026-09-16 驗收退回）。
pub(crate) fn typing_decision(live: Option<LiveVerdict>, local: bool, kind: &str, stored_ports: &[u16]) -> Result<(), LcError> {
    match live {
        None => Err(state_unknown_error()),
        Some(LiveVerdict::Gone) => Err(LcError::NotFound("shell".into())),
        Some(LiveVerdict::Agent) => Err(agent_pane_error()),
        Some(LiveVerdict::ReadOnly) => Err(read_only_error(&[])),
        Some(LiveVerdict::Typeable) if !local && (kind == "service" || !stored_ports.is_empty()) => Err(read_only_error(stored_ports)),
        Some(LiveVerdict::Typeable) => Ok(()),
    }
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
    // 表裡不該有的 kind 一律不給；port 在這裡不擋——本機打字前會即時重對，表上的可能是好幾分鐘前的。
    if !allowed(&kind, false, access) {
        return Err(read_only_error(&ports));
    }
    let session = app.session_for_host(host).await.unwrap_or_default();
    let runs = crate::db::active_runs_for_pane(&app.db, host, pane_id, &session, &session).await.map_err(up)?;
    if !runs.is_empty() {
        return Err(agent_pane_error());
    }
    if access == Access::Type {
        let local = host == crate::config::LOCAL_HOST;
        typing_decision(live_verdict(app, host, pane_id).await, local, &kind, &ports)?;
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

/// 關掉這個 daemon 自己開的 shell（登入流程之類的內部呼叫）。記憶體清單沒有時等同**沒確認**的 [`close_confirmed`]。
pub async fn close(app: &Arc<App>, host: &str, pane_id: &str) -> LcResult<()> {
    close_confirmed(app, host, pane_id, false).await
}

/// `DELETE /api/hosts/:name/shells/:pane_id[?confirm=true]`。跟 [`registered`] 認同樣兩份白名單（web review M2）：
/// daemon 重啟後記憶體那份是空的，面板靠 `panes` 表照常顯示自己開的 shell；以前這裡找不到就回 200、什麼都沒關，
/// 面板卻收掉了。
///
/// - 記憶體清單裡的（這個 daemon 自己開的）：照舊直接關。
/// - 記憶體沒有、`panes` 表有：走 `panes::close_tracked`，**`confirm` 照呼叫端帶的傳下去**——服務 pane（在 listen、前景有程式）
///   或讀不到事實時，沒確認就 409 `service_pane`（AGM 2026-09-16 驗收：以前寫死 confirm，確認整個被繞掉）。agent／active run 照樣 403。
/// - 兩邊都沒有：404。
pub async fn close_confirmed(app: &Arc<App>, host: &str, pane_id: &str, confirmed: bool) -> LcResult<()> {
    let (client, _) = client_for(app, host).await?;
    let Some(shell) = app.host_shells.lock().await.iter().find(|s| s.host == host && s.pane_id == pane_id).cloned()
    else {
        return match crate::panes::close_tracked(app, host, pane_id, confirmed).await {
            Err(LcError::NotFound(_)) => Err(LcError::NotFound("shell".into())),
            other => other.map(|_| ()),
        };
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
    #[tokio::test]
    async fn a_tracked_pane_is_reachable_through_the_pane_table() {
        let app = app().await;
        tracked(&app, "w1:pS", "shell").await;
        let got = registered(&app, "local", "w1:pS", Access::View).await.expect("看得到");
        assert_eq!((got.cwd.as_str(), got.workspace_id.as_str()), ("/tmp/proj", "w1"));
        // 沒被 trace 的 pane 仍然是陌生人。
        assert!(matches!(registered(&app, "local", "w1:pX", Access::View).await, Err(LcError::NotFound(_))));
        // 主機不對也不算（pane id 只在單一主機內唯一）。
        assert!(matches!(registered(&app, "m4p", "w1:pS", Access::View).await, Err(LcError::NotFound(_))));
        std::fs::remove_dir_all(&app.data_dir).ok();
    }

    /// AGM 2026-09-16 驗收退回 (1)：打字前的即時複查讀不到（這裡沒有 herdr），比照 GC「讀不到就不關」——不放行，
    /// 回 409 `pane_state_unknown` 請人稍後再試。只看畫面不需要複查。
    #[tokio::test]
    async fn typing_is_refused_when_the_pane_cannot_be_checked() {
        let app = app().await;
        tracked(&app, "w1:pS", "shell").await;
        match registered(&app, "local", "w1:pS", Access::Type).await {
            Err(LcError::Conflict(body)) => {
                assert_eq!(body["reason"], "pane_state_unknown");
                assert_eq!(body["retryable"], true);
            }
            other => panic!("讀不到不能放行：{:?}", other.map(|s| s.pane_id)),
        }
        assert!(registered(&app, "local", "w1:pS", Access::View).await.is_ok());
        std::fs::remove_dir_all(&app.data_dir).ok();
    }

    /// AGM 2026-09-16 驗收退回 (2)：遠端的 herdr 回答不含 port，退回表上已知的事實——`kind='service'` 或記過 port 就唯讀。
    /// 本機的 port 是即時重對的，所以本機跑著 vim 的 service（沒 port）照樣可以打字（web review H2）。
    #[test]
    fn a_remote_service_pane_is_read_only_but_a_local_one_without_ports_is_not() {
        let t = Some(LiveVerdict::Typeable);
        let code = |r: Result<(), LcError>| match r {
            Ok(()) => "ok".to_string(),
            Err(LcError::Forbidden(b)) => b["error"].as_str().unwrap().to_string(),
            Err(LcError::Conflict(b)) => b["reason"].as_str().unwrap().to_string(),
            Err(LcError::NotFound(_)) => "404".to_string(),
            Err(_) => "other".to_string(),
        };
        assert_eq!(code(typing_decision(t, false, "service", &[])), "read_only_pane", "遠端 service");
        assert_eq!(code(typing_decision(t, false, "shell", &[8080])), "read_only_pane", "遠端記過 port");
        assert_eq!(code(typing_decision(t, false, "shell", &[])), "ok", "遠端 shell");
        assert_eq!(code(typing_decision(t, true, "service", &[])), "ok", "本機跑著 vim、沒有 port");
        assert_eq!(code(typing_decision(None, false, "shell", &[])), "pane_state_unknown", "遠端讀不到也不放行");
        assert_eq!(code(typing_decision(None, true, "shell", &[])), "pane_state_unknown");
        assert_eq!(code(typing_decision(Some(LiveVerdict::ReadOnly), true, "shell", &[])), "read_only_pane");
        assert_eq!(code(typing_decision(Some(LiveVerdict::Agent), true, "shell", &[])), "agent_pane");
        assert_eq!(code(typing_decision(Some(LiveVerdict::Gone), true, "shell", &[])), "404");
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

    /// review 2026-09-16 core 4：打字前即時問 herdr 並在本機重對 listen port。表上記成 shell，但現在在 listen、
    /// 裡面現在有 agent、或 pane 已經不在，都不能照表放行；表上記過 port、現在沒有了（server 停了）也不該擋。
    #[tokio::test]
    async fn typing_rechecks_the_pane_live() {
        let env = crate::testing::env().await;
        let app = &env.app;
        let now = crate::db::now();
        let track = |id: String, ws: String, tab: String, ports: Option<&'static str>| {
            let (app, now) = (app.clone(), now.clone());
            async move {
                sqlx::query(
                    "INSERT INTO panes (pane_id, host, workspace_id, tab_id, kind, listen_ports, last_output_at, first_seen, last_seen)
                     VALUES (?, 'local', ?, ?, 'service', ?, ?, ?, ?)",
                )
                .bind(id)
                .bind(ws)
                .bind(tab)
                .bind(ports)
                .bind(&now)
                .bind(&now)
                .bind(&now)
                .execute(&app.db)
                .await
                .unwrap();
            }
        };

        // 跑著 vim 的 pane：shell 底下一個不 listen 的行程。表上還記著舊的 port——即時的為準，可以打字。
        // 行程樹與 listen port 是決定性的假貨：真的 `ps`／`lsof` 在 CI runner 上會慢到逾時（見 `probe_smoke_*`）。
        *app.pane_probe.lock().unwrap() = Arc::new(
            crate::pane_probe::Fixed::tree(&[
                (41001, 1, "-zsh"),
                (41002, 41001, "sleep 60"),
                (41003, 1, "-zsh"),
                (41004, 41003, "node dev-server"),
            ])
            .listening(41004, 3010),
        );
        let (_, vim) = app.herdr.workspace_create("/tmp", "vim", json!({})).await.unwrap();
        env.herdr.set_shell_pid(&vim.pane_id, 41001);
        track(vim.pane_id.clone(), vim.workspace_id.clone(), vim.tab_id.clone(), Some("3010")).await;
        let typed = registered(app, "local", &vim.pane_id, Access::Type).await;
        assert!(typed.is_ok(), "{:?}", typed.map(|s| s.pane_id));

        // 真的在 listen 的：唯讀。
        let (_, dev) = app.herdr.workspace_create("/tmp", "dev", json!({})).await.unwrap();
        env.herdr.set_shell_pid(&dev.pane_id, 41003);
        track(dev.pane_id.clone(), dev.workspace_id.clone(), dev.tab_id.clone(), None).await;
        match registered(app, "local", &dev.pane_id, Access::Type).await {
            Err(LcError::Forbidden(body)) => assert_eq!(body["error"], "read_only_pane"),
            other => panic!("在 listen 的不能打字：{:?}", other.map(|s| s.pane_id)),
        }

        // pane 已經不在：404。
        track("ws-9:pGone".into(), "ws-9".into(), "ws-9:t1".into(), None).await;
        assert!(matches!(registered(app, "local", "ws-9:pGone", Access::Type).await, Err(LcError::NotFound(_))));

        // 裡面現在有 agent：403。
        env.herdr.set_agent("kid", &vim.pane_id, false);
        app.pane_live.lock().await.clear();
        match registered(app, "local", &vim.pane_id, Access::Type).await {
            Err(LcError::Forbidden(body)) => assert_eq!(body["error"], "agent_pane"),
            other => panic!("pane 裡現在有 agent：{:?}", other.map(|s| s.pane_id)),
        }
    }

    /// 像剛重啟：記憶體那份白名單是空的，pane 只在 `panes` 表裡。
    async fn tracked_after_restart(env: &crate::testing::Env, label: &str) -> crate::herdr::PaneInfo {
        let app = &env.app;
        assert!(app.host_shells.lock().await.is_empty(), "像剛重啟");
        let (_, p) = app.herdr.workspace_create("/tmp", label, json!({})).await.unwrap();
        let now = crate::db::now();
        sqlx::query(
            "INSERT INTO panes (pane_id, host, workspace_id, tab_id, kind, last_output_at, first_seen, last_seen)
             VALUES (?, 'local', ?, ?, 'shell', ?, ?, ?)",
        )
        .bind(&p.pane_id)
        .bind(&p.workspace_id)
        .bind(&p.tab_id)
        .bind(&now)
        .bind(&now)
        .bind(&now)
        .execute(&app.db)
        .await
        .unwrap();
        p
    }

    fn conflict_body(r: LcResult<()>) -> Value {
        match r {
            Err(LcError::Conflict(body)) => body,
            other => panic!("沒確認要被擋：{other:?}"),
        }
    }

    /// AGM 2026-09-16 驗收：daemon 重啟後「結束 shell」走 `panes` 表關，confirm 要照呼叫端帶的。
    /// 在 listen 的服務 pane 沒確認 → 409 service_pane，帶即時的 kind／port（行程樹與 port 由 `pane_probe::Fixed` 決定）。
    #[tokio::test]
    async fn ending_an_unconfirmed_service_pane_after_a_restart_is_refused() {
        let env = crate::testing::env().await;
        let dev = tracked_after_restart(&env, "dev").await;
        let port = 8123;
        *env.app.pane_probe.lock().unwrap() = Arc::new(crate::pane_probe::Fixed::tree(&[(42001, 1, "-zsh")]).listening(42001, port));
        env.herdr.set_shell_pid(&dev.pane_id, 42001);
        let body = conflict_body(close_confirmed(&env.app, "local", &dev.pane_id, false).await);
        assert_eq!(body["reason"], "service_pane");
        assert_eq!(body["unverified"], false);
        assert_eq!(body["pane"]["kind"], "service");
        assert_eq!(body["pane"]["read_only"], true);
        assert!(body["pane"]["listen_ports"].as_array().unwrap().contains(&json!(port)), "{body}");
        assert!(env.app.herdr.pane_get(&dev.pane_id).await.unwrap().is_some(), "還沒關");
    }

    /// 讀不到事實（mock 沒報 shell pid）也一樣：沒確認 → 409，標 `unverified: true`。
    #[tokio::test]
    async fn ending_an_unconfirmed_pane_whose_state_cannot_be_read_is_refused() {
        let env = crate::testing::env().await;
        let p = tracked_after_restart(&env, "shell").await;
        let body = conflict_body(close_confirmed(&env.app, "local", &p.pane_id, false).await);
        assert_eq!((body["reason"].as_str(), body["unverified"].as_bool()), (Some("service_pane"), Some(true)));
        assert!(env.app.herdr.pane_get(&p.pane_id).await.unwrap().is_some(), "還沒關");
        // 內部呼叫的 `close`（沒有 confirm 可帶）同樣不繞過。
        assert!(matches!(close(&env.app, "local", &p.pane_id).await, Err(LcError::Conflict(_))));
    }

    /// 確認過才關（web review M2：以前回 200 卻什麼都沒關）；關完兩份都沒有 → 404。記憶體清單裡自己開的照舊直接關。
    #[tokio::test]
    async fn ending_a_confirmed_shell_after_a_restart_closes_it() {
        let env = crate::testing::env().await;
        let app = &env.app;
        let p = tracked_after_restart(&env, "shell").await;
        close_confirmed(app, "local", &p.pane_id, true).await.expect("確認過就關");
        assert!(app.herdr.pane_get(&p.pane_id).await.unwrap().is_none(), "herdr 上真的關了");
        assert!(matches!(close_confirmed(app, "local", &p.pane_id, true).await, Err(LcError::NotFound(_))));

        let opened = open(app, "local", Some("/tmp")).await.unwrap();
        close(app, "local", &opened.pane_id).await.expect("自己開的直接關，不用確認");
        assert!(app.herdr.pane_get(&opened.pane_id).await.unwrap().is_none());
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

    /// 煙霧測試：真的 `ps`／`lsof`（正式路徑）對真的 pid。上面兩條決定性的測試驗邏輯，這條驗指令本身。
    /// 外部指令不可用或逾時（CI 的 macOS runner 上 `lsof` 出了名的慢，回 `None`）就略過並印原因，
    /// 不把「機器慢」當成程式錯；能讀到就一定要對。
    #[tokio::test]
    async fn probe_smoke_real_ps_and_lsof_see_this_process_listening() {
        let env = crate::testing::env().await;
        let probe = crate::pane_probe::Real;
        let me = i32::try_from(std::process::id()).unwrap();
        let dump = match crate::pane_probe::PaneProbe::dump(&probe, &env.app, "local").await {
            Ok(d) => d,
            Err(e) => return eprintln!("略過：`ps` 讀不到（{e}）"),
        };
        let Some(facts) = crate::memproc::pane_facts_for_shell(&dump, "w1:p1", me) else {
            let head: String = dump.lines().take(3).collect::<Vec<_>>().join(" | ");
            return eprintln!("略過：這台機器的 `ps` 輸出裡找不到本行程 {me}（前三行：{head}）");
        };
        assert!(facts.pids.contains(&me));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        match crate::pane_probe::PaneProbe::listen_ports(&probe, "local", &[me]).await {
            None => eprintln!("略過：`lsof` 起不來或逾時（10 秒）——這台機器讀不到 port"),
            Some(ports) => assert!(ports.contains(&port), "lsof 讀得到就要看到 {port}：{ports:?}"),
        }
        drop(listener);
    }
}
