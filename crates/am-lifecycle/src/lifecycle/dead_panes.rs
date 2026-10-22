//! pane 已經不在、run 卻還標 running 的收尾（#380）。
//!
//! 對帳只在開機／重連／事件訊號時跑；pane-exit 事件漏了（或 herdr 沒發）之後，那顆 run 會一直是 running／idle，
//! 側欄畫成活的，別的 agent 的交辦全打進一個不存在的 pane。這裡定時問 herdr：**只認「pane_not_found」**
//! （RPC 失敗、遠端斷線都不是證據，同 stop 的判準），而且 herdr 計畫中的維護期間不動（所有 pane 同時消失不是 agent 做完了，§6.5.2）。

use super::*;
use crate::{capabilities::Db, lifecycle::s6_ports::DeadPanesServices};

/// 這顆 run 的 pane herdr 明確說不在。讀不到（沒 pane id、沒 client、RPC 失敗）＝不知道，回 `false`。
pub async fn pane_gone(app: &(impl crate::capabilities::Db + crate::capabilities::HerdrRoutes + crate::hosts::HostsAccess), run: &db::Run) -> bool {
    let Some(pane) = run.pane_id.as_deref() else { return false };
    let Ok(client) = client_for_run(app, run).await else { return false };
    match client.pane_get(pane).await {
        Ok(None) => true,
        // 同一個 id 已經掛著別的 agent：舊 pane 不在了，不能再當成這顆 run。
        Ok(Some(_)) => pane_id_reused(&client, run).await,
        Err(_) => false,
    }
}

/// A different native session proves that the pane now holds another agent. If our session
/// is still listed on another pane, it moved: the address is stale but the run is not dead.
pub async fn pane_id_reused(client: &HerdrClient, run: &db::Run) -> bool {
    let Some(pane) = run.pane_id.as_deref().map(str::trim).filter(|p| !p.is_empty()) else { return false };
    let Ok(Some(other)) = client.agent_get(pane).await else { return false };
    if !other_agent_is_foreign(&other, run.native_session_id.as_deref()) {
        return false;
    }
    let Ok(agents) = client.agent_list().await else { return false };
    !agents.iter().any(|agent| herdr_session_id(agent).as_deref() == run.native_session_id.as_deref().map(str::trim))
}

/// Names are mutable display metadata, never evidence that a pane was recycled.
fn other_agent_is_foreign(other: &crate::herdr::AgentInfo, known_session: Option<&str>) -> bool {
    match (known_session.map(str::trim).filter(|s| !s.is_empty()), herdr_session_id(other)) {
        (Some(ours), Some(theirs)) => ours != theirs,
        _ => false,
    }
}

/// herdr 綁在 agent 上的 CLI session id（`agent_session.kind = "id"`）。沒綁或不是 id 形狀＝`None`。
pub fn herdr_session_id(agent: &crate::herdr::AgentInfo) -> Option<String> {
    let s = agent.agent_session.as_ref()?;
    if s.get("kind").and_then(Value::as_str).is_some_and(|k| k != "id") {
        return None;
    }
    s.get("value").and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty()).map(str::to_string)
}

/// Reject a pane whose native session differs; missing session evidence remains unknown.
pub async fn pane_target_stale(client: &HerdrClient, run: &db::Run) -> bool {
    let Some(pane) = run.pane_id.as_deref().map(str::trim).filter(|p| !p.is_empty()) else { return true };
    match client.agent_get(pane).await {
        Ok(Some(other)) => {
            if other_agent_is_foreign(&other, run.native_session_id.as_deref()) { return true; }
            if herdr_session_id(&other).is_some() { return false; }
        }
        _ => {}
    }
    let Some(ours) = run.native_session_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) else { return false };
    client.agent_list().await.is_ok_and(|agents| agents.iter().any(|agent| {
        agent.pane_id != pane && herdr_session_id(agent).as_deref() == Some(ours)
    }))
}

