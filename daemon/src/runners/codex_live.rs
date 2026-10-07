//! `codex_live` runner。

use crate::herdr::HerdrClient;
use crate::state::App;
use std::sync::Arc;

/// 巡邏用。`hint` 是這一輪已經讀過的畫面：跟 runtime 一樣就不再讀、也不拿 bot 鎖。
/// 不一樣才拿鎖（當場套用握著同一把）重讀再校正，避免把套用前的狀態列寫回去。
pub async fn sync_runtime(
    app: &Arc<App>,
    client: &HerdrClient,
    bot_id: &str,
    run_id: &str,
    pane_id: &str,
    hint: Option<&str>,
) {
    if let Some(text) = hint {
        if !crate::codex_live::hint_moves_runtime(app, run_id, text).await {
            return;
        }
    }
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    // 鎖裡重查：等鎖的時候這個 run 可能已經被換掉。
    if !matches!(crate::db::active_run(&app.db, bot_id).await, Ok(Some(r)) if r.id == run_id) {
        return;
    }
    let Ok(read) = client.pane_read(pane_id, "visible", 60).await else { return };
    crate::codex_live::correct_runtime_from_screen(app, run_id, &read.text).await;
}
