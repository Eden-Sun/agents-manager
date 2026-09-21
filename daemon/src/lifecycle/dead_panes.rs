//! pane 已經不在、run 卻還標 running 的收尾（#380）。
//!
//! 對帳只在開機／重連／事件訊號時跑；pane-exit 事件漏了（或 herdr 沒發）之後，那顆 run 會一直是 running／idle，
//! 側欄畫成活的，別的 agent 的交辦全打進一個不存在的 pane。這裡定時問 herdr：**只認「pane_not_found」**
//! （RPC 失敗、遠端斷線都不是證據，同 stop 的判準），而且 herdr 計畫中的維護期間不動（所有 pane 同時消失不是 agent 做完了，§6.5.2）。

use super::*;

/// 這顆 run 的 pane herdr 明確說不在。讀不到（沒 pane id、沒 client、RPC 失敗）＝不知道，回 `false`。
pub(crate) async fn pane_gone(app: &Arc<App>, run: &db::Run) -> bool {
    let Some(pane) = run.pane_id.as_deref() else { return false };
    let Ok(client) = client_for_run(app, run).await else { return false };
    matches!(client.pane_get(pane).await, Ok(None))
}

/// 掃所有 running 的 run，pane 明確不在的收成 exited；回收掉的 run id。
pub(crate) async fn sweep(app: &Arc<App>) -> Vec<String> {
    // 讀不到維護狀態就不動：寧可晚收，不在 herdr 重啟中把整批活的 run 收掉。
    if !matches!(crate::herdr_maintenance::active(app).await, Ok(None)) {
        return Vec::new();
    }
    let Ok(runs) = db::all_active_runs(&app.db).await else { return Vec::new() };
    let mut gone = Vec::new();
    for run in runs.into_iter().filter(|r| r.state == "running") {
        if pane_gone(app, &run).await && mark_run_exited(app, &run.id, "pane gone").await != RunExit::NotRecorded {
            tracing::warn!(run = %run.id, bot = %run.bot_id, "run's pane no longer exists; marked the run exited");
            gone.push(run.id);
        }
    }
    gone
}
