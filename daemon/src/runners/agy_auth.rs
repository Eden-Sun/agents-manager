//! agy 回合以授權失敗收尾 → 這台主機的 agy 記成未登入（issue #870，SPEC §12a.7）。
//!
//! 額度探測看到 `Authentication required` 時 `quota_agy::record_probe_result` 已經會翻旗標；
//! 這裡補另一條路：Stop hook 的 `error` 被分類成 `FailureReason::Auth`（憑證被撤銷、過期）。

use crate::db;
use crate::runners::quota_agy::{auth_denied, set_logged_in_quiet, AUTH_DENIED_COOLDOWN};
use crate::state::App;
use std::sync::Arc;

/// agy bot 的回合因為授權失敗收尾：該主機 `tools.agy.logged_in=false`、清 agy 額度、推 `host_changed`。
/// 已經是未登入時什麼都不推（重送同一則不會再推）。
pub async fn on_turn_auth_failure(app: &Arc<App>, bot: &db::Bot) {
    let Ok(host) = db::bot_host(&app.db, &bot.id).await else { return };
    let Some(fence) = app.hosts.fence(&host).await else { return };
    let key = crate::quota::quota_key(&host, "agy");
    // 憑證檔還在（只是被撤銷）：登入偵測不能在冷卻期內又把旗標翻回已登入、再開一次約 200 MB 的探測。
    auth_denied().lock().unwrap().insert(key.clone(), std::time::Instant::now() + AUTH_DENIED_COOLDOWN);
    let mut changed = set_logged_in_quiet(app, &host, &fence, false).await;
    // 不要讓網頁同時顯示「已登入，但額度暫時拿不到」。
    changed |= crate::quota_agy::set_probe_error(&host, None);
    // 舊讀數不能繼續看起來很新。
    let had = app
        .hosts
        .run_if_current(&fence, async {
            let had = app.quotas.lock().await.contains_key(&key);
            crate::quota::forget(app, &key).await;
            had
        })
        .await
        .unwrap_or(false);
    if had {
        app.emit("quota_updated", serde_json::json!({"kind": key, "host": host, "quota": null})).await;
    }
    // 不能放進 `run_if_current` 的 closure：`emit_host_changed` 自己也拿同一把圍籬的 gate。
    if changed {
        crate::state::emit_host_changed(app, &fence).await;
    }
    tracing::info!(bot = %bot.name, %host, "agy turn failed on authentication; marking agy logged out");
}

#[cfg(test)]
mod tests;
