//! daemon 自己產生、排進佇列的通知（#562）：子 agent 停在 blocked（`child_alerts`）、重啟後的續行提示（`resume_nudge`）。
//!
//! 它們不是使用者送的、也不是 AGM 派工，寫入時由 daemon 指定獨立 lane（#1685：使用者、relay、通知各自最多一筆 queued）。一則通知送不進去時，
//! 照一般 prompt 的重試上限（12 次）要擋約 40 分鐘，後面的使用者訊息一直排不到。所以：
//! - 放回佇列的上限短得多（[`RETRY_LIMIT`]），用完就收成 failed＋一則 system 說明，讓佇列往下走；
//! - `POST /api/turns/{id}/withdraw` 可以撤回還在排的這一種（使用者訊息、AGM 派工照舊 409）。

/// daemon 通知最多放回佇列幾次：退避 15＋30＋60 秒，大約兩分鐘就讓路（一般 prompt 是 12 次、約 40 分鐘）。
pub const RETRY_LIMIT: i64 = 3;

/// 上限用完、收成 failed 時寫進對話的說明。
pub fn gave_up_hint(reason: &str) -> String {
    format!(
        "daemon 自動通知沒有送出：試了 {RETRY_LIMIT} 次都沒辦法打字（最後一次是 {reason}），已放棄，讓後面排著的訊息先送。通知的內容仍在上面，需要的話自己轉告 agent。"
    )
}

/// 使用者撤回（`POST /api/turns/{id}/withdraw`）時寫進對話的說明。
pub const WITHDRAWN_WHY: &str = "使用者撤回了這則 daemon 自動通知：沒有送出，不會再送。";
