use std::sync::Arc;

use crate::background_jobs::{parse, record, services};
use crate::db;
use crate::state::App;

/// 巡邏讀到一份畫面：數字變了才記、才推 `bot_status`。
pub async fn observe(app: &Arc<App>, run: &db::Run, kind: &str, screen: &str, client: &crate::herdr::HerdrClient, pane: &str) {
    let raw = parse(kind, screen);
    let mut n = raw;
    if n > 0 && kind == "claude" {
        n = n.saturating_sub(services(app, run, client, pane).await);
    }
    if kind == "claude" {
        n = crate::background_hook::reconcile(app, &run.id, n);
    }
    let changed = {
        let mut m = app.background_jobs.lock().unwrap_or_else(|e| e.into_inner());
        record(&mut m, &run.id, n)
    };
    if changed {
        tracing::info!(run = %run.id, bot = %run.bot_id, kind, background_jobs = n, "background jobs changed");
        app.emit_bot_status(&run.bot_id).await;
    }
}

/// 現場讀一次這個 run 的畫面並記帳（一鍵重啟在計畫時與輪到時用）。巡邏每 30 秒才一輪：回合剛結束、背景工作剛丟出去的
/// 那幾秒，帳上是「沒看過」或上一輪的 0，不能拿來當乾淨的證據。讀不到（沒有 pane、主機沒連、herdr 讀失敗）就維持原帳——
/// 沒有新證據不改舊證據。
pub async fn refresh(app: &Arc<App>, run: &db::Run, kind: &str) {
    if run.state != "running" || !matches!(kind, "claude" | "codex" | "agy") {
        return;
    }
    let Some(pane) = run.pane_id.as_deref() else { return };
    let Some(client) = app.herdr_for_run(run).await else { return };
    let Ok(read) = client.pane_read(pane, "visible", 80).await else { return };
    observe(app, run, kind, &read.text, &client, pane).await;
}
