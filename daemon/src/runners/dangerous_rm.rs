//! `dangerous_rm` runner。

use crate::dangerous_rm::observe_screen;
use crate::db;
use crate::state::App;
use std::sync::Arc;
use std::time::Duration;

const SETTLE: Duration = Duration::from_millis(700);

/// herdr 轉成 `blocked` 那一刻（[`crate::events`]）：等畫面畫完再看。自己開背景工作，不擋事件迴圈。
pub fn on_blocked(app: &Arc<App>, run: &db::Run) {
    let (app, run) = (app.clone(), run.clone());
    tokio::spawn(async move {
        tokio::time::sleep(SETTLE).await;
        observe(&app, &run).await;
    });
}

/// 讀一次畫面、照結果開或收（巡邏與 `blocked` 邊共用）。只看 claude；讀不到畫面什麼都不動——讀不到不等於框關了。
pub async fn observe(app: &Arc<App>, run: &db::Run) {
    if !matches!(db::bot(&app.db, &run.bot_id).await, Ok(Some(b)) if b.kind == "claude") {
        return;
    }
    let Some(pane) = run.pane_id.as_deref().filter(|p| !p.trim().is_empty()) else { return };
    let Some(client) = app.herdr_for_run(run).await else { return };
    let Ok(read) = client.pane_read(pane, "visible", 80).await else { return };
    observe_screen(app, run, &read.text).await;
}
