//! 子 agent（`managed_by=child`）的 `bots.model`／`effort` 是收編時從 argv 抄來的（SPEC §4.4a），沒有人會在
//! config.toml 改它；CLI 裡 `/model`、`/effort` 切換之後，畫面巡邏把新值寫進 `runs.runtime_*`，設定也要跟著改，
//! `/api/state` 的 `effort` 與側欄才不會停在啟動參數（2026-10-03 mkng2n：grok child `/effort low` 後仍寫 High）。
//!
//! 只跟**這一輪 runtime 真的變了**的欄位：網頁改了 child 設定、還沒套用時，狀態列沒變，不能把設定蓋回去。

use serde_json::json;

/// 強度怎麼跟到子 agent 的 `bots.effort`（SPEC §4.4a）。`NULL` 在設定欄只表示「CLI 預設」，所以清成 `NULL` 要明講，
/// 不能讓 `COALESCE(NULL, …)` 悄悄保留舊值；而「未知」（畫面沒讀到、或讀到 default 但之前也不知道）絕不能清。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortFollow<'a> {
    /// 不動設定：沒讀到、未知→default、或這一輪沒變。
    Keep,
    /// 設定改成同一個具體值。
    Set(&'a str),
    /// 已知的具體值 → 明確的 CLI 預設：設定清成 `NULL`（＝CLI 預設）。
    Clear,
}

/// runtime 剛從畫面讀到 `model`／`effort`（只傳變了的欄位）：bot 是 child 就把設定改成同一個值。
/// 回 `Err` = 寫不進去：呼叫端這輪不要寫 runtime，下一輪 runtime 仍不同才會再試（不然 runtime 收斂了、設定永遠落後，#743）。
/// claude／grok 仍用這個窄介面：只會 `Set` 或 `Keep`。
pub async fn follow(app: &(impl crate::capabilities::Db + crate::capabilities::Emit), bot_id: &str, model: Option<&str>, effort: Option<&str>) -> Result<(), sqlx::Error> {
    follow_with(app, bot_id, model, effort.map_or(EffortFollow::Keep, EffortFollow::Set)).await
}

/// 同 [`follow`]，但強度多一個 [`EffortFollow::Clear`]（codex 狀態列明確寫 `default`）。只動 `managed_by='child'`。
pub async fn follow_with(app: &(impl crate::capabilities::Db + crate::capabilities::Emit), bot_id: &str, model: Option<&str>, effort: EffortFollow<'_>) -> Result<(), sqlx::Error> {
    follow_with_fast(app, bot_id, model, effort, None).await
}

/// 同 [`follow_with`]，另接收這一輪已觀察到改變的 fast；`None` 保留設定。只動 child。
pub async fn follow_with_fast(app: &(impl crate::capabilities::Db + crate::capabilities::Emit), bot_id: &str, model: Option<&str>, effort: EffortFollow<'_>, fast: Option<bool>) -> Result<(), sqlx::Error> {
    let fast = fast.map(i64::from);
    let (value, clear) = match effort {
        EffortFollow::Keep => (None, 0_i64),
        EffortFollow::Set(e) => (Some(e), 0),
        EffortFollow::Clear => (None, 1),
    };
    if model.is_none() && effort == EffortFollow::Keep && fast.is_none() {
        return Ok(());
    }
    // `clear` 只在 Clear 時為 1：CASE 先判，才能把 NULL 寫進去（COALESCE 寫不進 NULL）。
    let r = sqlx::query(
        "UPDATE bots SET model = COALESCE(?, model),
                effort = CASE WHEN ? = 1 THEN NULL ELSE COALESCE(?, effort) END,
                fast = COALESCE(?, fast)
          WHERE id = ? AND managed_by = 'child'
            AND (COALESCE(?, model) IS NOT model
                 OR (CASE WHEN ? = 1 THEN NULL ELSE COALESCE(?, effort) END) IS NOT effort
                 OR COALESCE(?, fast) IS NOT fast)",
    )
    .bind(model)
    .bind(clear)
    .bind(value)
    .bind(fast)
    .bind(bot_id)
    .bind(model)
    .bind(clear)
    .bind(value)
    .bind(fast)
    .execute(app.db())
    .await?;
    if r.rows_affected() > 0 {
        tracing::info!(bot = bot_id, ?model, ?effort, "child settings follow a switch made in its TUI");
        // `bot_status` 不帶設定；網頁收到 `bot_changed` 才重抓 bots，不然 runtime 跟設定一樣了還畫著 ⟳。
        app.emit("bot_changed", json!({"bot_id": bot_id})).await;
    }
    Ok(())
}
