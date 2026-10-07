//! pane 已經不在、run 卻還標 running 的收尾（#380）。
//!
//! 對帳只在開機／重連／事件訊號時跑；pane-exit 事件漏了（或 herdr 沒發）之後，那顆 run 會一直是 running／idle，
//! 側欄畫成活的，別的 agent 的交辦全打進一個不存在的 pane。這裡定時問 herdr：**只認「pane_not_found」**
//! （RPC 失敗、遠端斷線都不是證據，同 stop 的判準），而且 herdr 計畫中的維護期間不動（所有 pane 同時消失不是 agent 做完了，§6.5.2）。

use super::*;
use crate::{capabilities::Db, lifecycle::s6_ports::DeadPanesServices};

/// 這顆 run 的 pane herdr 明確說不在。讀不到（沒 pane id、沒 client、RPC 失敗）＝不知道，回 `false`。
pub(crate) async fn pane_gone(app: &(impl crate::capabilities::Db + crate::capabilities::HerdrRoutes + crate::hosts::HostsAccess), run: &db::Run) -> bool {
    let Some(pane) = run.pane_id.as_deref() else { return false };
    let Ok(client) = client_for_run(app, run).await else { return false };
    match client.pane_get(pane).await {
        Ok(None) => true,
        // 同一個 id 已經掛著別的 agent：舊 pane 不在了，不能再當成這顆 run。
        Ok(Some(_)) => pane_id_reused(&client, run).await,
        Err(_) => false,
    }
}

/// herdr 把已消失的 pane id 交給別的 agent。這顆 run 的 agent 已經不在，查不到就不算證據。
/// agent 還活著、只是換了 pane 時回 false：run 不該因此被收掉，但 [`pane_target_stale`] 仍拒絕打進舊 id。
pub(crate) async fn pane_id_reused(client: &HerdrClient, run: &db::Run) -> bool {
    let Some(pane) = run.pane_id.as_deref().map(str::trim).filter(|p| !p.is_empty()) else {
        return false;
    };
    let Some(name) = run.agent_name.as_deref().map(str::trim).filter(|n| !n.is_empty()) else {
        return false;
    };
    let ours = match client.agent_get(name).await {
        Ok(agent) => agent,
        Err(_) => return false,
    };
    if ours.as_ref().is_some_and(|agent| agent.pane_id == pane) {
        return false;
    }
    if ours.is_some() {
        return false;
    }
    match client.agent_get(pane).await {
        Ok(Some(other)) => other_agent_is_foreign(&other, name, run.native_session_id.as_deref()),
        _ => false,
    }
}

/// 這顆 pane 上、查不到我方名字時看到的 agent，是不是「別人」。
///
/// **沒有名字不算別人**（#878）：herdr 的 `agent start` 等不到 agent 準備好（30 秒）就把名字從 pane 上拿掉，pane 與裡面的 CLI
/// 卻好端端還在跑；以前這裡把「名字不是我」連「沒有名字」一起當成被別人佔走，29 秒後整顆 run 被收成 `pane gone`。
/// 有名字而且不是我，才是 pane id 被別的 agent 重用；沒有名字時只有兩邊的 session 都知道、而且對不上才算別人。
fn other_agent_is_foreign(other: &crate::herdr::AgentInfo, name: &str, known_session: Option<&str>) -> bool {
    match other.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => n != name,
        None => match (known_session.filter(|s| !s.is_empty()), herdr_session_id(other)) {
            (Some(ours), Some(theirs)) => ours != theirs,
            _ => false,
        },
    }
}

/// herdr 綁在 agent 上的 CLI session id（`agent_session.kind = "id"`）。沒綁或不是 id 形狀＝`None`。
pub(crate) fn herdr_session_id(agent: &crate::herdr::AgentInfo) -> Option<String> {
    let s = agent.agent_session.as_ref()?;
    if s.get("kind").and_then(Value::as_str).is_some_and(|k| k != "id") {
        return None;
    }
    s.get("value").and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty()).map(str::to_string)
}

/// 這個 pane id 不能再收這顆 run 的字：被別的 agent 佔走，或自己的 agent 已經在別的 pane。
pub(crate) async fn pane_target_stale(client: &HerdrClient, run: &db::Run) -> bool {
    let Some(pane) = run.pane_id.as_deref().map(str::trim).filter(|p| !p.is_empty()) else {
        return false;
    };
    let Some(name) = run.agent_name.as_deref().map(str::trim).filter(|n| !n.is_empty()) else {
        return false;
    };
    match client.agent_get(name).await {
        Ok(Some(agent)) => agent.pane_id != pane,
        Ok(None) => pane_id_reused(client, run).await,
        Err(_) => false,
    }
}

/// 掃所有 running 的 run，pane 明確不在的收成 exited；回收掉的 run id。
pub(crate) async fn sweep(app: &(impl Db + DeadPanesServices + crate::lifecycle::s6_ports::QueueContext + crate::capabilities::HerdrRoutes + crate::hosts::HostsAccess)) -> Vec<String> {
    // 讀不到維護狀態就不動：寧可晚收，不在 herdr 重啟中把整批活的 run 收掉。
    if app.maintenance_window_active().await {
        return Vec::new();
    }
    let Ok(runs) = db::all_active_runs(app.db()).await else { return Vec::new() };
    let mut gone = Vec::new();
    for run in runs.into_iter().filter(|r| r.state == "running") {
        if pane_gone(app, &run).await && mark_run_exited(app, &run.id, "pane gone").await != RunExit::NotRecorded {
            tracing::warn!(run = %run.id, bot = %run.bot_id, "run's pane no longer exists; marked the run exited");
            gone.push(run.id);
        }
    }
    gone
}
