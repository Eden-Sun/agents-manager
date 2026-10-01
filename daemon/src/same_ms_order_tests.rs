//! 同一毫秒的兩列：`created_at` 只到毫秒（`db::now`），而 ULID 的隨機段在同一毫秒內不遞增，所以「取最新／照順序」
//! 的查詢只靠 `created_at`（或再加 `id`）會挑錯（#100／#461／#695 同一類）。這裡每個測試都種兩列同一毫秒、
//! **`id` 的字典序跟寫入順序相反**，要求挑的是寫入順序（`rowid`）。

use crate::testing::{self as tt, Env};
use crate::db;

const SAME_MS: &str = "2026-10-01T00:00:00.000Z";

/// `(bot_id, conversation_id)`。
async fn conv(e: &Env, name: &str) -> (String, String) {
    let bot = tt::claude_bot(&e.app, &e.project_id, name).await;
    let conv = db::conversation_id(&e.app.db, &bot.id).await.unwrap();
    (bot.id, conv)
}

/// 「同一份回覆不存第二次」的比對對象是**最新**那則助理訊息。
#[tokio::test]
async fn the_newest_assistant_message_is_the_one_written_last_when_two_share_a_millisecond() {
    let e = tt::env().await;
    let (_bot, conv_id) = conv(&e, "assistant-tie").await;
    for (id, content) in [("m-zzz-first", "第一則"), ("m-aaa-second", "第二則")] {
        sqlx::query("INSERT INTO messages (id, conversation_id, role, content, source, created_at) VALUES (?,?, 'assistant', ?, 'hook', ?)")
            .bind(id)
            .bind(&conv_id)
            .bind(content)
            .bind(SAME_MS)
            .execute(&e.app.db)
            .await
            .unwrap();
    }
    let got = crate::lifecycle::last_assistant_content(&e.app, &conv_id).await.unwrap();
    assert_eq!(got.as_deref(), Some("第二則"));
}
