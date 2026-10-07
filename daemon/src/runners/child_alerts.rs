use std::sync::Arc;
use crate::db;
use crate::state::App;

/// child 轉成 `blocked` 時呼叫（[`crate::events::handle_status`]）。自己開背景工作，不擋事件迴圈。
pub fn on_child_blocked(app: &Arc<App>, run: &db::Run) {
    if cfg!(test) {
        return;
    }
    let (app, run) = (app.clone(), run.clone());
    let working = crate::child_alerts::Working::start(&run.bot_id);
    tokio::spawn(async move {
        let _working = working;
        tokio::time::sleep(crate::child_alerts::SETTLE).await;
        crate::child_alerts::keep_telling(&app, &run, &crate::child_alerts::RETRY).await;
    });
}

/// 定時的安全網（#192，跟著卡住回合的定時掃描每分鐘一次）
pub async fn sweep(app: &Arc<App>) -> usize {
    let runs = match crate::child_alerts::blocked_children(app.as_ref()).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = ?e, "child alert sweep: could not list blocked children; next round");
            return 0;
        }
    };
    let mut started = 0;
    for run in runs {
        if crate::child_alerts::Working::running(&run.bot_id) {
            continue;
        }
        started += 1;
        let working = crate::child_alerts::Working::start(&run.bot_id);
        if cfg!(test) {
            // 測試裡就地跑完一輪、不重試：看得到結果，也不留背景工作。
            crate::child_alerts::keep_telling(app, &run, &[]).await;
            drop(working);
            continue;
        }
        let app = app.clone();
        tokio::spawn(async move {
            let _working = working;
            crate::child_alerts::keep_telling(&app, &run, &crate::child_alerts::RETRY).await;
        });
    }
    started
}
