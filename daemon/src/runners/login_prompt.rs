//! `login_prompt` runner 與主動提示推播。

use crate::db;
use crate::login_prompt::*;
use crate::state::App;
use std::sync::Arc;

async fn push_host(app: &Arc<App>, host: &str) {
    if let Some(fence) = app.hosts.fence(host).await {
        crate::app_ports_p13::emit_host_changed(app, &fence).await;
    }
}

/// 拿掉並在有變時推 `host_changed`。
pub async fn clear_and_push(app: &Arc<App>, host: &str, identity: &str) {
    if clear(app, host, identity) {
        push_host(app, host).await;
    }
}

/// 綁著身分的 claude bot 的回合因為授權失敗收尾：記下、推快照，並立刻重探該身分（不等探測週期）。
/// 沒綁身分（用主機預設帳號）的 bot 不處理：沒有「哪個身分」可以提示。
pub async fn on_auth_failure(app: &Arc<App>, bot: &db::Bot) {
    if bot.kind == "agy" {
        crate::runners::agy_auth::on_turn_auth_failure(app, bot).await;
        return;
    }
    if bot.kind != "claude" {
        return;
    }
    let Some(identity) = bot.identity.as_deref().filter(|i| !i.is_empty()) else { return };
    let Ok(host) = db::bot_host(&app.db, &bot.id).await else { return };
    // 身分不在這台主機的表裡（改名、刪除、還沒偵測）：沒有東西可以附註，也就不記。
    let known = app.tools.lock().await.get(&host).is_some_and(|h| h.identities.contains_key(identity));
    if !known {
        return;
    }
    if mark(app, &host, identity, VIA_TURN) {
        tracing::info!(bot = %bot.name, %host, identity, "a turn failed on authentication; asking the user to log this identity in again");
        push_host(app, &host).await;
    }
    let (app, host, identity) = (app.clone(), host, identity.to_string());
    tokio::spawn(async move {
        // 本機問得出「未登入」會更新快取並推快照（`record_identity_login_fenced`）；遠端 claude 問不出來就維持上面的記號。
        let _ = crate::tools::recheck_identity_login(&app, &host, &identity).await;
    });
}

/// 綁著身分的 claude bot 又正常答完一回合：那個身分是通的。
pub async fn on_turn_ok(app: &Arc<App>, bot: &db::Bot) {
    if bot.kind != "claude" {
        return;
    }
    let Some(identity) = bot.identity.as_deref().filter(|i| !i.is_empty()) else { return };
    // 先看有沒有帳：絕大多數回合都沒有，不必每次都查 DB。
    if !app.login_needed.lock().unwrap_or_else(|e| e.into_inner()).keys().any(|(_, i)| i == identity) {
        return;
    }
    let Ok(host) = db::bot_host(&app.db, &bot.id).await else { return };
    clear_and_push(app, &host, identity).await;
}

#[cfg(test)]
#[path = "../../../crates/am-base/src/login_prompt/tests.rs"]
mod tests;
