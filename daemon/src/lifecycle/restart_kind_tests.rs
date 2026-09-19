//! #188：重啟走哪一條路——子 agent 在原 pane 裡 exit + resume、其餘 stop + start——是破壞性的決定。
//! 子 agent 的 pane 是父 agent 開的，daemon 重建不了它的環境；一般路徑會把 pane 關掉再開一個新的。
//! 這裡放夾具（一顆有真 mock pane 的子 agent）與 lifecycle 這一層的守衛；批次那一層見 `bulk_restart` 的測試。
use super::*;
use crate::testing as tt;

pub(crate) struct LiveChild {
    pub id: String,
    pub run_id: String,
    pub pane_id: String,
    pub tab_id: String,
}

/// 一顆 claude 子 agent：真的（mock）pane、agent 在裡面、run 是 `running`／`idle`。同一個 env 可以開多顆。
pub(crate) async fn live_child(env: &tt::Env, name: &str) -> LiveChild {
    let app = &env.app;
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
    let parent = tt::claude_bot(app, &env.project_id, &format!("parent-{name}")).await;
    let id = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, parent_bot_id, created_at)
         VALUES (?,?,?,'claude','[]',0,0,'tok','child',?,?)",
    )
    .bind(&id)
    .bind(&env.project_id)
    .bind(name)
    .bind(&parent.id)
    .bind(db::now())
    .execute(&app.db)
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
    .execute(&app.db)
    .await
    .unwrap();
    env.herdr.agents.lock().unwrap().push(json!({
        "name": agent, "agent": "claude", "agent_status": "idle",
        "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"}));
    LiveChild { id, run_id, pane_id: pane.pane_id, tab_id: pane.tab_id }
}

/// 子 agent 一根寒毛都沒動：run 還是 `running`、沒有結束、pane 還在、沒有任何一次會動到 pane／agent 的 herdr 呼叫指向它。
/// 只看指向它的呼叫（pane／tab／agent 名）：同一批裡別的 bot 照常送自己的 ctrl+c、關自己的 pane。
pub(crate) async fn assert_child_untouched(env: &tt::Env, kid: &LiveChild) {
    let run = db::run(&env.app.db, &kid.run_id).await.unwrap().unwrap();
    assert_eq!(run.state, "running", "子 agent 的 run 被動過了");
    assert!(run.ended_at.is_none());
    assert_eq!(db::active_run(&env.app.db, &kid.id).await.unwrap().map(|r| r.id), Some(kid.run_id.clone()));
    assert!(env.herdr.tab(&kid.tab_id).unwrap().panes.contains(&kid.pane_id), "pane 被關掉了");
    let agent = run.agent_name.clone().unwrap();
    for m in ["pane.close", "tab.close", "agent.send_keys", "pane.send_keys", "agent.start"] {
        let aimed: Vec<_> = env
            .herdr
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

/// 驗收三：直接誤呼一般的 `restart_bot_with(child)` 要在停任何東西之前就被拒絕，不靠呼叫端先分類的約定。
#[tokio::test]
async fn a_regular_restart_of_a_child_is_refused_before_anything_is_stopped() {
    let env = tt::env().await;
    let app = env.app.clone();
    let kid = live_child(&env, "ui").await;

    for opts in [StartOpts::default(), StartOpts { resume_native: true, require_idle: true, ..Default::default() }] {
        match restart_bot_with(&app, &kid.id, opts).await {
            Err(LcError::Conflict(v)) => assert_eq!(v["reason"], "child_restarts_in_pane", "{v}"),
            other => panic!("child 走一般 stop + start 必須被拒絕：{other:?}"),
        }
        assert_child_untouched(&env, &kid).await;
    }
}