/// Resolve an execution address without consulting mutable names.
pub async fn execution_target(client: &HerdrClient, run: &db::Run, bot: &db::Bot) -> LcResult<String> {
    let target = db::run_target(run, bot);
    if target.is_empty() {
        return Err(LcError::NotFound("pane".into()));
    }
    if pane_target_stale(client, run).await {
        return Err(LcError::conflict("pane_reused", json!({"run_id": run.id, "pane_id": target, "sent": false})));
    }
    Ok(target)
}

/// 掃所有 running 的 run，pane 明確不在的收成 exited；回收掉的 run id。
pub async fn sweep(app: &(impl Db + DeadPanesServices + crate::lifecycle::s6_ports::QueueContext + crate::capabilities::HerdrRoutes + crate::hosts::HostsAccess)) -> Vec<String> {
    sweep_each(app).await.into_iter().map(|(id, _, _)| id).collect()
}

/// [`sweep`]，只回**這一次自己**寫下 `exited` 的 run 與轉移的證據（`AlreadyEnded`、寫不進去的不在內）。回報用，不影響 `sweep` 的行為。
pub async fn sweep_proved(app: &(impl Db + DeadPanesServices + crate::lifecycle::s6_ports::QueueContext + crate::capabilities::HerdrRoutes + crate::hosts::HostsAccess)) -> Vec<(String, ExitProof)> {
    sweep_each(app).await.into_iter().filter_map(|(id, _, proof)| proof.map(|p| (id, p))).collect()
}

async fn sweep_each(app: &(impl Db + DeadPanesServices + crate::lifecycle::s6_ports::QueueContext + crate::capabilities::HerdrRoutes + crate::hosts::HostsAccess)) -> Vec<(String, RunExit, Option<ExitProof>)> {
    // 讀不到維護狀態就不動：寧可晚收，不在 herdr 重啟中把整批活的 run 收掉。
    if app.maintenance_window_active().await {
        return Vec::new();
    }
    let Ok(runs) = db::all_active_runs(app.db()).await else { return Vec::new() };
    let mut gone = Vec::new();
    for run in runs.into_iter().filter(|r| r.state == "running") {
        if !pane_gone(app, &run).await {
            continue;
        }
        let (exit, proof) = mark_run_exited_proved(app, &run.id, "pane gone").await;
        if exit != RunExit::NotRecorded {
            tracing::warn!(run = %run.id, bot = %run.bot_id, "run's pane no longer exists; marked the run exited");
            gone.push((run.id, exit, proof));
        }
    }
    gone
}

#[cfg(all(test, feature = "daemon-test-harness"))]
mod nr_target_tests {
    use super::*;
    use crate::testing as tt;

