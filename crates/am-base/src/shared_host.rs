//! 共用 herdr session 的遠端主機（#709，SPEC §11.10）：`[[hosts]] shared_session = true`＝另一顆 daemon 也在用這個
//! session（典型：agm-host 的 daemon 以遠端主機 `m4p` 接手 Mac 上的專案，而 Mac 自己的 daemon 本機就開著同一個
//! `agents-manager` session）。這台上這顆 daemon **只碰自己的東西**：
//!
//! - 「自己的」＝本 daemon 專案（沒移交出去的，#708）在這台的 workspace、它們 bot 活著的 run 的 workspace／tab／pane、
//!   spawn hint 記的 pane、預覽 pane、自己開的 host shell。
//! - 其他 pane／tab／workspace：不進 `panes`（不 GC、不推 `pane_unowned`／`pane_orphaned`、不當 scratch）、不當孤兒關、
//!   不被認領成 child（連名字前綴也不算）。
//! - 連線時絕不 `herdr server stop`（那是對方的 server）；額度探測 workspace 的 label 帶本 daemon 的標記，只清自己的；
//!   遠端的 bot 目錄一律不搬（資料目錄可能就是對方的）。
//!
//! 旗標讀**當下的設定**，不是連線建立時的快照：改了不必重連就生效。

use anyhow::Result;
use std::collections::HashSet;
use std::future::Future;

/// 共用主機判斷與 `owned` 需要的最小外部事實（`App` 在 `app_ports_p3` 實作）：設定旗標、DB 連線、自己開的 host shell、資料目錄。
pub trait SharedHostEnv: Send + Sync {
    /// `[[hosts]]` 裡這台有沒有設 `shared_session`（讀當下的設定）。
    fn host_flagged_shared(&self, host: &str) -> impl Future<Output = bool> + Send;
    fn db_pool(&self) -> &sqlx::SqlitePool;
    /// 這顆 daemon 在 `host` 上自己開的 host shell：`(workspace_id, pane_id)`。
    fn own_shell_panes(&self, host: &str) -> impl Future<Output = Vec<(String, String)>> + Send;
    fn data_dir(&self) -> &std::path::Path;
}

impl<T: SharedHostEnv + ?Sized> SharedHostEnv for std::sync::Arc<T> {
    fn host_flagged_shared(&self, host: &str) -> impl Future<Output = bool> + Send {
        (**self).host_flagged_shared(host)
    }
    fn db_pool(&self) -> &sqlx::SqlitePool {
        (**self).db_pool()
    }
    fn own_shell_panes(&self, host: &str) -> impl Future<Output = Vec<(String, String)>> + Send {
        (**self).own_shell_panes(host)
    }
    fn data_dir(&self) -> &std::path::Path {
        (**self).data_dir()
    }
}

/// `host` 是不是跟別的 daemon 共用 session。本機與不在設定裡的主機都是 `false`。
pub async fn is_shared(app: &impl SharedHostEnv, host: &str) -> bool {
    host != crate::config::LOCAL_HOST && app.host_flagged_shared(host).await
}

/// 這顆 daemon 在一台主機上擁有的 herdr 物件。
#[derive(Debug, Default)]
pub struct Owned {
    pub workspaces: HashSet<String>,
    pub tabs: HashSet<String>,
    pub panes: HashSet<String>,
}

impl Owned {
    pub fn covers_agent(&self, a: &crate::herdr::AgentInfo) -> bool {
        self.panes.contains(&a.pane_id) || self.tabs.contains(&a.tab_id) || self.workspaces.contains(&a.workspace_id)
    }

    /// `session.snapshot` 的一顆 pane。
    pub fn covers_pane(&self, p: &serde_json::Value) -> bool {
        let field = |k: &str| p.get(k).and_then(serde_json::Value::as_str);
        field("pane_id").is_some_and(|v| self.panes.contains(v))
            || field("tab_id").is_some_and(|v| self.tabs.contains(v))
            || field("workspace_id").is_some_and(|v| self.workspaces.contains(v))
    }
}

