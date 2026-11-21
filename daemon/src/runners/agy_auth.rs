//! agy 回合以授權失敗收尾 → 這台主機的 agy 記成未登入（issue #870，SPEC §12a.7）。
//!
//! 額度探測看到 `Authentication required` 時 `quota_agy::record_probe_result` 已經會翻旗標；
//! 這裡補另一條路：Stop hook 的 `error` 被分類成 `FailureReason::Auth`（憑證被撤銷、過期）。

use crate::db;
use crate::runners::quota_agy::{note_auth_denied, set_logged_in_quiet};
use crate::state::App;
use std::sync::Arc;

/// agy bot 的回合因為授權失敗收尾：該主機 `tools.agy.logged_in=false`、清 agy 額度、推 `host_changed`。
/// 已經是未登入時什麼都不推（重送同一則不會再推）。
/// `fence`：這一則放行時捕獲的主機世代（#1024）。不在這裡重新取世代：事件屬於哪一代由放行決定，
/// 換代之後 `run_if_current` 會擋下寫入，舊事件不能把新主機標成未登入。
pub async fn on_turn_auth_failure(app: &Arc<App>, bot: &db::Bot, fence: &crate::hosts::HostFence) {
    let host = fence.conn().name.clone();
    let key = crate::quota::quota_key(&host, "agy");
    let Some((changed, had)) = app
        .hosts
        .run_if_current(&fence, async {
            note_auth_denied(key.clone(), &fence);
            let mut changed = set_logged_in_quiet(app, &host, &fence, false).await;
            changed |= crate::quota_agy::set_probe_error(&host, None);
            let had = app.quotas.lock().await.contains_key(&key);
            crate::quota::forget(app, &key).await;
            (changed, had)
        })
        .await
    else {
        tracing::info!(bot = %bot.name, %host, "agy turn failed on authentication after its host changed; ignored");
        return;
    };
    if had {
        app.emit("quota_updated", serde_json::json!({"kind": key, "host": host, "quota": null})).await;
    }
    if changed {
        crate::state::emit_host_changed(app, &fence).await;
        crate::webpush::spawn_auth_push(app, &host, "agy", "");
    }
    tracing::info!(bot = %bot.name, %host, "agy turn failed on authentication; marking agy logged out");
}

#[cfg(test)]
mod tests;
