//! 專案移交（#708，SPEC §6.5h）：`projects.handed_off_to` 有值＝這個專案已經交給另一台主機的 daemon 管。
//!
//! 這顆 daemon 對它的 bot／pane 一律不動：不對帳（不建／結束 run、不收編、不退役 child）、不當子 agent 認領的
//! 父候選、不關 pane／tab、不 GC、不送 prompt／佇列、hook 進來記一行就丟、不開不停不重啟，也不打任何 pane RPC
//! （連讀畫面都不讀）。設定當下不停也不關任何東西：agent 繼續跑，交給對方接；清掉旗標＝收回，下一輪對帳照常接手。
//!
//! 守衛放在收口的地方，一處擋一整類：`mark_run_exited`（結束 run 的唯一寫入）、`child_retire::retire`（軟刪 child 的
//! 唯一寫入）、`client_for_run`／`App::herdr_for_run`（pane RPC）、start／stop／restart／prompt 的入口、
//! `flush_queued_locked`、`hookrecv::process_locked`、對帳的逐 bot 迴圈與子 agent 認領、`panes::scan_snapshot`。

use crate::lc_error::{LcError, LcResult};
use anyhow::Result;
use serde_json::json;
use sqlx::{SqliteConnection, SqlitePool};
use std::collections::HashSet;

/// 這顆 bot 所屬專案移交給誰；`None`＝這顆 daemon 管（bot 不存在也是 `None`，由呼叫端自己的 not-found 處理）。
pub async fn bot_handed_off_to(pool: &SqlitePool, bot_id: &str) -> Result<Option<String>> {
    Ok(sqlx::query_scalar::<_, Option<String>>(
        "SELECT p.handed_off_to FROM bots b JOIN projects p ON p.id = b.project_id WHERE b.id = ?",
    )
    .bind(bot_id)
    .fetch_optional(pool)
    .await?
    .flatten())
}

/// Transaction-scoped variant for callers that must keep the handoff check atomic with a write.
pub async fn bot_handed_off_to_on(conn: &mut SqliteConnection, bot_id: &str) -> Result<Option<String>> {
    Ok(sqlx::query_scalar::<_, Option<String>>(
        "SELECT p.handed_off_to FROM bots b JOIN projects p ON p.id = b.project_id WHERE b.id = ?",
    )
    .bind(bot_id)
    .fetch_optional(&mut *conn)
    .await?
    .flatten())
}

/// 409 `handed_off`：這個專案歸另一台主機的 daemon 管。讀不到就不做（502），不當成「沒移交」。
pub async fn refuse(pool: &SqlitePool, bot_id: &str) -> LcResult<()> {
    match bot_handed_off_to(pool, bot_id).await {
        Ok(None) => Ok(()),
        Ok(Some(host)) => Err(LcError::conflict(
            "handed_off",
            json!({"bot_id": bot_id, "handed_off_to": host,
                   "message": format!("這個專案已移交給 {host} 的 daemon 管理，這裡不替它開、關、送 prompt 或操作 pane；要收回請先清掉專案的「已移交」。")}),
        )),
        Err(e) => Err(LcError::Upstream(format!("cannot tell whether bot `{bot_id}` was handed off: {e}"))),
    }
}

/// 一台主機上所有已移交專案看得見的足跡：對帳認領 child 與 pane 掃描拿它排除別人的東西。
#[derive(Debug, Default)]
pub struct Footprint {
    pub workspaces: HashSet<String>,
    pub tabs: HashSet<String>,
    pub panes: HashSet<String>,
    /// 它們的 bot 在 herdr 上可能用的名字（算出來的名字＋每個 run 記過的名字）。
    pub names: HashSet<String>,
}

impl Footprint {
    pub fn covers_agent(&self, a: &crate::herdr::AgentInfo) -> bool {
        a.name.as_deref().is_some_and(|n| self.names.contains(n))
            || self.panes.contains(&a.pane_id)
            || self.tabs.contains(&a.tab_id)
            || self.workspaces.contains(&a.workspace_id)
    }

