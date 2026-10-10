//! 分享沙箱總預算的巡邏（#853）：每 [`share::budget::REFRESH_EVERY`] 重量所有受限分享 bot，剛變滿的通知擁有者。
//! 量測與「滿了擋新訊息」在 `share::budget`；這裡只負責定期跑與通知（對話裡一則系統訊息＋AGM inbox 一則 `ops_alert`）。

use crate::share::budget;
use crate::state::App;
use std::sync::Arc;

/// 開機後起一條背景迴圈。測試不起。
pub fn spawn_ticker(app: &Arc<App>) {
    if cfg!(test) {
        return;
    }
    let app = app.clone();
    let tasks = app.background_tasks.clone();
    tasks.spawn(async move {
        let mut every = tokio::time::interval(budget::REFRESH_EVERY);
        loop {
            tokio::select! {
                _ = app.shutdown.cancelled() => return,
                _ = every.tick() => {}
            }
            tick(&app).await;
        }
    });
}

/// 巡一輪：先補做欠著的遠端保留標記（#1038），再重量，剛變滿的通知。
pub async fn tick(app: &Arc<App>) {
    crate::share::drain_keep_pending(app).await;
    for (bot_id, measured) in budget::sweep(app).await {
        notify_owner(app, &bot_id, measured).await;
    }
}

fn mib(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

const FULL_NOTE_PREFIX: &str = "分享空間滿了：";

async fn notify_owner(app: &Arc<App>, bot_id: &str, m: budget::Measured) {
    // 量到一半停下（樹太深或檔案太多）時，數字只是下限：不能說「已達上限」，要講清楚是量不完、擋下來是保守的做法（#1028）。
    let why = if m.truncated {
        "工作目錄或 outbox 的目錄太深或檔案太多，量不完（數字只是下限，所以先擋下）。".to_string()
    } else {
        format!("已達 {} MiB 上限。", mib(budget::SANDBOX_MAX_BYTES))
    };
    let text = format!(
        "{FULL_NOTE_PREFIX}工作目錄 {} MiB＋輸出 {} MiB，{}新的分享訊息會被擋下（507 share_storage_full），分享頁顯示「空間滿了」。\
         請整理工作目錄或 outbox（daemon 不會自動刪工作目錄的檔）；清掉後約 1 分鐘內恢復。",
        mib(m.workspace_bytes),
        mib(m.outbox_bytes),
        why,
    );
    match crate::db::conversation_id(&app.db, bot_id).await {
        Ok(conv) => {
            let told: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM messages WHERE conversation_id = ? AND role = 'system' AND content LIKE ? AND created_at >= ?)",
            )
            .bind(&conv)
            .bind(format!("{FULL_NOTE_PREFIX}%"))
            .bind(crate::db::iso_in(-24 * 3600))
            .fetch_one(&app.db)
            .await
            .unwrap_or(false); // 查不到就照貼：寧可多一則，不要漏講
            if !told {
                if let Err(e) = crate::lifecycle::insert_message(app.as_ref(), &conv, None, "system", &text, "system", false, None).await {
                    tracing::warn!(bot = %bot_id, error = %e, "could not tell the owner the share sandbox is full");
                }
            }
        }
        Err(e) => tracing::warn!(bot = %bot_id, error = %e, "could not find the share bot's conversation"),
    }
    // 同一顆一天最多推一則 ops_alert（key 帶日期）。
    let day = chrono::Utc::now().format("%Y-%m-%d");
    let key = format!("ops_alert:daemon:share_storage_full:{bot_id}:{day}");
    let payload = serde_json::json!({
        "source": "daemon",
        "reason": "share_storage_full",
        "subject": bot_id,
        "detail": text,
        "action": "只是通知，daemon 沒有刪任何檔：請擁有者整理這顆分享 bot 的工作目錄或 outbox。",
    });
    if let Err(e) = crate::supervisor::store::push_inbox(&app.db, &key, "ops_alert", None, None, None, &payload).await {
        tracing::warn!(bot = %bot_id, error = %e, "could not queue the share_storage_full ops_alert");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_full_share_bot_is_told_once_across_restarts() {
        let e = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&e.app, &e.project_id, "full").await;
        let m = budget::Measured {
            workspace_bytes: budget::SANDBOX_MAX_BYTES,
            ..Default::default()
        };
        notify_owner(&e.app, &bot.id, m).await;
        notify_owner(&e.app, &bot.id, m).await;

        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages m JOIN conversations c ON c.id = m.conversation_id WHERE c.bot_id = ? AND m.role = 'system' AND m.content LIKE '分享空間滿了：%'",
        )
        .bind(&bot.id)
        .fetch_one(&e.app.db)
        .await
        .unwrap();
        assert_eq!(count, 1);

        sqlx::query(
            "UPDATE messages SET created_at = ? WHERE conversation_id = (SELECT id FROM conversations WHERE bot_id = ?) AND role = 'system' AND content LIKE '分享空間滿了：%'",
        )
        .bind(crate::db::iso_in(-25 * 3600))
        .bind(&bot.id)
        .execute(&e.app.db)
        .await
        .unwrap();

        notify_owner(&e.app, &bot.id, m).await;

        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages m JOIN conversations c ON c.id = m.conversation_id WHERE c.bot_id = ? AND m.role = 'system' AND m.content LIKE '分享空間滿了：%'",
        )
        .bind(&bot.id)
        .fetch_one(&e.app.db)
        .await
        .unwrap();
        assert_eq!(count, 2);
    }
}

