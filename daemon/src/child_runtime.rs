//! 子 agent（`managed_by=child`）的 `bots.model`／`effort` 是收編時從 argv 抄來的（SPEC §4.4a），沒有人會在
//! config.toml 改它；CLI 裡 `/model`、`/effort` 切換之後，畫面巡邏把新值寫進 `runs.runtime_*`，設定也要跟著改，
//! `/api/state` 的 `effort` 與側欄才不會停在啟動參數（2026-10-03 mkng2n：grok child `/effort low` 後仍寫 High）。
//!
//! 只跟**這一輪 runtime 真的變了**的欄位：網頁改了 child 設定、還沒套用時，狀態列沒變，不能把設定蓋回去。

use crate::state::App;
use serde_json::json;

/// runtime 剛從畫面讀到 `model`／`effort`（只傳變了的欄位）：bot 是 child 就把設定改成同一個值。
/// 回 `Err` = 寫不進去：呼叫端這輪不要寫 runtime，下一輪 runtime 仍不同才會再試（不然 runtime 收斂了、設定永遠落後，#743）。
pub async fn follow(app: &App, bot_id: &str, model: Option<&str>, effort: Option<&str>) -> Result<(), sqlx::Error> {
    if model.is_none() && effort.is_none() {
        return Ok(());
    }
    let r = sqlx::query(
        "UPDATE bots SET model = COALESCE(?, model), effort = COALESCE(?, effort)
          WHERE id = ? AND managed_by = 'child'
            AND (COALESCE(?, model) IS NOT model OR COALESCE(?, effort) IS NOT effort)",
    )
    .bind(model)
    .bind(effort)
    .bind(bot_id)
    .bind(model)
    .bind(effort)
    .execute(&app.db)
    .await?;
    if r.rows_affected() > 0 {
        tracing::info!(bot = bot_id, ?model, ?effort, "child settings follow a switch made in its TUI");
        // `bot_status` 不帶設定；網頁收到 `bot_changed` 才重抓 bots，不然 runtime 跟設定一樣了還畫著 ⟳。
        app.emit("bot_changed", json!({"bot_id": bot_id})).await;
    }
    Ok(())
}
