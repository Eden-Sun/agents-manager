//! #188：重啟走哪一條路——子 agent 在原 pane 裡 exit + resume、其餘 stop + start——是破壞性的決定。
//! 子 agent 的 pane 是父 agent 開的，daemon 重建不了它的環境；一般路徑會把 pane 關掉再開一個新的。
//! 這裡放夾具（一顆有真 mock pane 的子 agent）與 lifecycle 這一層的守衛；批次那一層見 `bulk_restart` 的測試。
use super::*;
use crate::capabilities::Db;

pub trait RestartKindFixture: Db {
    fn project_id(&self) -> &str;
    fn herdr_client(&self) -> HerdrClient;
    fn create_parent(&self, name: &str) -> impl std::future::Future<Output = String> + Send;
    fn record_agent(&self, agent: Value);
    fn tab_has_pane(&self, tab_id: &str, pane_id: &str) -> bool;
    fn calls_to(&self, method: &str) -> Vec<Value>;
}

pub struct LiveChild {
    pub id: String,
    pub run_id: String,
    pub pane_id: String,
    pub tab_id: String,
}

/// 一顆 claude 子 agent：真的（mock）pane、agent 在裡面、run 是 `running`／`idle`。同一個 env 可以開多顆。
pub async fn live_child(env: &impl RestartKindFixture, name: &str) -> LiveChild {
    let client = env.herdr_client();
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
    let parent_id = env.create_parent(&format!("parent-{name}")).await;
    let id = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, parent_bot_id, created_at)
         VALUES (?,?,?,'claude','[]',0,0,'tok','child',?,?)",
    )
    .bind(&id)
    .bind(env.project_id())
    .bind(name)
    .bind(&parent_id)
    .bind(db::now())
    .execute(env.db())
    .await
    .unwrap();
    let run_id = db::ulid();
    let agent = format!("proj-{name}");
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, adopted, started_at)
         VALUES (?,?,'running','idle',?,?,?,?,'test',1,?)",
    )
    .bind(&run_id)
    .bind(&id)
    .bind(&ws.workspace_id)
    .bind(&pane.tab_id)
    .bind(&pane.pane_id)
    .bind(&agent)
    .bind(db::now())
    .execute(env.db())
    .await
    .unwrap();
    env.record_agent(json!({
        "name": agent, "agent": "claude", "agent_status": "idle",
        "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"}));
    LiveChild { id, run_id, pane_id: pane.pane_id, tab_id: pane.tab_id }
}

/// 子 agent 一根寒毛都沒動：run 還是 `running`、沒有結束、pane 還在、沒有任何一次會動到 pane／agent 的 herdr 呼叫指向它。
/// 只看指向它的呼叫（pane／tab／agent 名）：同一批裡別的 bot 照常送自己的 ctrl+c、關自己的 pane。
pub async fn assert_child_untouched(env: &impl RestartKindFixture, kid: &LiveChild) {
    let run = db::run(env.db(), &kid.run_id).await.unwrap().unwrap();
    assert_eq!(run.state, "running", "子 agent 的 run 被動過了");
    assert!(run.ended_at.is_none());
    assert_eq!(db::active_run(env.db(), &kid.id).await.unwrap().map(|r| r.id), Some(kid.run_id.clone()));
    assert!(env.tab_has_pane(&kid.tab_id, &kid.pane_id), "pane 被關掉了");
    let agent = run.agent_name.clone().unwrap();
    for m in ["pane.close", "tab.close", "agent.send_keys", "pane.send_keys", "agent.start"] {
        let aimed: Vec<_> = env
            .calls_to(m)
            .into_iter()
            .filter(|p| {
                let p = p.to_string();
                p.contains(&kid.pane_id) || p.contains(&kid.tab_id) || p.contains(&agent)
            })
            .collect();
        assert!(aimed.is_empty(), "{m} 動到了子 agent：{aimed:?}");
    }
}

#[cfg(all(test, feature = "daemon-test-harness"))]
impl crate::capabilities::Db for crate::testing::Env {
    fn db(&self) -> &sqlx::SqlitePool {
        &self.app.db
    }
}

#[cfg(all(test, feature = "daemon-test-harness"))]
impl crate::lifecycle::restart_kind_tests::RestartKindFixture for crate::testing::Env {
    fn project_id(&self) -> &str {
        &self.project_id
    }

    fn herdr_client(&self) -> crate::herdr::HerdrClient {
        crate::herdr::HerdrClient::new(self.dir.join("data/herdr.sock"))
    }

    async fn create_parent(&self, name: &str) -> String {
        crate::testing::claude_bot(&self.app, &self.project_id, name).await.id
    }

    fn record_agent(&self, agent: Value) {
        self.herdr.agents.lock().unwrap().push(agent);
    }

    fn tab_has_pane(&self, tab_id: &str, pane_id: &str) -> bool {
        self.herdr.tab(tab_id).unwrap().panes.iter().any(|pane| pane == pane_id)
    }

    fn calls_to(&self, method: &str) -> Vec<Value> {
        self.herdr.calls_to(method)
    }
}
