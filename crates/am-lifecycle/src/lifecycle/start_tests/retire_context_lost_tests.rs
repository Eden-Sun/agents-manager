use super::*;
use crate::capabilities::Db;

async fn notes(app: &Arc<App>, conv: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id = ? AND role = 'system' ORDER BY created_at, rowid")
        .bind(conv)
        .fetch_all(app.db())
        .await
        .unwrap()
}

async fn active_notice_sessions(app: &Arc<App>, bot_id: &str) -> Vec<Option<String>> {
    sqlx::query_scalar(
        "SELECT native_session_id FROM context_lost_notices WHERE bot_id = ? AND retired_at IS NULL ORDER BY created_at, rowid",
    )
    .bind(bot_id)
    .fetch_all(app.db())
    .await
    .unwrap()
}

#[tokio::test]
async fn resuming_a_session_retires_its_notice_and_keeps_legacy_notes() {
    let env = crate::testing::env().await;
    let app = env.app.clone();
    let bot = crate::testing::claude_bot(&app, &env.project_id, "rpa").await;
    let conv = db::conversation_id(app.db(), &bot.id).await.unwrap();
    // 舊版通知沒有 session metadata；保守留下，不從文字或時間猜它屬於哪一段。
    let _ = insert_message(&app, &conv, None, "system", "⚠️ 接不回原本的對話（舊的），已經開了新的對話——前面的脈絡沒有帶過來。", "system", false, None).await;
    sqlx::query("UPDATE messages SET created_at = '2026-10-01T00:00:00.000Z' WHERE conversation_id = ?").bind(&conv).execute(app.db()).await.unwrap();
    // 用過 S 的那個 run 在 10-04 00:19:32 結束，緊接著插了失敗通知與一則一般系統訊息。
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at, ended_at, native_session_id)
         VALUES ('run-old', ?, 'exited', 'idle', 'ws', 'p', 'a', 't', '2026-10-03T02:27:05.470Z', '2026-10-04T00:19:32.795Z', 'S')",
    )
    .bind(&bot.id)
    .execute(app.db())
    .await
    .unwrap();
    context_lost(&app, &bot, "transcript_missing", Some("S")).await.unwrap();
    let _ = insert_message(&app, &conv, None, "system", "Claude Code 已更新到 2.1.290", "system", false, None).await;
    assert_eq!(notes(&app, &conv).await.len(), 3);

    // 接回別段 session：不動。
    retire_context_lost(&app, &bot.id, "OTHER").await;
    assert_eq!(notes(&app, &conv).await.len(), 3);

    // 接回 S：只撤綁定 S 的通知，不解析舊通知文字或依時間刪除。
    retire_context_lost(&app, &bot.id, "S").await;
    let left = notes(&app, &conv).await;
    assert_eq!(left.len(), 2, "{left:?}");
    assert!(left[0].contains("（舊的）"), "更早、別段的失敗通知留著");
    assert!(left[1].contains("已更新"), "不是失敗通知的系統訊息不動");
}

#[tokio::test]
async fn notice_without_a_failed_session_id_is_not_guessed_from_history() {
    let env = crate::testing::env().await;
    let app = env.app.clone();
    let bot = crate::testing::claude_bot(&app, &env.project_id, "rpa").await;
    let conv = db::conversation_id(app.db(), &bot.id).await.unwrap();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at, ended_at, native_session_id)
         VALUES ('run-old', ?, 'exited', 'idle', 'ws', 'p', 'a', 't', '2026-10-04T01:00:00.000Z', '2026-10-04T01:10:00.000Z', 'sess-A')",
    )
    .bind(&bot.id)
    .execute(app.db())
    .await
    .unwrap();

    context_lost(&app, &bot, "no_session_id", None).await.unwrap();
    let stored_session: Option<String> = sqlx::query_scalar(
        "SELECT native_session_id FROM context_lost_notices WHERE bot_id = ? ORDER BY created_at DESC LIMIT 1",
    )
    .bind(&bot.id)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(stored_session, None, "沒有目標 session 時不能猜舊 run 的 id");
    assert_eq!(active_notice_sessions(&app, &bot.id).await, vec![None]);

    retire_context_lost(&app, &bot.id, "sess-A").await;
    assert_eq!(notes(&app, &conv).await.len(), 1, "無法證明身份的通知保守保留");
}