pub async fn owned(app: &impl SharedHostEnv, host: &str) -> Result<Owned> {
    let mut o = Owned::default();
    for p in crate::db::live_projects(app.db_pool()).await? {
        if p.host == host && p.handed_off_to.is_none() {
            o.workspaces.extend(p.workspace_id);
        }
    }
    let runs: Vec<(Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT r.workspace_id, r.tab_id, r.pane_id FROM runs r JOIN bots b ON b.id = r.bot_id JOIN projects p ON p.id = b.project_id
          WHERE p.host = ? AND p.deleted_at IS NULL AND p.handed_off_to IS NULL AND b.deleted_at IS NULL
            AND r.state IN ('starting','running','stopping')",
    )
    .bind(host)
    .fetch_all(app.db_pool())
    .await?;
    for (ws, tab, pane) in runs {
        o.workspaces.extend(ws);
        o.tabs.extend(tab);
        o.panes.extend(pane);
    }
    let hints: Vec<String> = sqlx::query_scalar(
        "SELECT h.pane_id FROM spawn_hints h
         JOIN bots b ON b.id = h.bot_id JOIN projects p ON p.id = b.project_id
         WHERE h.host = ? AND p.host = ? AND p.deleted_at IS NULL AND p.handed_off_to IS NULL
           AND b.deleted_at IS NULL AND h.created_at >= ?",
    )
    .bind(host)
    .bind(host)
    .bind(crate::spawn_hints::cutoff())
    .fetch_all(app.db_pool())
    .await?;
    o.panes.extend(hints);
    let previews: Vec<String> = sqlx::query_scalar(
        "SELECT v.pane_id FROM bot_previews v
         JOIN bots b ON b.id = v.bot_id JOIN projects p ON p.id = b.project_id
         WHERE v.host = ? AND p.host = ? AND p.deleted_at IS NULL AND p.handed_off_to IS NULL
           AND b.deleted_at IS NULL AND v.pane_id IS NOT NULL",
    )
    .bind(host)
    .bind(host)
    .fetch_all(app.db_pool())
    .await?;
    o.panes.extend(previews);
    for (workspace_id, pane_id) in app.own_shell_panes(host).await {
        o.workspaces.insert(workspace_id);
        o.panes.insert(pane_id);
    }
    Ok(o)
}

/// 本 daemon 的標記（共用主機上的額度探測 workspace label 帶著它，清殘留時只認自己的）。第一次用到時隨機產生、
/// 存在資料目錄的 `daemon-tag`，daemon 重啟後不變——開機清上一輪留下的探測才認得出來。
pub fn daemon_tag(data_dir: &std::path::Path) -> String {
    let path = data_dir.join("daemon-tag");
    if let Ok(s) = std::fs::read_to_string(&path) {
        let s = s.trim();
        if valid_tag(s) {
            return s.to_string();
        }
    }
    let tag: String = crate::db::ulid().to_ascii_lowercase().chars().rev().take(8).collect();
    if let Err(e) = std::fs::write(&path, &tag) {
        tracing::warn!(path = %path.display(), error = %e, "could not persist the daemon tag; probe leftovers from this run may be left behind after a restart");
    }
    tag
}

fn valid_tag(s: &str) -> bool {
    (4..=32).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric())
}

/// 共用主機上探測 workspace 要帶的標記；一般主機 `None`（label 照舊）。
pub async fn probe_tag(app: &impl SharedHostEnv, host: &str) -> Option<String> {
    if is_shared(app, host).await {
        Some(daemon_tag(app.data_dir()))
    } else {
        None
    }
}

/// 共用主機上探測 workspace 的 label：`<base>@<tag>`。
pub fn tagged_label(base: &str, tag: &str) -> String {
    format!("{base}@{tag}")
}

/// 清殘留時認不認這個探測 label：一般主機全是自己的（帶不帶標記都認，從共用改回來時留下的才收得掉）；
/// 共用主機只認帶自己標記的。
pub fn sweepable(label: &str, own_tag: Option<&str>, is_probe: impl Fn(&str) -> bool) -> bool {
    match own_tag {
        None => is_probe(label.rsplit_once('@').map_or(label, |(base, _)| base)),
        Some(tag) => label.strip_suffix(&format!("@{tag}")).is_some_and(is_probe),
    }
}
