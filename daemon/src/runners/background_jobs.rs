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
    // claude 的 Stop hook 自己報過背景工作（`background_hook.rs`）就以它為準；沒報過（舊版）才是畫面的數字。
    if kind == "claude" {
        n = crate::background_hook::reconcile(app, &run.id, n);
    }
    let changed = {
        let mut m = app.background_jobs.lock().unwrap_or_else(|e| e.into_inner());
        // 0 也記：「看過、乾淨」跟「沒看過」不同（#767）。從沒看過到第一次看過也算變了，要推，前端才把「未知」換掉。
        // 數字沒變、但這一輪跨過「可能卡住」門檻（#774）也算變了。
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
    if run.state != "running" || !matches!(kind, "claude" | "codex") {
        return;
    }
    let Some(pane) = run.pane_id.as_deref() else { return };
    let Some(client) = app.herdr_for_run(run).await else { return };
    let Ok(read) = client.pane_read(pane, "visible", 80).await else { return };
    observe(app, run, kind, &read.text, &client, pane).await;
}