/// issue #852: 跨 session 接回不能誤撤其他 session 仍屬實的警告。
/// 1. A 失敗 -> WA
/// 2. B 失敗 -> WB
/// 3. 接回 A -> WA 撤銷，WB 留著
/// 4. 接回 B -> WB 撤銷
#[tokio::test]
async fn recovering_older_session_does_not_retire_newer_session_warning() {
    let env = crate::testing::env().await;
    let app = env.app.clone();
    let bot = crate::testing::claude_bot(&app, &env.project_id, "rpa").await;
    let conv = db::conversation_id(app.db(), &bot.id).await.unwrap();

    // Run A 結束，接著 context_lost(A)
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at, ended_at, native_session_id)
         VALUES ('run-a', ?, 'exited', 'idle', 'ws', 'p', 'a', 't', '2026-10-04T01:00:00.000Z', '2026-10-04T01:10:00.000Z', 'sess-A')",
    )
    .bind(&bot.id)
    .execute(app.db())
    .await
    .unwrap();
    context_lost(&app, &bot, "transcript_missing", Some("sess-A")).await.unwrap();

    // Run B 結束，接著 context_lost(B)
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at, ended_at, native_session_id)
         VALUES ('run-b', ?, 'exited', 'idle', 'ws', 'p', 'a', 't', '2026-10-04T02:00:00.000Z', '2026-10-04T02:10:00.000Z', 'sess-B')",
    )
    .bind(&bot.id)
    .execute(app.db())
    .await
    .unwrap();
    context_lost(&app, &bot, "transcript_missing", Some("sess-B")).await.unwrap();

    assert_eq!(notes(&app, &conv).await.len(), 2);
    assert_eq!(active_notice_sessions(&app, &bot.id).await, vec![Some("sess-A".into()), Some("sess-B".into())]);

    // 接回 A：只有 WA 被撤銷，WB 依然在
    retire_context_lost(&app, &bot.id, "sess-A").await;
    let left = notes(&app, &conv).await;
    assert_eq!(left.len(), 1, "WA 撤銷後只剩 WB: {left:?}");
    assert_eq!(active_notice_sessions(&app, &bot.id).await, vec![Some("sess-B".into())]);

    // 接回 B：WB 被撤銷
    retire_context_lost(&app, &bot.id, "sess-B").await;
    let left = notes(&app, &conv).await;
    assert_eq!(left.len(), 0, "全部接回後警告清空: {left:?}");
    assert!(active_notice_sessions(&app, &bot.id).await.is_empty());
}

/// 倒序接回：後發生的 session B 先接回，先發生的 session A 警告不受影響。
#[tokio::test]
async fn recovery_in_reverse_chronological_order() {
    let env = crate::testing::env().await;
    let app = env.app.clone();
    let bot = crate::testing::claude_bot(&app, &env.project_id, "rpa").await;
    let conv = db::conversation_id(app.db(), &bot.id).await.unwrap();

    context_lost(&app, &bot, "transcript_missing", Some("sess-A")).await.unwrap();
    context_lost(&app, &bot, "transcript_missing", Some("sess-B")).await.unwrap();
    assert_eq!(notes(&app, &conv).await.len(), 2);

    // 先接回 B
    retire_context_lost(&app, &bot.id, "sess-B").await;
    let left = notes(&app, &conv).await;
    assert_eq!(left.len(), 1, "B 撤銷後只剩 A: {left:?}");
    assert_eq!(active_notice_sessions(&app, &bot.id).await, vec![Some("sess-A".into())]);

    // 再接回 A
    retire_context_lost(&app, &bot.id, "sess-A").await;
    let left = notes(&app, &conv).await;
    assert_eq!(left.len(), 0, "A 也撤銷: {left:?}");
    assert!(active_notice_sessions(&app, &bot.id).await.is_empty());
}

/// 同一個 session 重複失敗（多次 context_lost），接回時一次全數撤銷。
#[tokio::test]
async fn repeated_failures_for_same_session_retire_together() {
    let env = crate::testing::env().await;
    let app = env.app.clone();
    let bot = crate::testing::claude_bot(&app, &env.project_id, "rpa").await;
    let conv = db::conversation_id(app.db(), &bot.id).await.unwrap();

    context_lost(&app, &bot, "transcript_missing", Some("sess-A")).await.unwrap();
    context_lost(&app, &bot, "resume_mismatch", Some("sess-A")).await.unwrap();
    context_lost(&app, &bot, "transcript_missing", Some("sess-B")).await.unwrap();
    assert_eq!(notes(&app, &conv).await.len(), 3);

    // 接回 A：A 的兩則警告都撤銷，B 仍留著
    retire_context_lost(&app, &bot.id, "sess-A").await;
    let left = notes(&app, &conv).await;
    assert_eq!(left.len(), 1, "A 的重複警告撤銷，B 留著: {left:?}");
    assert_eq!(active_notice_sessions(&app, &bot.id).await, vec![Some("sess-B".into())]);

    // 接回 B
    retire_context_lost(&app, &bot.id, "sess-B").await;
    assert_eq!(notes(&app, &conv).await.len(), 0);
    assert!(active_notice_sessions(&app, &bot.id).await.is_empty());
}
