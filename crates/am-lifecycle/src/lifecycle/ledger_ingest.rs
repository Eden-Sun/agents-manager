//! 使用者訊息落地後的帳本收件（#1862 第 1 階段 T2）：入帳成功就推 `ledger_updated`，網頁才會即時顯示「待分流」。
//! 四個收件點（直送、忙碌排隊、啟動排隊、native API）共用這裡；收件失敗只記 log，訊息本身已經收下。

use crate::capabilities::{Db, Emit};
use serde_json::json;

pub(crate) async fn ingest_user_prompt(app: &(impl Db + Emit), bot_id: &str, message_id: &str, turn_id: &str, text: &str) {
    match am_base::ledger::ingest_if_management_message(app.db(), bot_id, message_id, Some(turn_id), text).await {
        Ok(Some(item)) => {
            Emit::emit(app, "ledger_updated", json!({"project_id": item.project_id, "source_message_id": item.source_message_id, "item_id": item.id})).await;
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(bot = %bot_id, error = %e, "帳本收件失敗（訊息已經收下）"),
    }
}
