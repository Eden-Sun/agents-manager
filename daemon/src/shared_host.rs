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

#[cfg(test)]
pub(crate) mod tests {
    //! 每道守衛各有一條會因為拿掉它而紅的測試；共用與不共用兩種都跑，看得出差別只在旗標。
    use crate::db;
    use crate::herdr::HerdrClient;
    use crate::state::App;
    use crate::testing as tt;
    use serde_json::{json, Value};
    use std::sync::Arc;

    pub(crate) const HOST: &str = "sh1";

    /// 另一顆 daemon 也在用的遠端主機：自己的一個 mock herdr，設定裡 `shared_session = true`。
    pub(crate) struct Shared {
        pub herdr: tt::MockHerdr,
        pub client: HerdrClient,
        pub project_id: String,
    }

    pub(crate) async fn shared_host(env: &tt::Env, shared: bool) -> Shared {
        let app = &env.app;
        let sock = env.dir.join("sh1.sock");
        let herdr = tt::MockHerdr::start(sock.clone());
        let cfg = crate::config::HostCfg {
            name: HOST.into(),
            ssh: "sh1.invalid".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "test".into(),
            remote_path: String::new(),
            shared_session: shared,
        };
        let conn = app.hosts.insert_remote_with_client_for_test(cfg.clone(), HerdrClient::new(sock.clone())).await;
        conn.connected.store(true, std::sync::atomic::Ordering::SeqCst);
        app.cfg
            .update(move |c| {
                c.hosts.retain(|h| h.name != HOST);
                c.hosts.push(cfg);
                Ok(())
            })
            .await
            .unwrap();
        let project_id = db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, '/tmp/sh', 'sh', ?, ?)")
            .bind(&project_id)
            .bind(HOST)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        Shared { herdr, client: HerdrClient::new(sock), project_id }
    }

    pub(crate) async fn set_shared(app: &Arc<App>, shared: bool) {
        app.cfg
            .update(move |c| {
                for h in c.hosts.iter_mut().filter(|h| h.name == HOST) {
                    h.shared_session = shared;
                }
                Ok(())
            })
            .await
            .unwrap();
    }

    fn entry(name: &str, ws: &str, tab: &str, pane: &str) -> Value {
        json!({"name": name, "agent": "claude", "agent_status": "idle",
               "workspace_id": ws, "tab_id": tab, "pane_id": pane, "cwd": "/tmp/sh"})
    }

    async fn run_row(app: &Arc<App>, bot: &str, state: &str, ws: &str, tab: &str, pane: &str, agent: &str) {
        let ended = (state != "running").then(|| db::iso_in(-600));
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, started_at, ended_at)
             VALUES (?,?,?,'idle',?,?,?,?,'test',?,?)",
        )
        .bind(db::ulid())
        .bind(bot)
        .bind(state)
        .bind(ws)
        .bind(tab)
        .bind(pane)
        .bind(agent)
        .bind(db::iso_in(-1200))
        .bind(ended)
        .execute(&app.db)
        .await
        .unwrap();
    }

    async fn children(app: &Arc<App>) -> Vec<String> {
        sqlx::query_scalar("SELECT name FROM bots WHERE managed_by = 'child' AND deleted_at IS NULL ORDER BY name")
            .fetch_all(&app.db)
            .await
            .unwrap()
    }

    /// 別顆 daemon 的 agent 名字剛好是 `<我的 bot>-<字尾>`、開在它自己的 workspace：共用主機上不認領；
    /// 同名前綴、開在我自己 workspace 裡的照常認領。不共用時兩顆都認領（前綴跨 tab 本來就算）。
    #[tokio::test]
    async fn a_foreign_agent_with_our_prefix_is_not_claimed_on_a_shared_host() {
        let env = tt::env().await;
        let app = env.app.clone();
        let sh = shared_host(&env, true).await;
        let (mine, root) = sh.client.workspace_create("/tmp/sh", "sh", json!({})).await.unwrap();
        let kid = sh.client.tab_create(&mine.workspace_id, "/tmp/sh", "kid", json!({})).await.unwrap();
        let (theirs, their_root) = sh.client.workspace_create("/tmp/x", "x", json!({})).await.unwrap();
        let bot = tt::claude_bot(&app, &sh.project_id, "alfa").await;
        let agent = crate::config::agent_name("sh", &bot.id);
        run_row(&app, &bot.id, "running", &mine.workspace_id, &root.tab_id, &root.pane_id, &agent).await;
        *sh.herdr.agents.lock().unwrap() = vec![
            entry(&agent, &mine.workspace_id, &root.tab_id, &root.pane_id),
            entry(&format!("{agent}-ui"), &mine.workspace_id, &kid.tab_id, &kid.pane_id),
            entry(&format!("{agent}-x"), &theirs.workspace_id, &their_root.tab_id, &their_root.pane_id),
        ];

        crate::reconcile::reconcile_host(&app, HOST).await.unwrap();
        assert_eq!(children(&app).await, ["ui"], "只認領自己 workspace 裡的");

        set_shared(&app, false).await;
        crate::reconcile::reconcile_host(&app, HOST).await.unwrap();
        assert_eq!(children(&app).await, ["ui", "x"], "不共用時前綴跨 workspace 照舊算");
    }

    /// 結束的 run 的 pane id 被別顆 daemon 的新 pane 用到（在它的 workspace 裡）：共用主機上不當孤兒關。
    #[tokio::test]
    async fn a_reused_pane_id_in_a_foreign_workspace_is_not_closed_on_a_shared_host() {
        let env = tt::env().await;
        let app = env.app.clone();
        let sh = shared_host(&env, true).await;
        let (theirs, their_root) = sh.client.workspace_create("/tmp/x", "x", json!({})).await.unwrap();
        let bot = tt::claude_bot(&app, &sh.project_id, "alfa").await;
        run_row(&app, &bot.id, "exited", "w-old", "t-old", &their_root.pane_id, "gone").await;
        sh.herdr.agents.lock().unwrap().clear();

        crate::reconcile::reconcile_host(&app, HOST).await.unwrap();
        assert!(sh.client.pane_get(&their_root.pane_id).await.unwrap().is_some(), "別人的 pane 不關");

        set_shared(&app, false).await;
        crate::reconcile::reconcile_host(&app, HOST).await.unwrap();
        assert!(sh.client.pane_get(&their_root.pane_id).await.unwrap().is_none(), "不共用時照舊當孤兒關");
        let _ = theirs;
    }

    /// pane 掃描：共用主機上別人的 pane 不進 `panes`（不 GC、不通知、不當 scratch）；自己 workspace 的照收。
    #[tokio::test]
    async fn only_our_panes_are_scanned_on_a_shared_host() {
        let env = tt::env().await;
        let app = env.app.clone();
        let sh = shared_host(&env, true).await;
        sqlx::query("UPDATE projects SET workspace_id = 'wM' WHERE id = ?").bind(&sh.project_id).execute(&app.db).await.unwrap();
        let pane = |id: &str, ws: &str| json!({"pane_id": id, "workspace_id": ws, "tab_id": format!("{ws}:t1"), "cwd": "/home/u", "agent": null, "revision": 1});
        let snapshot = json!({
            "workspaces": [{"workspace_id": "wM", "label": "sh"}, {"workspace_id": "wX", "label": "x"}],
            "panes": [pane("wM:p1", "wM"), pane("wX:p1", "wX")],
        });
        crate::panes::scan_snapshot(&app, HOST, &snapshot).await.unwrap();
        let ids: Vec<String> = sqlx::query_scalar("SELECT pane_id FROM panes ORDER BY pane_id").fetch_all(&app.db).await.unwrap();
        assert_eq!(ids, ["wM:p1"]);
        crate::panes::notify_unowned_and_orphans(&app, HOST).await.unwrap();
        let notified: Vec<String> =
            sqlx::query_scalar("SELECT pane_id FROM panes WHERE unowned_notified_at IS NOT NULL").fetch_all(&app.db).await.unwrap();
        assert_eq!(notified, ["wM:p1"], "別人的 pane 不推 pane_unowned");

        set_shared(&app, false).await;
        crate::panes::scan_snapshot(&app, HOST, &snapshot).await.unwrap();
        let ids: Vec<String> = sqlx::query_scalar("SELECT pane_id FROM panes ORDER BY pane_id").fetch_all(&app.db).await.unwrap();
        assert_eq!(ids, ["wM:p1", "wX:p1"]);
    }

    /// 遠端 bot 目錄：共用主機上一律不搬（掃描不動、刪除時那一趟也不動、不記成欠著）。
    #[tokio::test]
    async fn remote_bot_dirs_are_left_alone_on_a_shared_host() {
        let env = tt::env().await;
        let app = env.app.clone();
        let sh = shared_host(&env, true).await;
        let bot = tt::claude_bot(&app, &sh.project_id, "alfa").await;
        sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?").bind(db::now()).bind(&bot.id).execute(&app.db).await.unwrap();

        assert_eq!(crate::remote_purge::sweep(&app, HOST).await, (0, 0));
        assert!(!crate::lifecycle::purge_bot_dir(&app, &bot.id, HOST).await);
        let marks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM remote_bot_dir_purges").fetch_one(&app.db).await.unwrap();
        assert_eq!(marks, 0, "沒去碰，也不記成欠著");
    }

    /// 移交出去的專案（#708）的 bot 目錄：刪除那一趟與開機清掃都不動（接手的 daemon 可能在用）。
    #[tokio::test]
    async fn a_handed_off_bots_dir_is_never_purged_here() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
        let dir = app.bot_dir(&bot.id).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        sqlx::query("UPDATE projects SET handed_off_to = 'agm-host' WHERE id = ?").bind(&env.project_id).execute(&app.db).await.unwrap();
        assert!(!crate::lifecycle::purge_bot_dir(&app, &bot.id, crate::config::LOCAL_HOST).await);
        assert!(dir.exists());

        sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?").bind(db::now()).bind(&bot.id).execute(&app.db).await.unwrap();
        crate::lifecycle::purge_deleted_bot_dirs(&app).await;
        assert!(dir.exists(), "開機清掃也不動");

        sqlx::query("UPDATE projects SET handed_off_to = NULL WHERE id = ?").bind(&env.project_id).execute(&app.db).await.unwrap();
        crate::lifecycle::purge_deleted_bot_dirs(&app).await;
        assert!(!dir.exists(), "收回之後照舊清");
    }

    /// 探測標記：共用主機才有，重啟後不變（存在資料目錄）。
    #[tokio::test]
    async fn the_probe_tag_is_only_for_shared_hosts_and_survives_a_restart() {
        let env = tt::env().await;
        let app = env.app.clone();
        shared_host(&env, true).await;
        let tag = super::probe_tag(&app, HOST).await.expect("共用主機有標記");
        assert_eq!(super::probe_tag(&app, HOST).await.as_deref(), Some(tag.as_str()));
        assert_eq!(super::daemon_tag(&app.data_dir), tag, "存在資料目錄，重啟後同一個");
        assert_eq!(super::probe_tag(&app, crate::config::LOCAL_HOST).await, None);
        set_shared(&app, false).await;
        assert_eq!(super::probe_tag(&app, HOST).await, None);
    }

    /// 「自己的」除了 run 與專案 workspace，還有剛開的 child（spawn hint）、預覽 pane、自己開的 host shell；
    /// 移交出去的專案（#708）不算。
    #[tokio::test]
    async fn owned_covers_hints_previews_and_host_shells_but_not_handed_off_projects() {
        let env = tt::env().await;
        let app = env.app.clone();
        let sh = shared_host(&env, true).await;
        let bot = tt::claude_bot(&app, &sh.project_id, "alfa").await;
        sqlx::query("INSERT INTO spawn_hints (pane_id, host, bot_id, created_at) VALUES ('wK:p1', ?, ?, ?)")
            .bind(HOST)
            .bind(&bot.id)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO bot_previews (bot_id, host, pane_id, status, updated_at) VALUES (?, ?, 'wP:p1', 'running', ?)")
            .bind(&bot.id)
            .bind(HOST)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        app.host_shells.lock().await.push(crate::api::shell::HostShell {
            host: HOST.into(),
            herdr_session: "test".into(),
            workspace_id: "wS".into(),
            tab_id: "wS:t1".into(),
            pane_id: "wS:p1".into(),
            cwd: "/tmp".into(),
            created_at: db::now(),
        });
        let theirs = db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, workspace_id, handed_off_to, created_at) VALUES (?, '/tmp/t', 't', ?, 'wT', 'agm-host', ?)")
            .bind(&theirs)
            .bind(HOST)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let o = super::owned(&app, HOST).await.unwrap();
        for pane in ["wK:p1", "wP:p1", "wS:p1"] {
            assert!(o.panes.contains(pane), "{pane}");
        }
        assert!(o.workspaces.contains("wS"));
        assert!(!o.workspaces.contains("wT"), "移交出去的專案不是自己的");
    }

    /// Handoff removes a project's workspace and active runs from the local ownership set. Its
    /// auxiliary pane records must be filtered by the same ownership boundary on shared sessions.
    #[tokio::test]
    async fn owned_excludes_hints_and_previews_from_handed_off_projects() {
        let env = tt::env().await;
        let app = env.app.clone();
        let sh = shared_host(&env, true).await;
        let mine = tt::claude_bot(&app, &sh.project_id, "mine").await;
        let handed_off = db::ulid();
        sqlx::query(
            "INSERT INTO projects (id, path, label, host, handed_off_to, created_at) VALUES (?, '/tmp/handed-off', 'old', ?, 'other-daemon', ?)",
        )
        .bind(&handed_off)
        .bind(HOST)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let theirs = tt::claude_bot(&app, &handed_off, "theirs").await;
        for (bot, pane) in [(&mine, "wM:p1"), (&theirs, "wT:p1")] {
            sqlx::query("INSERT INTO spawn_hints (pane_id, host, bot_id, created_at) VALUES (?, ?, ?, ?)")
                .bind(pane)
                .bind(HOST)
                .bind(&bot.id)
                .bind(db::now())
                .execute(&app.db)
                .await
                .unwrap();
            sqlx::query("INSERT INTO bot_previews (bot_id, host, pane_id, status, updated_at) VALUES (?, ?, ?, 'running', ?)")
                .bind(&bot.id)
                .bind(HOST)
                .bind(pane.replace(":p", ":preview"))
                .bind(db::now())
                .execute(&app.db)
                .await
                .unwrap();
        }

        let owned = super::owned(&app, HOST).await.unwrap();
        assert!(owned.panes.contains("wM:p1"), "own spawn hint stays owned");
        assert!(owned.panes.contains("wM:preview1"), "own preview stays owned");
        assert!(!owned.panes.contains("wT:p1"), "handed-off spawn hint belongs to the other daemon");
        assert!(!owned.panes.contains("wT:preview1"), "handed-off preview belongs to the other daemon");
    }

    #[test]
    fn only_our_tagged_probes_are_sweepable_on_a_shared_host() {
        let is_probe = |l: &str| l.starts_with("am-quota-claude");
        assert!(super::sweepable("am-quota-claude", None, is_probe), "一般主機照舊全清");
        assert!(super::sweepable("am-quota-claude@x", None, is_probe));
        assert!(super::sweepable("am-quota-claude-cc1@mine", Some("mine"), is_probe));
        assert!(!super::sweepable("am-quota-claude@other", Some("mine"), is_probe));
        assert!(!super::sweepable("am-quota-claude", Some("mine"), is_probe), "沒標記的可能是別顆舊版 daemon 的");
        assert!(!super::sweepable("proj@mine", Some("mine"), is_probe));
    }
}