    /// `session.snapshot` 的一顆 pane。
    pub fn covers_pane(&self, p: &serde_json::Value) -> bool {
        let field = |k: &str| p.get(k).and_then(serde_json::Value::as_str);
        field("pane_id").is_some_and(|v| self.panes.contains(v))
            || field("tab_id").is_some_and(|v| self.tabs.contains(v))
            || field("workspace_id").is_some_and(|v| self.workspaces.contains(v))
    }
}

pub async fn footprint(pool: &SqlitePool, host: &str) -> Result<Footprint> {
    let mut fp = Footprint::default();
    let projects = crate::db::live_projects(pool).await?;
    for p in projects.iter().filter(|p| p.host == host && p.handed_off_to.is_some()) {
        if let Some(ws) = p.workspace_id.clone() {
            fp.workspaces.insert(ws);
        }
        let bots: Vec<crate::db::Bot> =
            sqlx::query_as("SELECT * FROM bots WHERE project_id = ? AND deleted_at IS NULL").bind(&p.id).fetch_all(pool).await?;
        for b in &bots {
            fp.names.insert(crate::config::agent_name(&p.label, &b.id));
        }
        type RunRow = (Option<String>, Option<String>, Option<String>, Option<String>, bool);
        let runs: Vec<RunRow> = sqlx::query_as(
            "SELECT r.agent_name, r.pane_id, r.tab_id, r.workspace_id, r.state IN ('starting','running','stopping')
               FROM runs r JOIN bots b ON b.id = r.bot_id WHERE b.project_id = ? AND b.deleted_at IS NULL",
        )
        .bind(&p.id)
        .fetch_all(pool)
        .await?;
        for (name, pane, tab, ws, active) in runs {
            fp.names.extend(name.filter(|n| !n.is_empty()));
            // 結束的 run 的 pane id 會被 herdr 重用（#469），只有活著的才算它的。
            if active {
                fp.panes.extend(pane);
                fp.tabs.extend(tab);
                fp.workspaces.extend(ws);
            }
        }
    }
    Ok(fp)
}

#[cfg(all(test, feature = "daemon-test-harness"))]
mod tests {
    //! 每道守衛各有一條會因為拿掉它而紅的測試；同一條也驗「清掉旗標就恢復」。
    use crate::config::LOCAL_HOST;
    use crate::db;
    use crate::herdr::HerdrClient;
    use crate::lifecycle::{LcError, RunExit};
    use crate::state::App;
    use crate::testing as tt;
    use serde_json::{json, Value};
    use std::sync::Arc;

    async fn hand_off(app: &Arc<App>, project_id: &str, to: Option<&str>) {
        sqlx::query("UPDATE projects SET handed_off_to = ? WHERE id = ?").bind(to).bind(project_id).execute(&app.db).await.unwrap();
    }

    fn client(env: &tt::Env) -> HerdrClient {
        HerdrClient::new(env.dir.join("data/herdr.sock"))
    }

