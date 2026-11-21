//! 使用者訊息落地後的帳本收件（#1862 第 1 階段 T2／修正）：入帳成功就推 `ledger_updated`，網頁才會即時顯示「待分流」。
//!
//! 只收「人打的字」：來源白名單是 `QueueClass::User`、沒有轉發者（`from`）、不是 daemon 自己產生的回合（`daemon`）。
//! daemon 排的提醒（DaemonNotice）、保溫、控制面握手、relay 一律不收，否則提醒會變成自己的待辦、越排越多。
//! 收件失敗只記 log，訊息本身已經收下。

use crate::capabilities::{Cfg, Db, Emit};
use crate::lifecycle::prompt::RelaySrc;
use crate::lc_error::QueueClass;
use serde_json::json;

pub(crate) async fn ingest_user_prompt(app: &(impl Db + Emit + Cfg), relay: RelaySrc<'_>, bot_id: &str, message_id: &str, turn_id: &str, text: &str) {
    // 預設不自動收件：每則都建項太吵，要追蹤的才加（`[ledger] auto_ingest`）。
    if !app.cfg().get().await.ledger.auto_ingest {
        return;
    }
    if relay.from.is_some() || relay.unverified || relay.daemon || relay.queue_class != QueueClass::User {
        return;
    }
    match am_base::ledger::ingest_if_management_message(app.db(), bot_id, message_id, Some(turn_id), text).await {
        Ok(Some(item)) => {
            Emit::emit(app, "ledger_updated", json!({"project_id": item.project_id, "source_message_id": item.source_message_id, "item_id": item.id})).await;
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(bot = %bot_id, error = %e, "帳本收件失敗（訊息已經收下）"),
    }
}
