//! #1862 S3b：從 `crates/am-supervisor/src/supervisor/bot_requests.rs` 搬來的 daemon 整合測試；``supervisor::bot_requests::flow_tests::…` → `supervisor_tests::bot_requests::flow_tests::…``。
//! 測試本文與斷言不變；私有項由 am-supervisor 的 `*_for_test`（test-hooks）轉呼叫，下面用別名維持原名。

#![allow(unused_imports)]

use crate::supervisor::bot_requests::*;
use am_base::agent_relay::ct_eq;
use crate::lifecycle::LcError;
use sqlx::SqlitePool;
use serde_json::{json, Value};
use crate::supervisor::roles::{self, Role};
use crate::supervisor::store;
use crate::state::App;
use std::sync::Arc;
use super::flow_fixture::{app, configure_responder};

mod flow_tests {
    use super::*;

    /// 端到端：巡檢用 `assign`（persona 教的那條路）交接給協調者 → 進協調者的佇列，不是一件
    /// 交辦、也不開任何回合；協調者回一句明講 `--ack` 的 → 只記錄；沒標記的 → 叫醒巡檢。
        #[tokio::test]
    async fn an_assignment_addressed_to_a_role_goes_through_the_queue_and_only_a_marked_reply_stays_quiet() {
        let app = app().await;
        configure_responder(&app).await;
        let out = crate::supervisor::assign(
            &app, "resp", "交接：builder 卡在 rebase，請你接手協調", "handover-1", None, &[], None, false, None, Some(Role::Patrol), ReplyMark::default(),
        )
        .await
        .unwrap();
        assert_eq!(out["kind"], "handover");
        assert_eq!(out["routed"], "responder");
        assert_eq!(out["wake"], true);
        assert_eq!(out["delivery"], "queued");
        assert!(out["turn_id"].is_null());
        let assignments: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_assignments").fetch_one(&app.db).await.unwrap();
        assert_eq!(assignments, 0, "交接不是交辦，不會留下要驗收的工作");
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns").fetch_one(&app.db).await.unwrap();
        assert_eq!(turns, 0, "沒有任何模型被叫醒");
        let ev = roles::due_for(&app.db, Role::Responder, true, "2999-01-01T00:00:00Z", 0).await.unwrap();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].wake, Some(1));
        let payload: serde_json::Value = serde_json::from_str(&ev[0].payload_json).unwrap();
        assert_eq!(payload["from_bot_id"], "patrol");
        assert_eq!(payload["via"], "assignment");

        // 協調者被那一批叫醒；它在同一個回合裡明講 `--ack` 回巡檢一句「收到，我接手」→ 只記錄。
        turn_for(&app, "resp", "run-r", "c-r", "t-r").await;
        roles::mark_delivered(&app.db, &[ev[0].id.clone()], Role::Responder, "t-r", "ok").await.unwrap();
        let back = crate::supervisor::assign(
            &app, "patrol", "收到，我接手 builder", "handover-1-ack", None, &[], None, false, None, Some(Role::Responder),
            ReplyMark { ack: true, reply_to: None },
        )
        .await
        .unwrap();
        assert_eq!(back["routed"], "patrol");
        assert_eq!(back["wake"], false, "明講 ack 的回信只記錄，不叫醒巡檢");
        let patrol_due = roles::due_for(&app.db, Role::Patrol, true, "2999-01-01T00:00:00Z", 5).await.unwrap();
        assert!(patrol_due.iter().all(|e| e.wake == Some(0)));

        // 同一個回合裡另一句「要使用者裁示」沒有標記：那是新的事，叫醒巡檢（review 2026-09-16 H1 的第二條路）。
        let ask = crate::supervisor::assign(
            &app, "patrol", "builder 要刪使用者的 worktree，需要使用者裁示", "handover-1-ask", None, &[], None, false, None, Some(Role::Responder),
            ReplyMark::default(),
        )
        .await
        .unwrap();
        assert_eq!(ask["wake"], true);
        let patrol_due = roles::due_for(&app.db, Role::Patrol, true, "2999-01-01T00:00:00Z", 5).await.unwrap();
        assert_eq!(patrol_due.iter().filter(|e| e.wake == Some(1)).count(), 1);

        // 對一般 bot 的交辦帶 ack 是呼叫端搞錯了，不要默默吞掉。
        let err = crate::supervisor::assign(
            &app, "w1", "去做", "ack-to-worker", None, &[], None, false, None, Some(Role::Responder), ReplyMark { ack: true, reply_to: None },
        )
        .await;
        assert!(matches!(err, Err(LcError::Bad(_))), "{err:?}");

        // 對自己交辦仍然是錯的。
        let err = crate::supervisor::assign(
            &app, "resp", "自言自語", "self-1", None, &[], None, false, None, Some(Role::Responder), ReplyMark::default(),
        )
        .await;
        assert!(err.is_err());
    }

    /// 兩個角色同時在回合中時，交辦的來源要綁**呼叫者自己**。照固定順序先撿巡檢的回合，
    /// 協調者派的工就會被記成「使用者對巡檢說的另一句話」，授權與稽核從此對不上人。
        #[tokio::test]
    async fn the_assignment_source_follows_the_caller_not_whoever_is_busy_first() {
        let app = app().await;
        configure_responder(&app).await;
        for (bot, run, conv, turn, said) in [
            ("patrol", "run-p", "c-p", "t-p", "幫我看一下登入流程"),
            ("resp", "run-r", "c-r", "t-r", "builder 申請重建 abc123"),
        ] {
            turn_for(&app, bot, run, conv, turn).await;
            sqlx::query("INSERT INTO messages (id,conversation_id,turn_id,role,content,source,created_at) VALUES (?,?,?,'user',?, 'web', ?)")
                .bind(crate::db::ulid())
                .bind(conv)
                .bind(turn)
                .bind(said)
                .bind(crate::db::now())
                .execute(&app.db)
                .await
                .unwrap();
        }

        let by_responder = crate::supervisor::assign(
            &app, "w1", "去修 builder 的 rebase", "src-resp", None, &[], None, true, None, Some(Role::Responder), ReplyMark::default(),
        )
        .await
        .unwrap();
        let by_patrol = crate::supervisor::assign(
            &app, "w2", "去看登入流程", "src-patrol", None, &[], None, true, None, Some(Role::Patrol), ReplyMark::default(),
        )
        .await
        .unwrap();
        let src = |a: &serde_json::Value| -> (String, String) {
            let rid = a["request_id"].as_str().unwrap().to_string();
            (rid, String::new())
        };
        let (rid_r, _) = src(&by_responder);
        let (rid_p, _) = src(&by_patrol);
        let text_of = |rid: String| {
            let db = app.db.clone();
            async move {
                sqlx::query_as::<_, (String, Option<String>)>("SELECT text, source_key FROM supervisor_requests WHERE id=?")
                    .bind(rid)
                    .fetch_one(&db)
                    .await
                    .unwrap()
            }
        };
        let (text_r, key_r) = text_of(rid_r).await;
        assert_eq!(key_r.as_deref(), Some("t-r"), "協調者派的工綁它自己的回合");
        assert_eq!(text_r, "builder 申請重建 abc123");
        let (text_p, key_p) = text_of(rid_p).await;
        assert_eq!(key_p.as_deref(), Some("t-p"));
        assert_eq!(text_p, "幫我看一下登入流程");

        // 認不出呼叫者就不猜：記成交辦文字本身，不替某個人編一句話。
        let anon = crate::supervisor::assign(
            &app, "w1", "腳本派的例行工作", "src-anon", None, &[], None, true, None, None, ReplyMark::default(),
        )
        .await
        .unwrap();
        let (text_a, key_a) = text_of(anon["request_id"].as_str().unwrap().to_string()).await;
        assert_eq!(key_a, None);
        assert_eq!(text_a, "腳本派的例行工作");

        // 明講別人的回合也不行。
        let err = crate::supervisor::assign(
            &app, "w1", "借別人的授權", "src-borrow", Some("t-p"), &[], None, true, None, Some(Role::Responder), ReplyMark::default(),
        )
        .await;
        assert!(err.is_err(), "協調者不能指巡檢的回合當來源");
    }

    /// 協調者的 bot 被刪掉之後，申請仍然排給它（不倒回巡檢），狀態說得出是 `missing`，
    /// 並且巡檢會收到一則「它不見了」。
        #[tokio::test]
    async fn a_deleted_responder_bot_does_not_send_its_queue_back_to_patrol() {
        let app = app().await;
        configure_responder(&app).await;
        sqlx::query("UPDATE bots SET deleted_at=? WHERE id='resp'").bind(crate::db::now()).execute(&app.db).await.unwrap();

        let out = intercept(&app, "patrol", "w1", "請協調 ownership", Some("r-9"), &[], true, "api", ReplyMark::default()).await.unwrap().unwrap();
        assert_eq!(out["routed"], "responder");
        assert!(roles::due_for(&app.db, Role::Patrol, true, "2999-01-01T00:00:00Z", 5).await.unwrap().is_empty(), "不倒回巡檢");
        assert_eq!(roles::due_for(&app.db, Role::Responder, true, "2999-01-01T00:00:00Z", 0).await.unwrap().len(), 1);

        crate::supervisor::responder::notify(&app).await;
        let kinds: Vec<String> = sqlx::query_scalar("SELECT kind FROM supervisor_inbox ORDER BY created_at").fetch_all(&app.db).await.unwrap();
        assert!(kinds.contains(&"responder_bot_missing".to_string()), "{kinds:?}");
        let status = crate::supervisor::responder::status_json(&app).await.unwrap();
        assert_eq!(status["status"], "missing");
        assert_eq!(status["configured"], true);
        assert_eq!(status["bot_present"], false);
        // 再跑一次不會重複報。
        crate::supervisor::responder::notify(&app).await;
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE kind='responder_bot_missing'").fetch_one(&app.db).await.unwrap();
        assert_eq!(n, 1);
    }

    // 測試本地輔助（#1862 S3b）：與 am-supervisor crate-only 版同一份本體，這裡是 daemon `App` 版。
    async fn turn_for(app: &Arc<App>, bot: &str, run: &str, conv: &str, turn: &str) {
        let now = crate::db::now();
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES (?,?,'running','working',?)")
            .bind(run).bind(bot).bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO conversations (id,bot_id,created_at) VALUES (?,?,?)")
            .bind(conv).bind(bot).bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,created_at) VALUES (?,?,?,'web','in_flight',?)")
            .bind(turn).bind(conv).bind(run).bind(&now).execute(&app.db).await.unwrap();
    }
}