    fn entry(name: &str, ws: &str, tab: &str, pane: &str) -> Value {
        json!({"name": name, "agent": "claude", "agent_status": "idle",
               "workspace_id": ws, "tab_id": tab, "pane_id": pane, "cwd": "/tmp/p"})
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_row(app: &Arc<App>, bot: &str, state: &str, ws: &str, tab: &str, pane: &str, agent: &str) -> String {
        let id = db::ulid();
        let ended = (state != "running").then(|| db::iso_in(-600));
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, started_at, ended_at)
             VALUES (?,?,?,'idle',?,?,?,?,'test',?,?)",
        )
        .bind(&id)
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
        id
    }

    async fn other_project(app: &Arc<App>, label: &str, host: &str) -> String {
        let id = db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?,?,?,?,?)")
            .bind(&id)
            .bind(format!("/tmp/{label}"))
            .bind(label)
            .bind(host)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        id
    }

    async fn children(app: &Arc<App>) -> Vec<String> {
        sqlx::query_scalar("SELECT name FROM bots WHERE managed_by = 'child' AND deleted_at IS NULL ORDER BY name")
            .fetch_all(&app.db)
            .await
            .unwrap()
    }

    fn reason(e: LcError) -> String {
        match e {
            LcError::Conflict(v) => v["reason"].as_str().unwrap_or_default().to_string(),
            other => format!("{other:?}"),
        }
    }

    async fn run_state(app: &Arc<App>, run: &str) -> String {
        db::run(&app.db, run).await.unwrap().unwrap().state
    }

    /// 對帳的逐 bot 迴圈：agent 出現在 agent.list 上也不收編；收回之後下一輪照常收編。
    #[tokio::test]
    async fn an_agent_of_a_handed_off_bot_is_not_adopted_until_the_project_is_taken_back() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (ws, root) = client(&env).workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
        let agent = crate::config::agent_name("proj", &bot.id);
        *env.herdr.agents.lock().unwrap() = vec![entry(&agent, &ws.workspace_id, &root.tab_id, &root.pane_id)];
        hand_off(&app, &env.project_id, Some("agm-host")).await;

        crate::reconcile::reconcile_host(&app, LOCAL_HOST).await.unwrap();
        assert!(db::active_run(&app.db, &bot.id).await.unwrap().is_none(), "移交出去的 bot 不收編");

        hand_off(&app, &env.project_id, None).await;
        crate::reconcile::reconcile_host(&app, LOCAL_HOST).await.unwrap();
        assert!(db::active_run(&app.db, &bot.id).await.unwrap().is_some(), "收回之後照常收編");
    }

    /// agent 從 agent.list 消失：run 照舊是 running（交給對方接，這裡不收）。
    #[tokio::test]
    async fn a_handed_off_run_whose_agent_left_the_list_stays_running() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (ws, root) = client(&env).workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
        let agent = crate::config::agent_name("proj", &bot.id);
        let run = run_row(&app, &bot.id, "running", &ws.workspace_id, &root.tab_id, &root.pane_id, &agent).await;
        env.herdr.agents.lock().unwrap().clear();
        hand_off(&app, &env.project_id, Some("agm-host")).await;

        crate::reconcile::reconcile_host(&app, LOCAL_HOST).await.unwrap();
        assert_eq!(run_state(&app, &run).await, "running");
    }

    /// `mark_run_exited` 是結束 run 的唯一寫入：pane 關閉事件、dead-pane 巡邏、default session 都經過它。
    #[tokio::test]
    async fn a_handed_off_run_is_not_ended_by_anyone_here() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
        let run = tt::fake_run(&app, &bot.id).await;
        hand_off(&app, &env.project_id, Some("agm-host")).await;
        assert_eq!(crate::lifecycle::mark_run_exited(&app, &run, "pane exited").await, RunExit::AlreadyEnded);
        assert_eq!(run_state(&app, &run).await, "running");

        hand_off(&app, &env.project_id, None).await;
        assert_ne!(crate::lifecycle::mark_run_exited(&app, &run, "pane exited").await, RunExit::AlreadyEnded);
        assert_eq!(run_state(&app, &run).await, "exited");
    }

    /// child 的隱式退役（對帳、維護收尾）不動移交出去的專案。
    #[tokio::test]
    async fn a_handed_off_child_is_not_retired() {
        let env = tt::env().await;
        let app = env.app.clone();
        let parent = tt::claude_bot(&app, &env.project_id, "alfa").await;
        let kid = tt::claude_bot(&app, &env.project_id, "kid").await;
        sqlx::query("UPDATE bots SET managed_by = 'child', parent_bot_id = ? WHERE id = ?")
            .bind(&parent.id)
            .bind(&kid.id)
            .execute(&app.db)
            .await
            .unwrap();
        hand_off(&app, &env.project_id, Some("agm-host")).await;
        let out = crate::child_retire::retire(&app, &kid.id, "reconcile_agent_gone", crate::child_retire::Mode::Implicit).await.unwrap();
        assert_eq!(out, crate::child_retire::Outcome::HandedOff);
        assert!(db::bot(&app.db, &kid.id).await.unwrap().unwrap().deleted_at.is_none());

        hand_off(&app, &env.project_id, None).await;
        let out = crate::child_retire::retire(&app, &kid.id, "reconcile_agent_gone", crate::child_retire::Mode::Implicit).await.unwrap();
        assert_eq!(out, crate::child_retire::Outcome::Retired);
    }

    /// 移交出去的 bot 的 agent 被搬進別顆 bot 的 tab：不因為「同 tab」被收成那顆的 child。
    #[tokio::test]
    async fn a_handed_off_agent_in_another_bots_tab_is_not_claimed_as_its_child() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (ws, root) = client(&env).workspace_create("/tmp/o", "other", json!({})).await.unwrap();
        let moved = client(&env).pane_split(&root.pane_id, "right", "/tmp/o", json!({})).await.unwrap();
        let other = other_project(&app, "other", LOCAL_HOST).await;
        let host_bot = tt::claude_bot(&app, &other, "bravo").await;
        let host_agent = crate::config::agent_name("other", &host_bot.id);
        run_row(&app, &host_bot.id, "running", &ws.workspace_id, &root.tab_id, &root.pane_id, &host_agent).await;
        let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
        let agent = crate::config::agent_name("proj", &bot.id);
        *env.herdr.agents.lock().unwrap() = vec![
            entry(&host_agent, &ws.workspace_id, &root.tab_id, &root.pane_id),
            entry(&agent, &ws.workspace_id, &moved.tab_id, &moved.pane_id),
        ];
        hand_off(&app, &env.project_id, Some("agm-host")).await;

        crate::reconcile::reconcile_host(&app, LOCAL_HOST).await.unwrap();
        assert!(children(&app).await.is_empty(), "不是 bravo 的 child");
    }

    /// 移交出去的 bot 開的 `<名字>-<字尾>` agent：沒人認領（它不是父候選）。
    #[tokio::test]
    async fn an_unclaimed_prefixed_agent_of_a_handed_off_bot_is_left_alone() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (ws, root) = client(&env).workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let kid_pane = client(&env).pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
        let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
        let agent = crate::config::agent_name("proj", &bot.id);
        run_row(&app, &bot.id, "running", &ws.workspace_id, &root.tab_id, &root.pane_id, &agent).await;
        *env.herdr.agents.lock().unwrap() = vec![
            entry(&agent, &ws.workspace_id, &root.tab_id, &root.pane_id),
            entry(&format!("{agent}-ui"), &ws.workspace_id, &kid_pane.tab_id, &kid_pane.pane_id),
        ];
        hand_off(&app, &env.project_id, Some("agm-host")).await;

        crate::reconcile::reconcile_host(&app, LOCAL_HOST).await.unwrap();
        assert!(children(&app).await.is_empty());

        hand_off(&app, &env.project_id, None).await;
        crate::reconcile::reconcile_host(&app, LOCAL_HOST).await.unwrap();
        assert_eq!(children(&app).await, ["ui"], "收回之後照常認領");
    }

    /// workspace 從 snapshot 消失：移交出去的專案的映射不清。
    #[tokio::test]
    async fn a_handed_off_projects_workspace_mapping_is_kept() {
        let env = tt::env().await;
        let app = env.app.clone();
        sqlx::query("UPDATE projects SET workspace_id = 'w-gone' WHERE id = ?").bind(&env.project_id).execute(&app.db).await.unwrap();
        hand_off(&app, &env.project_id, Some("agm-host")).await;
        crate::reconcile::reconcile_host(&app, LOCAL_HOST).await.unwrap();
        assert_eq!(db::project(&app.db, &env.project_id).await.unwrap().unwrap().workspace_id.as_deref(), Some("w-gone"));

        hand_off(&app, &env.project_id, None).await;
        crate::reconcile::reconcile_host(&app, LOCAL_HOST).await.unwrap();
        assert!(db::project(&app.db, &env.project_id).await.unwrap().unwrap().workspace_id.is_none());
    }

    /// herdr 的 `workspace.closed` 事件：一樣不清移交出去的專案的映射。
    #[tokio::test]
    async fn a_workspace_closed_event_keeps_a_handed_off_projects_mapping() {
        let env = tt::env().await;
        let app = env.app.clone();
        sqlx::query("UPDATE projects SET workspace_id = 'w9' WHERE id = ?").bind(&env.project_id).execute(&app.db).await.unwrap();
        hand_off(&app, &env.project_id, Some("agm-host")).await;
        let ev = crate::herdr::Event { event: "workspace_closed".into(), data: json!({"workspace_id": "w9"}) };
        crate::runners::events::handle_global(&app, LOCAL_HOST, "test", &ev).await;
        assert_eq!(db::project(&app.db, &env.project_id).await.unwrap().unwrap().workspace_id.as_deref(), Some("w9"));

        hand_off(&app, &env.project_id, None).await;
        crate::runners::events::handle_global(&app, LOCAL_HOST, "test", &ev).await;
        assert!(db::project(&app.db, &env.project_id).await.unwrap().unwrap().workspace_id.is_none());
    }

    /// 結束的 run 留下的 pane：移交出去的專案不當孤兒關。
    #[tokio::test]
    async fn a_handed_off_projects_leftover_pane_is_not_closed_as_an_orphan() {
        let env = tt::env().await;
        let app = env.app.clone();
        let c = client(&env);
        let (ws, _root) = c.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let left = c.tab_create(&ws.workspace_id, "/tmp/p", "left", json!({})).await.unwrap();
        let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
        run_row(&app, &bot.id, "exited", &ws.workspace_id, &left.tab_id, &left.pane_id, "gone").await;
        env.herdr.agents.lock().unwrap().clear();
        hand_off(&app, &env.project_id, Some("agm-host")).await;

        crate::reconcile::reconcile_host(&app, LOCAL_HOST).await.unwrap();
        assert!(c.pane_get(&left.pane_id).await.unwrap().is_some(), "不關");
    }

    /// pane 掃描：移交出去的專案 workspace 裡的 shell 不進 `panes`（不 GC、不通知、不當 scratch）。
    #[tokio::test]
    async fn a_handed_off_projects_panes_are_not_scanned() {
        let env = tt::env().await;
        let app = env.app.clone();
        let theirs = other_project(&app, "theirs", "zz92").await;
        sqlx::query("UPDATE projects SET workspace_id = 'wF', handed_off_to = 'agm-host' WHERE id = ?")
            .bind(&theirs)
            .execute(&app.db)
            .await
            .unwrap();
        let pane = |id: &str, ws: &str| json!({"pane_id": id, "workspace_id": ws, "tab_id": format!("{ws}:t1"), "cwd": "/home/u", "agent": null, "revision": 1});
        let snapshot = json!({
            "workspaces": [{"workspace_id": "w1", "label": "mine"}, {"workspace_id": "wF", "label": "theirs"}],
            "panes": [pane("w1:p1", "w1"), pane("wF:p1", "wF")],
        });
        crate::panes::scan_snapshot(&app, "zz92", &snapshot).await.unwrap();
        let ids: Vec<String> = sqlx::query_scalar("SELECT pane_id FROM panes ORDER BY pane_id").fetch_all(&app.db).await.unwrap();
        assert_eq!(ids, ["w1:p1"]);
    }

    /// pane 狀態事件：移交出去的 run 的狀態不寫、不走任何邊（外部回合、備援、通知 parent）。
    #[tokio::test]
    async fn a_status_event_for_a_handed_off_run_is_ignored() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
        let run = tt::fake_run(&app, &bot.id).await;
        hand_off(&app, &env.project_id, Some("agm-host")).await;
        let ev = crate::herdr::Event {
            event: "pane_agent_status_changed".into(),
            data: json!({"pane_id": format!("pane-{}", bot.id), "agent_status": "blocked"}),
        };
        crate::runners::events::handle_status(&app, LOCAL_HOST, "test", &ev).await;
        assert_eq!(db::run(&app.db, &run).await.unwrap().unwrap().agent_status, "idle");

        hand_off(&app, &env.project_id, None).await;
        crate::runners::events::handle_status(&app, LOCAL_HOST, "test", &ev).await;
        assert_eq!(db::run(&app.db, &run).await.unwrap().unwrap().agent_status, "blocked");
    }

    #[tokio::test]
    async fn a_handed_off_bot_is_not_started() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
        hand_off(&app, &env.project_id, Some("agm-host")).await;
        assert_eq!(reason(crate::lifecycle::start_bot(&app, &bot.id).await.unwrap_err()), "handed_off");
    }

    /// stop 也擋住閒置回收、批次重啟、restart 的前半段。
    #[tokio::test]
    async fn a_handed_off_bot_is_not_stopped() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
        let run = tt::fake_run(&app, &bot.id).await;
        hand_off(&app, &env.project_id, Some("agm-host")).await;
        assert_eq!(reason(crate::lifecycle::stop_bot(&app, &bot.id).await.unwrap_err()), "handed_off");
        assert_eq!(reason(crate::lifecycle::restart_bot(&app, &bot.id).await.unwrap_err()), "handed_off");
        assert_eq!(run_state(&app, &run).await, "running");
        // 沒有 run 的 stop 也擋：它會去收這顆的預覽 pane（不經過 `client_for_run`）。
        let idle = tt::claude_bot(&app, &env.project_id, "bravo").await;
        assert_eq!(reason(crate::lifecycle::stop_bot(&app, &idle.id).await.unwrap_err()), "handed_off");
    }

    /// pane RPC 的收口（keys／text／interrupt／login……）。
    #[tokio::test]
    async fn no_pane_rpc_reaches_a_handed_off_run() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        hand_off(&app, &env.project_id, Some("agm-host")).await;
        assert_eq!(reason(crate::lifecycle::send_keys(&app, &bot.id, vec!["Enter".into()], None).await.unwrap_err()), "handed_off");

        let run = db::run(&app.db, &run_id).await.unwrap().unwrap();
        assert!(app.herdr_for_run(&run).await.is_none(), "背景巡邏拿不到 client");
        hand_off(&app, &env.project_id, None).await;
        assert!(app.herdr_for_run(&run).await.is_some());
    }

    /// prompt 不收也不排隊；已經在佇列裡的原樣留著。
    #[tokio::test]
    async fn a_handed_off_bot_takes_no_prompt_and_keeps_its_queue() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
        tt::fake_run(&app, &bot.id).await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let queued = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,'web','queued','pending','派工',?)")
            .bind(&queued)
            .bind(&conv)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        hand_off(&app, &env.project_id, Some("agm-host")).await;

        assert_eq!(reason(crate::lifecycle::prompt(&app, &bot.id, "hi", "crid-1").await.unwrap_err()), "handed_off");
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns").fetch_one(&app.db).await.unwrap();
        assert_eq!(turns, 1, "沒有新回合");

        crate::lifecycle::flush_queued_locked(&app, &bot.id).await.unwrap();
        let t = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id = ?").bind(&queued).fetch_one(&app.db).await.unwrap();
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("queued", "pending"), "佇列原樣留著");
        assert_eq!((t.flush_retries, t.next_flush_at.as_deref(), t.run_id.as_deref()), (0, None, None), "沒被認領、也沒花掉重試");
    }

    /// hook 記一行就丟：連 spawn hint 都不記。
    #[tokio::test]
    async fn a_hook_for_a_handed_off_bot_is_ignored() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
        hand_off(&app, &env.project_id, Some("agm-host")).await;
        let body = crate::hookrecv::HookBody {
            bot_id: bot.id.clone(),
            provider: "claude".into(),
            payload: json!({
                "hook_event_name": "PostToolUse",
                "tool_name": "Bash",
                "tool_input": {"command": "herdr pane split"},
                "tool_response": {"stdout": r#"{"id":"cli:pane:split","result":{"pane":{"pane_id":"w1:p2"}}}"#, "stderr": ""},
            }),
            received_at: None,
            truncated: false,
            run_id: None,
        };
        crate::hookrecv::process(&app, &body).await.unwrap();
        let hints: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM spawn_hints").fetch_one(&app.db).await.unwrap();
        assert_eq!(hints, 0);
    }

    /// 手改 config.toml 寫了空字串：不是「本機管」也不是哪台主機，擋下來。
    #[test]
    fn an_empty_handed_off_to_in_config_is_rejected() {
        let mut cfg: crate::config::ConfigFile = toml::from_str(
            "[[projects]]\nid = '01M1Y75JST9RSMVKMWSKV3XHBQ'\npath = '/tmp/p'\nlabel = 'p'\nhanded_off_to = 'agm-host'\n",
        )
        .unwrap();
        crate::projection::validate(&cfg).unwrap();
        cfg.projects[0].handed_off_to = Some("  ".into());
        assert!(crate::projection::validate(&cfg).unwrap_err().to_string().contains("handed_off_to"));
    }

    /// 背景只讀 pane 的也不讀：codex 額度讀狀態列、對帳補 codex runtime。
    #[tokio::test]
    async fn no_background_pass_reads_a_handed_off_codex_pane() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
        sqlx::query("UPDATE bots SET kind = 'codex' WHERE id = ?").bind(&bot.id).execute(&app.db).await.unwrap();
        tt::fake_run(&app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        hand_off(&app, &env.project_id, Some("agm-host")).await;
        let reads = || env.herdr.calls_to("pane.read").into_iter().filter(|p| p["pane_id"] == pane.as_str()).count();

        crate::runners::quota::refresh_codex_from_panes(&app, LOCAL_HOST).await;
        assert_eq!(reads(), 0, "額度不讀它的狀態列");
        crate::reconcile::reconcile_host(&app, LOCAL_HOST).await.unwrap();
        assert_eq!(reads(), 0, "對帳不補它的 runtime");
    }

    /// default session 的匯入不收移交出去的專案。
    #[tokio::test]
    async fn the_default_session_imports_nothing_into_a_handed_off_project() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (ws, root) = client(&env).workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let mut a = entry("mine", &ws.workspace_id, &root.tab_id, &root.pane_id);
        a["cwd"] = json!(env.repo.to_string_lossy());
        *env.herdr.agents.lock().unwrap() = vec![a];
        // 匯入會寫回 config.toml，所以專案要在裡面。
        let (pid, path) = (env.project_id.clone(), env.repo.to_string_lossy().to_string());
        app.cfg
            .update(move |cfg| {
                cfg.projects = vec![crate::config::ProjectCfg {
                    id: Some(pid),
                    path,
                    label: "proj".into(),
                    host: LOCAL_HOST.into(),
                    bots: vec![],
                    handed_off_to: None,
                }];
                Ok(())
            })
            .await
            .unwrap();
        hand_off(&app, &env.project_id, Some("agm-host")).await;
        crate::default_session::sync(&app).await.unwrap();
        let imported: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bots").fetch_one(&app.db).await.unwrap();
        assert_eq!(imported, 0);

        hand_off(&app, &env.project_id, None).await;
        crate::default_session::sync(&app).await.unwrap();
        let imported: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bots").fetch_one(&app.db).await.unwrap();
        assert_eq!(imported, 1, "收回之後照常匯入");
    }
}
