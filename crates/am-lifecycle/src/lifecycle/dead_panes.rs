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

