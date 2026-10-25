use super::*;
use crate::testing as tt;

struct CodexFixture {
    env: tt::Env,
    bot_id: String,
    run_id: String,
    conv_id: String,
    turn_id: String,
}

async fn setup_codex_fixture(session: &str, _native_turn: &str) -> CodexFixture {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot = tt::claude_bot(&app, &env.project_id, "codex-test-bot").await;
    sqlx::query("UPDATE bots SET kind = 'codex' WHERE id = ?")
        .bind(&bot.id)
        .execute(&app.db)
        .await
        .unwrap();

    let run_id = tt::fake_run(&app, &bot.id).await;
    sqlx::query("UPDATE runs SET native_session_id = ? WHERE id = ?")
        .bind(session)
        .bind(&run_id)
        .execute(&app.db)
        .await
        .unwrap();

    let conv_id = db::conversation_id(&app.db, &bot.id).await.unwrap();
    let turn_id = db::ulid();
    let now = db::now();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, native_session_id, native_turn_id, created_at, delivered_at)
         VALUES (?,?,?,'web','in_flight','ok','hello codex',?,NULL,?,?)",
    )
    .bind(&turn_id)
    .bind(&conv_id)
    .bind(&run_id)
    .bind(session)
    .bind(&now)
    .bind(&now)
    .execute(&app.db)
    .await
    .unwrap();

    CodexFixture {
        env,
        bot_id: bot.id,
        run_id,
        conv_id,
        turn_id,
    }
}

#[tokio::test]
async fn codex_process_single_completion_order_legacy_then_stop() {
    let session = "sess-codex-ord1";
    let native_turn = "turn-native-001";
    let f = setup_codex_fixture(session, native_turn).await;

    let legacy_notify = HookBody {
        bot_id: f.bot_id.clone(),
        provider: "codex".into(),
        payload: json!({
            "type": "agent-turn-complete",
            "thread-id": session,
            "turn-id": native_turn,
            "input-messages": ["hello codex"],
            "last-assistant-message": "Legacy assistant reply"
        }),
        received_at: Some(db::iso_in(1)),
        truncated: false,
        run_id: Some(f.run_id.clone()),
    };

    let new_stop = HookBody {
        bot_id: f.bot_id.clone(),
        provider: "codex".into(),
        payload: json!({
            "hook_event_name": "Stop",
            "session_id": session,
            "turn_id": native_turn,
            "last_assistant_message": "Duplicate stop reply",
            "stop_hook_active": false
        }),
        received_at: Some(db::iso_in(2)),
        truncated: false,
        run_id: Some(f.run_id.clone()),
    };

    // 舊 notify 先 process
    process(&f.env.app, &legacy_notify).await.unwrap();

    // 驗證原 turn completed、native IDs 留存、無額外 external turn
    let (status, nat_sess, nat_turn): (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT status, native_session_id, native_turn_id FROM turns WHERE id = ?",
    )
    .bind(&f.turn_id)
    .fetch_one(&f.env.app.db)
    .await
    .unwrap();
    assert_eq!(status, "completed");
    assert_eq!(nat_sess.as_deref(), Some(session));
    assert_eq!(nat_turn.as_deref(), Some(native_turn));

    let total_turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id = ?")
        .bind(&f.conv_id)
        .fetch_one(&f.env.app.db)
        .await
        .unwrap();
    assert_eq!(total_turns, 1, "不准產生額外 external turn");

    // 驗證 messages 中剛好只有單一 assistant message，內容為先到者
    let msgs: Vec<(String, String)> = sqlx::query_as(
        "SELECT role, content FROM messages WHERE turn_id = ? AND role = 'assistant'",
    )
    .bind(&f.turn_id)
    .fetch_all(&f.env.app.db)
    .await
    .unwrap();
    assert_eq!(msgs.len(), 1, "剛好單一 assistant message");
    assert_eq!(msgs[0].1, "Legacy assistant reply");

    // 新 Stop 到達：真 process 執行
    process(&f.env.app, &new_stop).await.unwrap();

    let (status2, total_turns2): (String, i64) = (
        sqlx::query_scalar("SELECT status FROM turns WHERE id = ?").bind(&f.turn_id).fetch_one(&f.env.app.db).await.unwrap(),
        sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id = ?").bind(&f.conv_id).fetch_one(&f.env.app.db).await.unwrap(),
    );
    assert_eq!(status2, "completed");
    assert_eq!(total_turns2, 1, "Stop 重複到達仍為單一 turn");

    let msgs2: Vec<(String, String)> = sqlx::query_as(
        "SELECT role, content FROM messages WHERE turn_id = ? AND role = 'assistant'",
    )
    .bind(&f.turn_id)
    .fetch_all(&f.env.app.db)
    .await
    .unwrap();
    assert_eq!(msgs2.len(), 1, "重複 completion 不可產生第二個 assistant message");
    assert_eq!(msgs2[0].1, "Legacy assistant reply", "原回覆不可被覆蓋");
}

