//! `prompt_suggestion` runner。

use crate::db;
use crate::state::App;
use am_ports::DbContext;
use std::sync::Arc;
use std::time::Duration;

/// idle 邊之後第幾毫秒補讀一次：建議句的產生要一點時間（另一次模型呼叫），畫面也要畫完。
const BURST: [u64; 4] = [1500, 3500, 7000, 14000];

/// 讀一次樣式畫面、照結果記下或清掉建議（idle 邊補讀用）。回傳現在有沒有建議；讀不到回 `None`（什麼都不動）。
pub async fn observe(app: &Arc<App>, run: &db::Run) -> Option<bool> {
    crate::app_ports_r2a8::prompt_suggestion_observe(app, run).await
}

/// herdr 轉成 `idle` 那一刻（[`crate::events`]）：在 [`BURST`] 的幾個時間點各看一次，看到建議或不再 idle 就停。
/// 自己開背景工作，不擋事件迴圈。
pub fn on_idle(app: &Arc<App>, run: &db::Run) {
    let (app, run_id) = (app.clone(), run.id.clone());
    tokio::spawn(async move {
        let db = DbContext::new(app.db.clone());
        let mut waited = 0;
        for at in BURST {
            tokio::time::sleep(Duration::from_millis(at - waited)).await;
            waited = at;
            // 事件帶來的 Run 是更新前的複本：每次重讀，狀態才是現在的。
            let Ok(Some(run)) = db::run(db.pool(), &run_id).await else { return };
            match observe(&app, &run).await {
                Some(true) => return,
                _ if run.state != "running" || run.agent_status != "idle" => return,
                _ => {}
            }
        }
    });
}
