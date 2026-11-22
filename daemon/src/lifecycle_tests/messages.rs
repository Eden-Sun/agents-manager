#![allow(unused_imports)]

use crate::lifecycle_tests::prelude::*;
use crate::db;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use crate::state::App;
use crate::lifecycle::*;
use crate::lifecycle::start::{restart_bot_with, StartOpts};
use crate::lifecycle::messages::*;
use crate::lifecycle::messages::test_seams::*;

mod tests {
    use super::*;
    use super::*;
    use crate::testing as tt;
    use tokio::sync::broadcast::error::TryRecvError;

    #[tokio::test]
    async fn unreadable_message_owner_does_not_persist_or_publish_until_retry() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "message-owner-retry").await;
        let conversation_id = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let mut events = app.subscribe();

        tt::make_table_unreadable(&app, "conversations").await;
        let first = insert_message(&app, &conversation_id, None, "assistant", "reply", "hook", false, None).await;
        tt::make_table_readable(&app, "conversations").await;

        assert!(first.is_err(), "owner lookup failure must be returned for retry: {first:?}");
        let failed_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id=?")
            .bind(&conversation_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(failed_rows, 0, "the failed attempt must not leave an unrouteable durable message");
        assert!(matches!(events.try_recv(), Err(TryRecvError::Empty)), "an unverified owner cannot be published");

        let retried = insert_message(&app, &conversation_id, None, "assistant", "reply", "hook", false, None)
            .await
            .unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match events.recv().await {
                    Ok(event) if event.kind == "message_added" && event.data["message"]["id"] == retried.id => break event,
                    Ok(_) => continue,
                    Err(error) => panic!("event bus closed before message retry: {error}"),
                }
            }
        })
        .await
        .expect("the retry should publish its message");
        assert_eq!(event.data["bot_id"], bot.id);
        assert_eq!(event.data["message"]["id"], retried.id);
        assert!(matches!(events.try_recv(), Err(TryRecvError::Empty)), "the retry emits exactly one message event");
    }

    /// #822：讀完 owner、還沒 INSERT 訊息的那一瞬，一個不相干的 writer commit 了一筆。deferred 交易這時 INSERT 直接 517，
    /// `let _ = insert_message(..)` 的呼叫端連說明訊息都默默丟了。寫鎖從讀之前就拿著：插進來的那一筆等，訊息寫進去一次、
    /// 事件帶對的 owner。
    #[tokio::test]
    async fn an_unrelated_writer_between_the_owner_read_and_the_insert_does_not_lose_the_message() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "message-owner-831").await;
        let conversation_id = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let mut events = app.subscribe();
        let other = tt::arm_app_foreign_writer(&app, "insert_message_after_owner_read", &conversation_id);

        let m = insert_message(&app, &conversation_id, None, "system", "說明", "system", false, None)
            .await
            .expect("an unrelated writer must not make the message insert fail");
        assert_eq!(*other.lock().unwrap(), Some(false), "the insert holds the write lock from its owner read on; the other writer waits");
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id=?").bind(&conversation_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(rows, 1);
        let event = loop {
            let ev = events.recv().await.unwrap();
            if ev.kind == "message_added" {
                break ev;
            }
        };
        assert_eq!(event.data["bot_id"], bot.id, "the event carries the owner read in the same transaction");
        assert_eq!(event.data["message"]["id"], m.id);
    }

    #[tokio::test]
    async fn unreadable_turn_owner_does_not_publish_an_empty_owner_and_recovers() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "emit-turn-owner").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        let conversation = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let terminal = db::ulid();
        let queued = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?, ?, ?, 'web', 'failed', 'unknown', ?)")
            .bind(&terminal).bind(&conversation).bind(&run_id).bind(db::now()).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at, prompt_text) VALUES (?, ?, 'web', 'queued', 'pending', ?, 'wake after terminal turn')")
            .bind(&queued).bind(&conversation).bind(db::now()).execute(&app.db).await.unwrap();

        let mut turn_events = app.subscribe_turns();
        let mut ws_events = app.subscribe();
        let mut conn = app.db.acquire().await.unwrap();
        sqlx::query("PRAGMA foreign_keys=OFF").execute(&mut *conn).await.unwrap();
        sqlx::query("UPDATE conversations SET bot_id=CAST(bot_id AS BLOB) WHERE id=?").bind(&conversation).execute(&mut *conn).await.unwrap();
        drop(conn);

        emit_turn(&app, &terminal).await;

        assert!(matches!(turn_events.try_recv(), Err(TryRecvError::Empty)), "a terminal event needs a readable owner");
        assert!(matches!(ws_events.try_recv(), Err(TryRecvError::Empty)), "do not publish turn_updated with an empty bot id");

        let mut conn = app.db.acquire().await.unwrap();
        sqlx::query("UPDATE conversations SET bot_id=? WHERE id=?").bind(&bot.id).bind(&conversation).execute(&mut *conn).await.unwrap();
        sqlx::query("PRAGMA foreign_keys=ON").execute(&mut *conn).await.unwrap();
        drop(conn);

        let recovered = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                if let Ok(event) = turn_events.recv().await {
                    if event.turn_id == terminal { break event; }
                }
            }
        }).await.expect("owner lookup should retry after SQLite recovers");
        assert_eq!(recovered.bot_id, bot.id);
        assert_eq!(crate::lifecycle::take_scheduled_flush_count(&bot.id), 1, "the real owner receives the queue wake-up");
        assert_eq!(sqlx::query_scalar::<_, String>("SELECT status FROM turns WHERE id=?").bind(&queued).fetch_one(&app.db).await.unwrap(), "queued", "the test scheduler is intentionally inert");
    }

    #[tokio::test]
    async fn queued_turn_wake_is_rearmed_after_restart_when_owner_read_recovers() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "emit-turn-restart").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        let conversation = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let terminal = db::ulid();
        let queued = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?, ?, ?, 'web', 'failed', 'unknown', ?)")
            .bind(&terminal).bind(&conversation).bind(&run_id).bind(db::now()).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at, prompt_text) VALUES (?, ?, 'web', 'queued', 'pending', ?, 'wake after daemon restart')")
            .bind(&queued).bind(&conversation).bind(db::now()).execute(&app.db).await.unwrap();

        let mut conn = app.db.acquire().await.unwrap();
        sqlx::query("PRAGMA foreign_keys=OFF").execute(&mut *conn).await.unwrap();
        sqlx::query("UPDATE conversations SET bot_id=CAST(bot_id AS BLOB) WHERE id=?").bind(&conversation).execute(&mut *conn).await.unwrap();
        drop(conn);
        emit_turn(&app, &terminal).await;

        let restarted = tt::restart_app(&env).await;
        forget_turn_event_retry(&terminal);
        assert!(rearm_queue_retries(&restarted).await.is_err(), "startup retains its queue-recovery debt while ownership is unreadable");

        let mut conn = restarted.db.acquire().await.unwrap();
        sqlx::query("UPDATE conversations SET bot_id=? WHERE id=?").bind(&bot.id).bind(&conversation).execute(&mut *conn).await.unwrap();
        sqlx::query("PRAGMA foreign_keys=ON").execute(&mut *conn).await.unwrap();
        drop(conn);

        let pane = format!("pane-{}", bot.id);
        env.herdr.set_agent("agent", &pane, true);
        env.herdr.live_pane(&pane, tt::LivePane { width: Some(120), ..Default::default() });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let app_for_fire = restarted.clone();
        let fire = move |bot_id: String| {
            let (app, tx) = (app_for_fire.clone(), tx.clone());
            tokio::spawn(async move {
                let ok = flush_queued_locked(&app, &bot_id).await.is_ok();
                let _ = tx.send((bot_id, ok));
            });
        };
        assert_eq!(rearm_queue_retries_with(&restarted, fire).await.unwrap(), 1);
        let (woken, flushed) = tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv()).await
            .expect("a durable queued row is retried after restart")
            .expect("the retry callback reports the bot");
        assert_eq!(woken, bot.id, "the owner is resolved from the recovered DB row");
        assert!(flushed, "the recovered bot's queued prompt is flushed");
        let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(&queued).fetch_one(&restarted.db).await.unwrap();
        assert_eq!(status, "in_flight", "the queued prompt is driven after restart");
    }
}