#[tokio::test]
async fn codex_process_single_completion_order_stop_then_legacy() {
    let session = "sess-codex-ord2";
    let native_turn = "turn-native-002";
    let f = setup_codex_fixture(session, native_turn).await;

    let new_stop = HookBody {
        bot_id: f.bot_id.clone(),
        provider: "codex".into(),
        payload: json!({
            "hook_event_name": "Stop",
            "session_id": session,
            "turn_id": native_turn,
            "last_assistant_message": r#"{"title":"A normal JSON reply"}"#,
            "stop_hook_active": false
        }),
        received_at: Some(db::iso_in(1)),
        truncated: false,
        run_id: Some(f.run_id.clone()),
    };

    let legacy_notify = HookBody {
        bot_id: f.bot_id.clone(),
        provider: "codex".into(),
        payload: json!({
            "type": "agent-turn-complete",
            "thread-id": session,
            "turn-id": native_turn,
            "input-messages": ["hello codex"],
            "last-assistant-message": "Late legacy reply"
        }),
        received_at: Some(db::iso_in(2)),
        truncated: false,
        run_id: Some(f.run_id.clone()),
    };

    // 新 Stop 先 process
    process(&f.env.app, &new_stop).await.unwrap();

    let (status, nat_sess, nat_turn): (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT status, native_session_id, native_turn_id FROM turns WHERE id = ?",
    )
    .bind(&f.turn_id)
    .fetch_one(&f.env.app.db)
    .await
    .unwrap();
    assert_eq!(status, "completed");
    assert_eq!(nat_sess.as_deref(), Some(session));
    assert_eq!(nat_turn.as_deref(), Some(native_turn));

    let total_turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id = ?")
        .bind(&f.conv_id)
        .fetch_one(&f.env.app.db)
        .await
        .unwrap();
    assert_eq!(total_turns, 1, "不准產生額外 external turn");

    let msgs: Vec<(String, String)> = sqlx::query_as(
        "SELECT role, content FROM messages WHERE turn_id = ? AND role = 'assistant'",
    )
    .bind(&f.turn_id)
    .fetch_all(&f.env.app.db)
    .await
    .unwrap();
    assert_eq!(msgs.len(), 1, "剛好單一 assistant message");
    assert_eq!(msgs[0].1, r#"{"title":"A normal JSON reply"}"#);

    // 舊 notify 後 process
    process(&f.env.app, &legacy_notify).await.unwrap();

    let (status2, total_turns2): (String, i64) = (
        sqlx::query_scalar("SELECT status FROM turns WHERE id = ?").bind(&f.turn_id).fetch_one(&f.env.app.db).await.unwrap(),
        sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id = ?").bind(&f.conv_id).fetch_one(&f.env.app.db).await.unwrap(),
    );
    assert_eq!(status2, "completed");
    assert_eq!(total_turns2, 1);

    let msgs2: Vec<(String, String)> = sqlx::query_as(
        "SELECT role, content FROM messages WHERE turn_id = ? AND role = 'assistant'",
    )
    .bind(&f.turn_id)
    .fetch_all(&f.env.app.db)
    .await
    .unwrap();
    assert_eq!(msgs2.len(), 1, "重複 completion 不可產生第二個 assistant message");
    assert_eq!(msgs2[0].1, r#"{"title":"A normal JSON reply"}"#, "Stop 原回覆不可被覆蓋");
}