    async fn fixture() -> (tt::Env, db::Bot, db::Run) {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "old-name").await;
        let id = tt::fake_run(&env.app, &bot.id).await;
        let run = db::run(&env.app.db, &id).await.unwrap().unwrap();
        (env, bot, run)
    }

    #[tokio::test]
    async fn nr_target_is_the_pane_even_after_names_change_or_are_reused() {
        let (env, mut bot, mut run) = fixture().await;
        let pane = run.pane_id.clone().unwrap();
        env.herdr.set_agent("renamed", &pane, true);
        env.herdr.set_agent("agent", "foreign-pane", true);
        bot.name = "new-display-name".into();
        assert_eq!(db::run_target(&run, &bot), pane);
        run.agent_name = None;
        assert_eq!(db::run_target(&run, &bot), pane);
        run.pane_id = None;
        assert!(db::run_target(&run, &bot).is_empty(), "missing ID must never fall back to a name");
    }

    #[tokio::test]
    async fn nr_target_rename_without_session_evidence_does_not_retire_the_run() {
        let (env, _, run) = fixture().await;
        env.herdr.set_agent("renamed", run.pane_id.as_deref().unwrap(), false);
        assert!(!pane_gone(&env.app, &run).await);
        assert!(sweep(&env.app).await.is_empty());
    }

    #[tokio::test]
    async fn nr_target_old_name_elsewhere_does_not_redirect_keys_or_delivery() {
        let (env, bot, mut run) = fixture().await;
        let pane = run.pane_id.as_deref().unwrap();
        env.herdr.set_agent("agent", "foreign-pane", true);
        env.herdr.set_agent("renamed", pane, true);
        run.native_session_id = Some("sess-1".into());
        let client = client_for_run(&env.app, &run).await.unwrap();
        assert!(!pane_target_stale(&client, &run).await);
        super::super::slash::send_keys(&env.app, &bot.id, vec!["esc".into()], None).await.unwrap();
        assert_eq!(env.herdr.calls_to("agent.send_keys")[0]["target"], pane);
        let plan = super::super::delivery::plan_delivery(&env.app, &client, &run, &bot, "hello", false, false).await.unwrap();
        match plan {
            Ok(super::super::delivery::Plan::AgentPrompt { target }) => assert_eq!(target, pane),
            _ => panic!("bound original pane must remain the delivery target"),
        }
    }

    #[tokio::test]
    async fn nr_target_missing_pane_sends_no_keys() {
        let (env, bot, run) = fixture().await;
        env.herdr.set_agent("old-name", "foreign-pane", true);
        sqlx::query("UPDATE runs SET pane_id=NULL WHERE id=?")
            .bind(&run.id).execute(&env.app.db).await.unwrap();
        assert!(super::super::slash::send_keys(&env.app, &bot.id, vec!["esc".into()], None).await.is_err());
        assert!(env.herdr.calls_to("agent.send_keys").is_empty());
    }

    #[tokio::test]
    async fn nr_target_moved_native_session_is_stale_but_not_dead() {
        let (env, _, mut run) = fixture().await;
        env.herdr.set_agent("agent", run.pane_id.as_deref().unwrap(), true);
        env.herdr.set_unnamed_agent("moved-pane", Some("original-session"));
        run.native_session_id = Some("original-session".into());
        let client = client_for_run(&env.app, &run).await.unwrap();
        assert!(pane_target_stale(&client, &run).await);
        assert!(!pane_id_reused(&client, &run).await);
    }

    #[tokio::test]
    async fn nr_target_native_session_moved_out_of_an_empty_pane_blocks_keys() {
        let (env, _, mut run) = fixture().await;
        env.herdr.set_unnamed_agent("moved-pane", Some("original-session"));
        run.native_session_id = Some("original-session".into());
        let client = client_for_run(&env.app, &run).await.unwrap();
        assert!(pane_target_stale(&client, &run).await);
    }

    #[tokio::test]
    async fn nr_target_same_name_cannot_hide_a_different_native_session() {
        let (env, bot, mut run) = fixture().await;
        env.herdr.set_agent("agent", run.pane_id.as_deref().unwrap(), true);
        run.native_session_id = Some("original-session".into());
        sqlx::query("UPDATE runs SET native_session_id=? WHERE id=?")
            .bind(&run.native_session_id).bind(&run.id).execute(&env.app.db).await.unwrap();
        let client = client_for_run(&env.app, &run).await.unwrap();
        assert!(pane_target_stale(&client, &run).await);
        assert!(pane_id_reused(&client, &run).await);
        assert!(super::super::slash::send_keys(&env.app, &bot.id, vec!["esc".into()], None).await.is_err());
        assert!(super::super::stop::stop_bot(&env.app, &bot.id).await.is_err());
        assert_eq!(db::run(&env.app.db, &run.id).await.unwrap().unwrap().state, "running");
        let aborted = super::super::stop::abort_turns(&env.app, &bot.id).await.unwrap();
        assert_eq!(aborted["keys_sent"], false);
        assert!(env.herdr.calls_to("agent.send_keys").is_empty());
        let plan = super::super::delivery::plan_delivery(&env.app, &client, &run, &bot, "hello", false, false).await.unwrap();
        assert!(plan.is_err(), "a bound but foreign session must not receive agent.prompt");
    }
}
