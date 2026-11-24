#![allow(unused_imports)]

use crate::lifecycle_tests::prelude::*;
use crate::lifecycle::*;
use crate::lifecycle::s6_ports::*;
use crate::lifecycle::prompt::SEND_NOW_NOTE;
use crate::lifecycle::interruption::*;
use crate::lifecycle::interruption::test_seams::*;
use chrono::{DateTime, Utc};
use std::collections::HashMap;

#[cfg(test)]
mod tests {
    use super::*;

    use crate::testing as tt;

    struct Busy {
        env: tt::Env,
        bot: db::Bot,
        run: String,
        turn: String,
    }

    /// 一顆正在跑回合的 claude：herdr 有這個 agent（Esc 送得進去），run 上有一筆 in-flight。
    async fn busy(name: &str) -> Busy {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, name).await;
        let run = tt::fake_run(&app, &bot.id).await;
        env.herdr.set_agent("agent", &format!("pane-{}", bot.id), true);
        let conv = db::conversation_id(app.db(), &bot.id).await.unwrap();
        let turn = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at, prompt_text)
             VALUES (?,?,?,'web','in_flight','ok',?,'跑一下測試')",
        )
        .bind(&turn)
        .bind(&conv)
        .bind(&run)
        .bind(db::now())
        .execute(app.db())
        .await
        .unwrap();
        Busy { env, bot, run, turn }
    }

    /// SQLite 這一刻寫不進 `turns.status`（磁碟、鎖、I/O 錯誤）。
    async fn lose_turn_writes(app: &Arc<App>) {
        sqlx::query("CREATE TRIGGER lost_turn_write BEFORE UPDATE OF status ON turns BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END")
            .execute(app.db())
            .await
            .unwrap();
    }

    async fn heal_turn_writes(app: &Arc<App>) {
        sqlx::query("DROP TRIGGER lost_turn_write").execute(app.db()).await.unwrap();
    }

    async fn status_of(app: &Arc<App>, turn: &str) -> String {
        sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(turn).fetch_one(app.db()).await.unwrap()
    }

    async fn system_notes(app: &Arc<App>, turn: &str) -> Vec<String> {
        sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system' ORDER BY created_at")
            .bind(turn)
            .fetch_all(app.db())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_codex_interrupt_keeps_the_streamed_plan_marked_incomplete() {
        let b = busy("codex-interrupted-plan").await;
        let app = b.env.app.clone();
        sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&b.bot.id).execute(app.db()).await.unwrap();
        b.env.herdr.set_screen(
            &format!("pane-{}", b.bot.id),
            "› 跑一下測試\n\n• Proposed Plan\n  Compare the current behavior with the rollout.\n■ Conversation interrupted - tell the model what to do differently.\n",
        );

        close_turn(&app, &b.bot.id, &b.run, &b.turn, "interrupted by user").await.unwrap();

        assert_eq!(status_of(&app, &b.turn).await, "failed");
        let messages: Vec<(String, String, i64, String)> = sqlx::query_as(
            "SELECT role, content, incomplete, source FROM messages WHERE turn_id=? ORDER BY rowid",
        )
        .bind(&b.turn)
        .fetch_all(app.db())
        .await
        .unwrap();
        assert_eq!(
            messages,
            vec![
                (
                    "assistant".into(),
                    "Proposed Plan\n  Compare the current behavior with the rollout.".into(),
                    1,
                    "terminal_fallback".into(),
                ),
                ("system".into(), "interrupted by user".into(), 0, "system".into()),
            ],
        );
    }

    fn escs(env: &tt::Env) -> usize {
        env.herdr
            .calls_to("agent.send_keys")
            .iter()
            .filter(|p| p.get("keys").and_then(|k| k.as_array()).is_some_and(|k| k.iter().any(|k| k == "esc")))
            .count()
    }

    fn echo(bot_id: &str, detail: Option<&str>) -> crate::hookrecv::HookBody {
        let mut payload = json!({"hook_event_name": "StopFailure", "session_id": "s-esc", "prompt_id": "p-esc"});
        if let Some(d) = detail {
            payload["error"] = json!(d);
        }
        crate::hookrecv::HookBody {
            bot_id: bot_id.to_string(),
            provider: "claude".into(),
            payload,
            received_at: Some(db::now()),
            truncated: false,
            run_id: None,
        }
    }

    /// #147 驗收一：Esc 真的送進去了、回合卻沒收掉（`set_status` 失敗）——API 不能回普通的成功。
    /// 以前 `fail_in_flight` 把錯吞掉只記 warning，interrupt 照樣 200，回合永遠 in_flight。
    #[tokio::test]
    async fn an_esc_that_landed_while_the_turn_could_not_be_closed_is_not_a_plain_success() {
        let b = busy("esc-lost-write").await;
        let app = b.env.app.clone();
        lose_turn_writes(&app).await;

        let err = interrupt_bot(&app, &b.bot.id).await.expect_err("Esc 送進去、回合沒收掉：不能回普通的成功");
        let LcError::Uncommitted(body) = err else { panic!("要是可重試的結構化錯誤（503）：{err:?}") };
        assert_eq!(body["error"], "interrupt_state_uncommitted", "{body}");
        assert_eq!(body["turn_id"], b.turn.as_str(), "{body}");
        assert_eq!(body["run_id"], b.run.as_str(), "{body}");
        assert_eq!(body["esc_sent"], true, "{body}");
        assert_eq!(body["retryable"], true, "{body}");
        assert_eq!(escs(&b.env), 1);
        assert_eq!(status_of(&app, &b.turn).await, "in_flight", "寫不進去就是還沒收：不假裝收掉了");
    }

    /// #147 驗收二：寫失敗之後，那次 Esc 的 `StopFailure` 回聲到了——回合要在這裡收掉（照 Esc 的說明），
    /// 不能因為「回聲一律什麼都不動」而永遠卡在 in_flight；也不能被算成 provider／額度失敗。
    #[tokio::test]
    async fn the_esc_echo_finishes_the_turn_that_the_lost_write_left_in_flight() {
        let b = busy("esc-echo-recovers").await;
        let app = b.env.app.clone();
        lose_turn_writes(&app).await;
        interrupt_bot(&app, &b.bot.id).await.expect_err("寫不進去");
        heal_turn_writes(&app).await;

        let quota = "You've hit your session limit · resets 3pm (Asia/Taipei)";
        crate::hookrecv::process(&app, &echo(&b.bot.id, Some(quota))).await.unwrap();

        assert_eq!(status_of(&app, &b.turn).await, "failed", "回聲是 Esc 已經生效的證據：回合收掉");
        assert_eq!(system_notes(&app, &b.turn).await, vec!["interrupted by user".to_string()], "說明是使用者中斷，不是失敗收尾");
        assert_eq!(escs(&b.env), 1, "補收尾不再按鍵");
    }

    /// #147 驗收三：同一次中斷重試（使用者再按一次、呼叫端照 409 重送）——Esc 已經生效，不能再按一次
    /// （claude 閒著時連按兩次 Esc 會跳 rewind 選單）。只把欠著的那一半補上。
    #[tokio::test]
    async fn retrying_the_same_interrupt_never_presses_esc_twice() {
        let b = busy("esc-retry").await;
        let app = b.env.app.clone();
        lose_turn_writes(&app).await;
        interrupt_bot(&app, &b.bot.id).await.expect_err("寫不進去");

        let again = interrupt_bot(&app, &b.bot.id).await.expect_err("DB 還是寫不進去：照樣不是成功");
        let LcError::Uncommitted(body) = again else { panic!("{again:?}") };
        assert_eq!(body["error"], "interrupt_state_uncommitted", "{body}");
        assert_eq!(escs(&b.env), 1, "Esc 已經生效：重試不再按");

        heal_turn_writes(&app).await;
        interrupt_bot(&app, &b.bot.id).await.expect("DB 好了：重試把欠著的收尾補上就成功");
        assert_eq!(status_of(&app, &b.turn).await, "failed");
        assert_eq!(system_notes(&app, &b.turn).await, vec!["interrupted by user".to_string()]);
        assert_eq!(escs(&b.env), 1, "從頭到尾只按了一次");
    }

    /// #147 驗收三之二：重試綁著那一筆的身份。它已經收掉、下一回合已經開始時，帶著它的 `turn_id` 重試
    /// **不按 Esc**，回 409——不會誤傷下一回合。定時重試同樣只認記下的那一筆、從不按鍵。
    #[tokio::test]
    async fn a_retry_bound_to_the_old_turn_never_escapes_the_next_one() {
        let b = busy("esc-retry-bound").await;
        let app = b.env.app.clone();
        lose_turn_writes(&app).await;
        interrupt_bot(&app, &b.bot.id).await.expect_err("寫不進去");
        heal_turn_writes(&app).await;
        settle(&app, &b.bot.id).await.expect("定時重試補上了");
        assert_eq!(status_of(&app, &b.turn).await, "failed");

        let conv = db::conversation_id(app.db(), &b.bot.id).await.unwrap();
        let next = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
            .bind(&next)
            .bind(&conv)
            .bind(&b.run)
            .bind(db::now())
            .execute(app.db())
            .await
            .unwrap();
        settle(&app, &b.bot.id).await.unwrap();

        let err = interrupt_turn(&app, &b.bot.id, Some(&b.turn)).await.expect_err("那一筆已經不在飛");
        let LcError::Conflict(body) = err else { panic!("{err:?}") };
        assert_eq!(body["reason"], "turn_not_in_flight", "{body}");
        assert_eq!(body["in_flight_turn_id"], next.as_str(), "{body}");
        assert_eq!(escs(&b.env), 1, "沒有對下一回合按 Esc");
        assert_eq!(status_of(&app, &next).await, "in_flight");
    }

    /// 到達之後、拿到 bot 鎖之前：那一筆結束，排著的 prompt 被 flush 認領並送出（`delivery=ok`）。
    async fn hand_over_to_a_queued_turn(b: &Busy, next: &str) {
        let app = b.env.app.clone();
        let conv = db::conversation_id(app.db(), &b.bot.id).await.unwrap();
        let (old, run, next) = (b.turn.clone(), b.run.clone(), next.to_string());
        crate::race_point::arm("interrupt_after_arrival", &b.bot.id, move || async move {
            sqlx::query("UPDATE turns SET status='completed' WHERE id=?").bind(&old).execute(app.db()).await.unwrap();
            sqlx::query(
                "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at, prompt_text)
                 VALUES (?,?,?,'web','in_flight','ok',?,'排隊的那一則')",
            )
            .bind(&next)
            .bind(&conv)
            .bind(&run)
            .bind(db::now())
            .execute(app.db())
            .await
            .unwrap();
        });
    }

    /// issue #1474：使用者按下打斷時看到的是那一筆長回合；它剛好結束、排著的 prompt 接著送出——Esc 不能按在
    /// 使用者從沒看過是「在跑」的新回合上（以前：送出後 117ms 被收成 `interrupted by user`，claude 把字放回輸入框）。
    #[tokio::test]
    async fn an_interrupt_aimed_at_a_finished_turn_does_not_hit_the_queued_prompt_that_took_over() {
        let b = busy("esc-handoff").await;
        let app = b.env.app.clone();
        let next = db::ulid();
        hand_over_to_a_queued_turn(&b, &next).await;

        let err = interrupt_bot(&app, &b.bot.id).await.expect_err("按下去的那一筆已經結束");
        let LcError::Conflict(body) = err else { panic!("{err:?}") };
        assert_eq!(body["reason"], "turn_not_in_flight", "{body}");
        assert_eq!(body["turn_id"], b.turn.as_str(), "{body}");
        assert_eq!(body["in_flight_turn_id"], next.as_str(), "{body}");
        assert_eq!(body["esc_sent"], false, "{body}");
        assert_eq!(escs(&b.env), 0, "沒有對新回合按 Esc");
        assert_eq!(status_of(&app, &next).await, "in_flight", "排隊送出的那一則照舊在跑");
        assert!(system_notes(&app, &next).await.is_empty(), "也沒有被寫上 interrupted by user");
        assert!(pending(&b.bot.id).is_empty(), "不記帳");
    }

    /// 同一件事的另一個窗口：按下去的時候排著的 prompt 已經被 flush 認領、還在打字（`delivery=pending`、`auto_resend=0`），
    /// 畫面上它還是「排隊中」；等拿到鎖時它已經送出。那一筆不是使用者要停的。
    #[tokio::test]
    async fn an_interrupt_that_arrived_while_a_queued_prompt_was_being_typed_does_not_hit_it() {
        let b = busy("esc-claimed-typing").await;
        let app = b.env.app.clone();
        let conv = db::conversation_id(app.db(), &b.bot.id).await.unwrap();
        sqlx::query("UPDATE turns SET status='completed' WHERE id=?").bind(&b.turn).execute(app.db()).await.unwrap();
        let turn = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at, prompt_text)
             VALUES (?,?,'web','queued','pending',?,'排隊的 prompt')",
        )
        .bind(&turn)
        .bind(&conv)
        .bind(db::now())
        .execute(app.db())
        .await
        .unwrap();
        let mut conn = app.db().acquire().await.unwrap();
        assert_eq!(crate::lifecycle::turn_controller::claim_queued(&mut conn, &turn, &b.run).await.unwrap(), crate::lifecycle::turn_controller::Outcome::Applied);
        drop(conn);
        let (queued_turn, db_app) = (turn.clone(), app.clone());
        crate::race_point::arm("interrupt_after_arrival", &b.bot.id, move || async move {
            sqlx::query("UPDATE turns SET delivery='ok' WHERE id=?").bind(&queued_turn).execute(db_app.db()).await.unwrap();
        });

        let err = interrupt_bot(&app, &b.bot.id).await.expect_err("到達時它還在打字，不是使用者看到在跑的那一筆");
        let LcError::Conflict(body) = err else { panic!("{err:?}") };
        assert_eq!(body["reason"], "turn_not_in_flight", "{body}");
        assert!(body["turn_id"].is_null(), "{body}");
        assert_eq!(body["in_flight_turn_id"], turn.as_str(), "{body}");
        assert_eq!(escs(&b.env), 0);
        assert_eq!(status_of(&app, &turn).await, "in_flight");
    }

    /// 反面一：直接送出、還在打字的回合（`auto_resend` 還是預設的 1）是使用者剛送的那一則，照舊可以打斷。
    #[tokio::test]
    async fn a_directly_sent_turn_still_being_typed_can_be_interrupted() {
        let b = busy("esc-direct-typing").await;
        let app = b.env.app.clone();
        sqlx::query("UPDATE turns SET delivery='pending' WHERE id=?").bind(&b.turn).execute(app.db()).await.unwrap();

        interrupt_bot(&app, &b.bot.id).await.expect("照舊打斷");
        assert_eq!(escs(&b.env), 1);
        assert_eq!(status_of(&app, &b.turn).await, "failed");
    }

    /// issue #1482：走真正的 `prompt` INSERT，並在 `delivery='pending', auto_resend=0` 還沒開始打字時
    /// 記到 interrupt 的到達快照。送出路徑放行後，interrupt 拿到 bot 鎖仍應打斷同一筆。
    #[tokio::test]
    async fn a_direct_prompt_at_its_insert_state_can_be_interrupted() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "esc-direct-insert-state").await;
        let run = tt::fake_run(&app, &bot.id).await;
        env.herdr.set_agent("agent", &format!("pane-{}", bot.id), true);

        let (prompt_at_typing_tx, prompt_at_typing_rx) = tokio::sync::oneshot::channel();
        let (resume_prompt_tx, resume_prompt_rx) = tokio::sync::oneshot::channel();
        crate::race_point::arm("prompt_before_typing", &bot.id, move || async move {
            let _ = prompt_at_typing_tx.send(());
            let _ = resume_prompt_rx.await;
        });
        let prompt_app = app.clone();
        let prompt_bot = bot.id.clone();
        let prompt = tokio::spawn(async move {
            crate::lifecycle::prompt::prompt(&prompt_app, &prompt_bot, "直接送出的那一則", "issue-1482-direct").await
        });
        prompt_at_typing_rx.await.expect("direct prompt reached the pre-typing point");

        let turn: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE client_request_id='issue-1482-direct'")
            .fetch_one(app.db())
            .await
            .unwrap();
        assert_eq!((turn.status.as_str(), turn.delivery.as_str(), turn.auto_resend), ("in_flight", "pending", 0), "must be the production direct-prompt INSERT state");

        let (interrupt_at_arrival_tx, interrupt_at_arrival_rx) = tokio::sync::oneshot::channel();
        let (resume_interrupt_tx, resume_interrupt_rx) = tokio::sync::oneshot::channel();
        crate::race_point::arm("interrupt_after_arrival", &bot.id, move || async move {
            let _ = interrupt_at_arrival_tx.send(());
            let _ = resume_interrupt_rx.await;
        });
        let interrupt_app = app.clone();
        let interrupt_bot_id = bot.id.clone();
        let interrupt = tokio::spawn(async move { interrupt_bot(&interrupt_app, &interrupt_bot_id).await });
        interrupt_at_arrival_rx.await.expect("interrupt captured its arrival snapshot");

        let _ = resume_prompt_tx.send(());
        let sent = prompt.await.unwrap().expect("mock delivery returns a recorded turn");
        assert_eq!(sent.turn_id, turn.id);
        assert_eq!(db::active_run(app.db(), &bot.id).await.unwrap().unwrap().id, run);

        let _ = resume_interrupt_tx.send(());
        interrupt.await.unwrap().expect("the same direct turn is still what the user meant to interrupt");
        assert_eq!(escs(&env), 1);
        assert_eq!(status_of(&app, &turn.id).await, "failed");
    }

    async fn an_unreadable_arrival_snapshot_fails_closed(table: &'static str, name: &str) {
        let b = busy(name).await;
        let app = b.env.app.clone();
        tt::make_table_unreadable(&app, table).await;
        let restore_app = app.clone();
        let restore_table = table;
        let bot_id = b.bot.id.clone();
        crate::race_point::arm("interrupt_after_arrival", &bot_id, move || async move {
            tt::make_table_readable(&restore_app, restore_table).await;
        });

        let result = interrupt_bot(&app, &b.bot.id).await;
        crate::race_point::hit("interrupt_after_arrival", &b.bot.id).await;
        let err = result.expect_err("unreadable arrival state must produce a retryable error before Esc");
        let LcError::Unavailable(body) = err else { panic!("the arrival snapshot needs an explicit 503: {err:?}") };
        assert_eq!(body["error"], "interrupt_state_unavailable", "{body}");
        assert_eq!((body["retryable"].as_bool(), body["sent"].as_bool()), (Some(true), Some(false)), "{body}");
        assert_eq!(escs(&b.env), 0, "a failed arrival read must never authorize Esc");
        assert_eq!(status_of(&app, &b.turn).await, "in_flight");
    }

    #[tokio::test]
    async fn unreadable_run_or_turn_arrival_snapshot_never_sends_esc() {
        an_unreadable_arrival_snapshot_fails_closed("runs", "esc-arrival-run-unreadable").await;
        an_unreadable_arrival_snapshot_fails_closed("turns", "esc-arrival-turn-unreadable").await;
    }

    /// 反面二：到達時沒有在飛的回合，之後才冒出來的是 CLI 自己開的外部回合（畫面上本來就在跑）——照舊打斷它。
    #[tokio::test]
    async fn an_external_turn_opened_after_arrival_is_still_what_the_user_meant_to_stop() {
        let b = busy("esc-external-late").await;
        let app = b.env.app.clone();
        sqlx::query("UPDATE turns SET status='completed' WHERE id=?").bind(&b.turn).execute(app.db()).await.unwrap();
        let conv = db::conversation_id(app.db(), &b.bot.id).await.unwrap();
        let (run, ext, db_app) = (b.run.clone(), db::ulid(), app.clone());
        let ext_id = ext.clone();
        crate::race_point::arm("interrupt_after_arrival", &b.bot.id, move || async move {
            sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'external','in_flight','ok',?)")
                .bind(&ext_id)
                .bind(&conv)
                .bind(&run)
                .bind(db::now())
                .execute(db_app.db())
                .await
                .unwrap();
        });

        interrupt_bot(&app, &b.bot.id).await.expect("外部回合照舊打斷");
        assert_eq!(escs(&b.env), 1);
        assert_eq!(status_of(&app, &ext).await, "failed");
    }

    /// herdr 明確拒收 Esc：什麼都沒發生——502、回合照舊在飛、不記任何帳，之後的回聲式 hook 也不會把它收掉。
    #[tokio::test]
    async fn an_esc_herdr_refused_changes_nothing() {
        let b = busy("esc-refused").await;
        let app = b.env.app.clone();
        b.env.herdr.fail_next("agent.send_keys", tt::Fault::Refuse);

        let err = interrupt_bot(&app, &b.bot.id).await.expect_err("Esc 沒進去");
        assert!(matches!(err, LcError::Upstream(_)), "{err:?}");
        assert_eq!(status_of(&app, &b.turn).await, "in_flight");
        assert!(pending(&b.bot.id).is_empty(), "沒有欠著、也沒有待證");
        assert!(crate::lifecycle::interrupt_grace::pending_echo(&b.bot.id).is_none(), "也不等回聲");
    }

    /// Esc 送出去了但 herdr 沒回（不知道進了沒有）：不假定打斷——回合留在 in_flight、回 409 說清楚。
    /// 之後那次 Esc 的回聲到了才收（照 Esc 的說明）；回合自己正常答完的話就照答完收，帳作廢。
    #[tokio::test]
    async fn an_unanswered_esc_waits_for_its_echo_before_closing_anything() {
        let b = busy("esc-unknown").await;
        let app = b.env.app.clone();
        b.env.herdr.fail_next("agent.send_keys", tt::Fault::DropAfter);

        let err = interrupt_bot(&app, &b.bot.id).await.expect_err("不知道 Esc 進了沒有");
        let LcError::Conflict(body) = err else { panic!("{err:?}") };
        assert_eq!(body["reason"], "interrupt_unconfirmed", "{body}");
        assert_eq!(body["turn_id"], b.turn.as_str(), "{body}");
        assert_eq!(status_of(&app, &b.turn).await, "in_flight", "不假定打斷");
        settle(&app, &b.bot.id).await.unwrap();
        assert_eq!(status_of(&app, &b.turn).await, "in_flight", "時間過去不是證據");

        crate::hookrecv::process(&app, &echo(&b.bot.id, None)).await.unwrap();
        assert_eq!(status_of(&app, &b.turn).await, "failed", "回聲證明 Esc 進去了");
        assert_eq!(system_notes(&app, &b.turn).await, vec![INTERRUPT_NOTE.to_string()]);
        assert!(pending(&b.bot.id).is_empty());

        let c = busy("esc-unknown-finished").await;
        let app = c.env.app.clone();
        c.env.herdr.fail_next("agent.send_keys", tt::Fault::DropBefore);
        interrupt_bot(&app, &c.bot.id).await.expect_err("不知道");
        let stop = crate::hookrecv::HookBody {
            bot_id: c.bot.id.clone(),
            provider: "claude".into(),
            payload: json!({"hook_event_name": "Stop", "session_id": "s-done", "prompt_id": "p-done", "last_assistant_message": "測完了"}),
            received_at: None,
            truncated: false,
            run_id: None,
        };
        crate::hookrecv::process(&app, &stop).await.unwrap();
        assert_eq!(status_of(&app, &c.turn).await, "completed", "Esc 沒進去：照答完收");
        assert!(system_notes(&app, &c.turn).await.is_empty(), "不補「被中斷」");
        settle(&app, &c.bot.id).await.unwrap();
        assert!(pending(&c.bot.id).is_empty(), "那一筆不在飛了：帳作廢");
    }

    /// claude transcript 的一列（`at`＝claude 寫下它的時間，RFC 3339 UTC）。中斷標記照 2.1.278 實測的樣子（#178）：
    /// 帶 `interruptedMessageId`、`promptId` 是被中斷的那一則。
    fn prompt_line(text: &str, at: &str) -> String {
        json!({"type": "user", "promptId": "p-run", "timestamp": at, "message": {"role": "user", "content": text}}).to_string()
    }

    fn esc_marker(at: &str) -> String {
        json!({"type": "user", "promptId": "p-run", "interruptedMessageId": "msg_011CfCSBuE", "timestamp": at,
               "message": {"role": "user", "content": [{"type": "text", "text": "[Request interrupted by user]"}]}})
        .to_string()
    }

    fn at(offset_ms: i64) -> String {
        (chrono::Utc::now() + chrono::Duration::milliseconds(offset_ms)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    /// 這個 run 的 transcript（本機 claude）。
    async fn transcript(b: &Busy, lines: &[String]) -> std::path::PathBuf {
        let path = tt::trusted_transcript(&b.env.app, &b.bot.id, &b.env.dir, &format!("t-{}.jsonl", db::ulid())).await;
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        sqlx::query("UPDATE runs SET transcript_path=? WHERE id=?").bind(path.to_string_lossy()).bind(&b.run).execute(b.env.app.db()).await.unwrap();
        path
    }

    fn append(path: &std::path::Path, lines: &[String]) {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(f, "{}", lines.join("\n")).unwrap();
    }

    /// #223：claude 2.1.276～2.1.278 按 Esc 不送 Stop／StopFailure（#178 實測＋執行檔），herdr 沒回 Esc 時等回聲永遠等不到。
    /// Esc 生效當下 transcript 就寫了中斷標記——無損證據。放鎖之前就看到：當場照 Esc 收（跟 herdr 有回一樣），
    /// 不留給 §4.3 備援把它記成「答完了」、半截的字當回覆。
    #[tokio::test]
    async fn an_unanswered_esc_is_proven_by_the_interruption_the_transcript_recorded() {
        let b = busy("esc-unknown-logged").await;
        let app = b.env.app.clone();
        transcript(&b, &[prompt_line("跑一下測試", &at(0)), esc_marker(&at(300))]).await;
        b.env.herdr.fail_next("agent.send_keys", tt::Fault::DropAfter);

        interrupt_bot(&app, &b.bot.id).await.expect("transcript 證明 Esc 進去了：跟 herdr 有回一樣是成功");
        assert_eq!(status_of(&app, &b.turn).await, "failed");
        assert_eq!(system_notes(&app, &b.turn).await, vec![INTERRUPT_NOTE.to_string()]);
        assert!(pending(&b.bot.id).is_empty(), "證明了、收好了：沒有帳");
        assert_eq!(escs(&b.env), 1);
    }

    /// #223：當下還看不到（標記晚一點才寫）就照舊 409、留在 in_flight；之後結帳（hook、prompt、重試、定時重試）看到標記就收。
    #[tokio::test]
    async fn an_unanswered_esc_is_closed_once_the_transcript_shows_it_landed() {
        let b = busy("esc-unknown-late-log").await;
        let app = b.env.app.clone();
        let path = transcript(&b, &[prompt_line("跑一下測試", &at(0))]).await;
        b.env.herdr.fail_next("agent.send_keys", tt::Fault::DropAfter);

        let err = interrupt_bot(&app, &b.bot.id).await.expect_err("還看不出 Esc 進了沒有");
        let LcError::Conflict(body) = err else { panic!("{err:?}") };
        assert_eq!(body["reason"], "interrupt_unconfirmed", "{body}");
        settle(&app, &b.bot.id).await.unwrap();
        assert_eq!(status_of(&app, &b.turn).await, "in_flight", "log 裡還沒有：照舊等");

        append(&path, &[esc_marker(&at(0))]);
        settle(&app, &b.bot.id).await.unwrap();
        assert_eq!(status_of(&app, &b.turn).await, "failed", "標記證明 Esc 進去了");
        assert_eq!(system_notes(&app, &b.turn).await, vec![INTERRUPT_NOTE.to_string()]);
        assert!(pending(&b.bot.id).is_empty());
        assert_eq!(escs(&b.env), 1, "結帳從不按鍵");
    }

    /// #223 最糟的一種：Esc 生效（沒有任何 hook）、使用者直接在 pane 裡打了下一句，那一句的 Stop 先到。以前在飛的還是被中斷的
    /// 那一筆，下一句的回覆就掛到它身上、收成 completed。hook 一進來先結帳：被中斷的那一筆照 Esc 收，回覆以外部回合出現。
    #[tokio::test]
    async fn the_next_answer_after_an_unanswered_esc_is_not_pinned_on_the_interrupted_turn() {
        let b = busy("esc-unknown-then-typed").await;
        let app = b.env.app.clone();
        let path = transcript(&b, &[prompt_line("跑一下測試", &at(0))]).await;
        b.env.herdr.fail_next("agent.send_keys", tt::Fault::DropAfter);
        interrupt_bot(&app, &b.bot.id).await.expect_err("還看不出來");

        append(&path, &[esc_marker(&at(0)), prompt_line("算了，我自己來", &at(10))]);
        let stop = crate::hookrecv::HookBody {
            bot_id: b.bot.id.clone(),
            provider: "claude".into(),
            payload: json!({"hook_event_name": "Stop", "session_id": "s-next", "prompt_id": "p-next", "last_assistant_message": "下一句的回覆"}),
            received_at: None,
            truncated: false,
            run_id: None,
        };
        crate::hookrecv::process(&app, &stop).await.unwrap();

        assert_eq!(status_of(&app, &b.turn).await, "failed", "被中斷的那一筆照 Esc 收");
        let replies: Vec<(String, String)> = sqlx::query_as(
            "SELECT m.turn_id, m.content FROM messages m JOIN turns t ON t.id = m.turn_id WHERE t.run_id=? AND m.role='assistant'",
        )
        .bind(&b.run)
        .fetch_all(app.db())
        .await
        .unwrap();
        assert_eq!(replies.len(), 1, "{replies:?}");
        assert_ne!(replies[0].0, b.turn, "回覆不掛在被中斷的那一筆上");
        assert_eq!(replies[0].1, "下一句的回覆");
    }

    /// 證據要是**這一次** Esc 的：這一回合開始之前就留在 transcript 裡的中斷標記（上一回合被按停的）不算——
    /// 就算它離按鍵不到一秒、落在時鐘放寬的範圍裡。
    #[tokio::test]
    async fn an_older_interruption_in_the_transcript_does_not_prove_this_esc() {
        let b = busy("esc-unknown-old-marker").await;
        let app = b.env.app.clone();
        let began: String = sqlx::query_scalar("SELECT created_at FROM turns WHERE id=?").bind(&b.turn).fetch_one(app.db()).await.unwrap();
        let just_before = (chrono::DateTime::parse_from_rfc3339(&began).unwrap() - chrono::Duration::milliseconds(200))
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        transcript(&b, &[prompt_line("上一件事", &at(-60_000)), esc_marker(&just_before), prompt_line("跑一下測試", &at(0))]).await;
        b.env.herdr.fail_next("agent.send_keys", tt::Fault::DropAfter);

        let err = interrupt_bot(&app, &b.bot.id).await.expect_err("舊的標記不是證據");
        let LcError::Conflict(body) = err else { panic!("{err:?}") };
        assert_eq!(body["reason"], "interrupt_unconfirmed", "{body}");
        settle(&app, &b.bot.id).await.unwrap();
        assert_eq!(status_of(&app, &b.turn).await, "in_flight");
    }

    /// 寫失敗之後，使用者直接在 pane 裡打了下一句、它的 Stop 先到：被 Esc 停掉的那一筆要先收掉（hook 一進來先補帳），
    /// 下一句的回覆才不會掛到它身上、把它記成「答完了」。
    #[tokio::test]
    async fn the_next_answer_after_a_lost_interrupt_write_is_not_pinned_on_the_interrupted_turn() {
        let b = busy("esc-then-typed").await;
        let app = b.env.app.clone();
        lose_turn_writes(&app).await;
        interrupt_bot(&app, &b.bot.id).await.expect_err("寫不進去");
        heal_turn_writes(&app).await;

        let stop = crate::hookrecv::HookBody {
            bot_id: b.bot.id.clone(),
            provider: "claude".into(),
            payload: json!({"hook_event_name": "Stop", "session_id": "s-next", "prompt_id": "p-next", "last_assistant_message": "下一句的回覆"}),
            received_at: None,
            truncated: false,
            run_id: None,
        };
        crate::hookrecv::process(&app, &stop).await.unwrap();

        assert_eq!(status_of(&app, &b.turn).await, "failed", "被 Esc 停掉的就是被停掉的");
        assert_eq!(system_notes(&app, &b.turn).await, vec![INTERRUPT_NOTE.to_string()]);
        let pinned: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'")
            .bind(&b.turn)
            .fetch_one(app.db())
            .await
            .unwrap();
        assert_eq!(pinned, 0, "下一句的回覆沒有掛到被中斷的回合上");
    }

    /// 不知道 Esc 進了沒有，然後 claude 自己說「使用者中斷」（payload 就講得出來的那種回聲）：一樣是證據。
    #[tokio::test]
    async fn an_unanswered_esc_is_also_proven_by_an_echo_that_says_so_itself() {
        let b = busy("esc-unknown-says-so").await;
        let app = b.env.app.clone();
        b.env.herdr.fail_next("agent.send_keys", tt::Fault::DropBefore);
        interrupt_bot(&app, &b.bot.id).await.expect_err("不知道");

        crate::hookrecv::process(&app, &echo(&b.bot.id, Some("[Request interrupted by user]"))).await.unwrap();
        assert_eq!(status_of(&app, &b.turn).await, "failed");
        assert_eq!(system_notes(&app, &b.turn).await, vec![INTERRUPT_NOTE.to_string()]);
    }

    /// #147 驗收五（沒有 hook、也沒有下一則 prompt）：定時重試自己把欠著的補上——排隊 flush 看的是「這個 run
    /// 有沒有 in-flight」，那筆幽靈不在了，排著的派工只剩 Esc 本來就有的寬限（§4.4a），不會永久卡住。
    #[tokio::test]
    async fn the_retry_alone_unblocks_the_queue_after_a_lost_interrupt_write() {
        let b = busy("esc-queue").await;
        let app = b.env.app.clone();
        lose_turn_writes(&app).await;
        interrupt_bot(&app, &b.bot.id).await.expect_err("寫不進去");
        assert!(db::in_flight_turn(app.db(), &b.run).await.unwrap().is_some(), "還擋著");

        settle(&app, &b.bot.id).await.expect_err("DB 還沒好：這一輪補不上，帳留著");
        assert!(owes(&b.bot.id, &b.turn));
        heal_turn_writes(&app).await;
        settle(&app, &b.bot.id).await.expect("定時重試的下一輪");
        assert!(db::in_flight_turn(app.db(), &b.run).await.unwrap().is_none(), "不再擋著排隊的那一則");
        assert!(!owes(&b.bot.id, &b.turn), "帳結清了");
        assert_eq!(escs(&b.env), 1, "重試從不按鍵");
    }

    /// 強制中止同一個洞：以前 `fail_in_flight` 吞錯，迴圈又因為它在 `aborted` 裡跳過它——回 200、它卻還在飛。
    #[tokio::test]
    async fn an_abort_whose_write_was_lost_is_not_reported_as_done() {
        let b = busy("abort-lost-write").await;
        let app = b.env.app.clone();
        lose_turn_writes(&app).await;

        let err = abort_turns(&app, &b.bot.id).await.expect_err("回合沒收掉：不能回 aborted");
        assert!(matches!(err, LcError::Upstream(_)), "{err:?}");
        assert_eq!(status_of(&app, &b.turn).await, "in_flight");

        heal_turn_writes(&app).await;
        settle(&app, &b.bot.id).await.unwrap();
        assert_eq!(status_of(&app, &b.turn).await, "failed");
        assert_eq!(system_notes(&app, &b.turn).await, vec!["回合已由使用者強制中止".to_string()]);
    }

    /// #147 驗收四：DB 正常時跟以前一模一樣——一次 Esc、回合收成 failed、一則說明、回 Ok、不留帳。
    #[tokio::test]
    async fn with_a_healthy_db_an_interrupt_is_exactly_what_it_was() {
        let b = busy("esc-healthy").await;
        let app = b.env.app.clone();
        interrupt_bot(&app, &b.bot.id).await.expect("照舊成功");
        assert_eq!(status_of(&app, &b.turn).await, "failed");
        assert_eq!(system_notes(&app, &b.turn).await, vec![INTERRUPT_NOTE.to_string()]);
        assert_eq!(escs(&b.env), 1);
        assert!(pending(&b.bot.id).is_empty());
        assert!(crate::lifecycle::interrupt_grace::pending_echo(&b.bot.id).is_some(), "照舊等回聲（#117）");
    }

    /// #120：插隊送出途中 daemon 停了——還沒掛上 run 的那一則（`run_id = NULL` 的 in_flight）沒有人會收，而且它會一直
    /// 擋住維護窗口的閘門。重啟時收成 failed、送達 unknown、寫明原因；被插隊的那一筆照舊在它的 run 上，由 hook／watchdog 收。
    #[tokio::test]
    async fn a_send_now_cut_off_by_a_restart_is_closed_as_unknown() {
        let b = busy("send-now-restart").await;
        let app = b.env.app.clone();
        let conv = db::conversation_id(app.db(), &b.bot.id).await.unwrap();
        let orphan = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at, prompt_text)
             VALUES (?,?,NULL,'web','in_flight','pending',?,'先看這句')",
        )
        .bind(&orphan)
        .bind(&conv)
        .bind(db::now())
        .execute(app.db())
        .await
        .unwrap();

        let fresh = tt::restart_app(&b.env).await;
        crate::reconcile::rearm_progress(&fresh).await;

        let (status, delivery): (String, String) =
            sqlx::query_as("SELECT status, delivery FROM turns WHERE id=?").bind(&orphan).fetch_one(app.db()).await.unwrap();
        assert_eq!((status.as_str(), delivery.as_str()), ("failed", "unknown"));
        assert!(system_notes(&app, &orphan).await.iter().any(|n| n.contains("daemon 停了")));
        assert_eq!(status_of(&app, &b.turn).await, "in_flight", "被插隊的那一筆不動");
    }

    /// 插隊送出的鍵**生效了**（transcript 裡已經有新那一則），daemon 在把舊的那一筆收掉、新的掛上 run 之前停了。重啟後
    /// claude 替新那一則送的 Stop 不能掛到被插隊的那一筆上：舊的照被插隊打斷收，回覆歸新的那一則（或外部回合），不歸舊的。
    #[tokio::test]
    async fn after_a_restart_the_send_now_answer_is_not_pinned_on_the_turn_it_interrupted() {
        let b = busy("send-now-restart-answer").await;
        let app = b.env.app.clone();
        let conv = db::conversation_id(app.db(), &b.bot.id).await.unwrap();
        let orphan = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at, prompt_text)
             VALUES (?,?,NULL,'web','in_flight','pending',?,'先看這句')",
        )
        .bind(&orphan)
        .bind(&conv)
        .bind(at(-50))
        .execute(app.db())
        .await
        .unwrap();
        let path = transcript(&b, &[prompt_line("跑一下測試", &at(-5_000)), esc_marker(&at(-10)), prompt_line("先看這句", &at(-9))]).await;

        let fresh = tt::restart_app(&b.env).await;
        crate::reconcile::rearm_progress(&fresh).await;
        let (status, run, delivery, verified): (String, Option<String>, String, i64) =
            sqlx::query_as("SELECT status, run_id, delivery, delivery_verified FROM turns WHERE id=?").bind(&orphan).fetch_one(app.db()).await.unwrap();
        assert_eq!((status.as_str(), run.as_deref(), delivery.as_str(), verified), ("in_flight", Some(b.run.as_str()), "ok", 1));
        assert_eq!(system_notes(&app, &b.turn).await, vec![SEND_NOW_NOTE.to_string()]);
        let stop = crate::hookrecv::HookBody {
            bot_id: b.bot.id.clone(),
            provider: "claude".into(),
            payload: json!({"hook_event_name": "Stop", "session_id": "s-sn", "prompt_id": "p-new", "transcript_path": path.to_string_lossy(),
                            "last_assistant_message": "新那一則的回覆"}),
            received_at: None,
            truncated: false,
            run_id: None,
        };
        crate::hookrecv::process(&fresh, &stop).await.unwrap();

        let on_old: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='assistant'")
            .bind(&b.turn)
            .fetch_all(app.db())
            .await
            .unwrap();
        assert!(on_old.is_empty(), "新那一則的回覆掛到了被插隊的那一筆上：{on_old:?}");
        assert_eq!(status_of(&app, &b.turn).await, "failed", "被插隊打斷");
        let answered: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages m JOIN turns t ON t.id = m.turn_id WHERE t.conversation_id=? AND m.role='assistant' AND m.content='新那一則的回覆'",
        )
        .bind(&conv)
        .fetch_one(app.db())
        .await
        .unwrap();
        assert_eq!(answered, 1, "回覆還在對話裡");
        assert_eq!(status_of(&app, &orphan).await, "completed", "回覆歸新的那一則");
    }

    /// #229 的反面：transcript 裡看不到新那一則（鍵沒生效，或還沒寫出來）——照舊收成 failed／unknown，被插隊的那一筆不動。
    /// 同一句話在它建立**之前**就出現過（使用者先前自己打過）也不算。
    #[tokio::test]
    async fn after_a_restart_a_send_now_without_proof_stays_unknown() {
        let b = busy("send-now-restart-no-proof").await;
        let app = b.env.app.clone();
        let conv = db::conversation_id(app.db(), &b.bot.id).await.unwrap();
        let orphan = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at, prompt_text)
             VALUES (?,?,NULL,'web','in_flight','pending',?,'先看這句')",
        )
        .bind(&orphan)
        .bind(&conv)
        .bind(at(-50))
        .execute(app.db())
        .await
        .unwrap();
        transcript(&b, &[prompt_line("先看這句", &at(-60_000)), prompt_line("跑一下測試", &at(-5_000))]).await;

        let fresh = tt::restart_app(&b.env).await;
        crate::reconcile::rearm_progress(&fresh).await;

        let (status, delivery): (String, String) =
            sqlx::query_as("SELECT status, delivery FROM turns WHERE id=?").bind(&orphan).fetch_one(app.db()).await.unwrap();
        assert_eq!((status.as_str(), delivery.as_str()), ("failed", "unknown"));
        assert_eq!(status_of(&app, &b.turn).await, "in_flight", "被插隊的那一筆不動");
    }

    fn end_turn_line(text: &str, at: &str) -> String {
        json!({"type": "assistant", "timestamp": at, "message": {"role": "assistant", "stop_reason": "end_turn", "content": [{"type": "text", "text": text}]}})
            .to_string()
    }

    /// 打斷的帳只在記憶體：Esc 生效但 DB 沒寫成（欠著）、Esc 待證，或使用者在 daemon 停著的時候直接在 pane 裡按了 Esc。
    /// claude 2.1.276+ 按 Esc 不送任何 hook（#223），重啟後那一筆被接回 poller、等著被 §4.3 備援當成答完了收掉；
    /// 使用者接著在 pane 裡打的下一句，它的 Stop 還會認領那一筆。transcript 裡我們那一則之後、答完之前有中斷標記＝被按停了：
    /// 重啟時就照 Esc 收，下一句的回覆成外部回合。
    #[tokio::test]
    async fn a_turn_interrupted_before_a_restart_is_closed_from_the_transcript() {
        let b = busy("esc-before-restart").await;
        let app = b.env.app.clone();
        let path = transcript(&b, &[prompt_line("跑一下測試", &at(-5_000)), esc_marker(&at(-1_000))]).await;

        let fresh = tt::restart_app(&b.env).await;
        crate::reconcile::rearm_progress(&fresh).await;
        assert_eq!(status_of(&app, &b.turn).await, "failed", "重啟前就被按停了");
        assert_eq!(system_notes(&app, &b.turn).await.len(), 1, "{:?}", system_notes(&app, &b.turn).await);

        append(&path, &[prompt_line("算了，我自己來", &at(0))]);
        let stop = crate::hookrecv::HookBody {
            bot_id: b.bot.id.clone(),
            provider: "claude".into(),
            payload: json!({"hook_event_name": "Stop", "session_id": "s-typed", "prompt_id": "p-typed", "last_assistant_message": "下一句的回覆"}),
            received_at: None,
            truncated: false,
            run_id: None,
        };
        crate::hookrecv::process(&fresh, &stop).await.unwrap();
        let on_old: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='assistant'")
            .bind(&b.turn)
            .fetch_all(app.db())
            .await
            .unwrap();
        assert!(on_old.is_empty(), "下一句的回覆掛到了被按停的那一筆上：{on_old:?}");
    }

    /// 反面：我們那一則在 daemon 停著時**答完了**（Stop 還在 spool 裡，重播在接回之後），之後使用者在 pane 裡打的另一句被按停——
    /// 那個中斷標記是另一句的，不能把我們那一則收成被按停。
    #[tokio::test]
    async fn an_interruption_after_our_turn_finished_does_not_close_it_at_a_restart() {
        let b = busy("answered-before-restart").await;
        let app = b.env.app.clone();
        transcript(
            &b,
            &[
                prompt_line("跑一下測試", &at(-9_000)),
                end_turn_line("測完了", &at(-8_000)),
                prompt_line("再幫我看 lint", &at(-3_000)),
                esc_marker(&at(-1_000)),
            ],
        )
        .await;

        let fresh = tt::restart_app(&b.env).await;
        crate::reconcile::rearm_progress(&fresh).await;
        assert_eq!(status_of(&app, &b.turn).await, "in_flight", "它是答完的，等它的 Stop（或備援）收");
    }

    /// #156：欠著的帳只在記憶體。run 已經結束（`exited`／`stopped`）、那一筆卻還 in_flight，而 daemon 在補上之前重啟了——
    /// 沒有 hook 會來（run 不在了），閒置 watchdog 也只看活著的 run。重啟時補收成 failed，寫明原因。
    #[tokio::test]
    async fn a_turn_left_in_flight_on_an_ended_run_is_closed_after_a_restart() {
        let b = busy("ended-run-turn").await;
        let app = b.env.app.clone();
        sqlx::query("UPDATE runs SET state='exited', ended_at=? WHERE id=?").bind(db::now()).bind(&b.run).execute(app.db()).await.unwrap();

        let fresh = tt::restart_app(&b.env).await;
        crate::reconcile::rearm_progress(&fresh).await;

        assert_eq!(status_of(&app, &b.turn).await, "failed");
        assert!(system_notes(&app, &b.turn).await.iter().any(|n| n.contains("已經結束")), "{:?}", system_notes(&app, &b.turn).await);
    }

    #[tokio::test]
    async fn d3b2_restart_interruption_guards_preserve_api_and_process_cli() {
        let b = busy("d3b2-api").await;
        let app = b.env.app.clone();
        sqlx::query("UPDATE bots SET kind='claude-api' WHERE id=?").bind(&b.bot.id).execute(app.db()).await.unwrap();
        sqlx::query("UPDATE runs SET state='exited',ended_at=? WHERE id=?").bind(db::now()).bind(&b.run).execute(app.db()).await.unwrap();
        let cli = tt::claude_bot(&app, &b.env.project_id, "d3b2-cli").await;
        let cli_run = tt::fake_run(&app, &cli.id).await;
        sqlx::query("UPDATE runs SET state='exited',ended_at=? WHERE id=?").bind(db::now()).bind(&cli_run).execute(app.db()).await.unwrap();
        let cli_conv = db::conversation_id(app.db(), &cli.id).await.unwrap();
        let api_conv = db::conversation_id(app.db(), &b.bot.id).await.unwrap();
        for (id,conv,run) in [("d3b2-cli-ended", &cli_conv, Some(&cli_run)), ("d3b2-api-unbound", &api_conv, None), ("d3b2-cli-unbound", &cli_conv, None)] {
            sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES(?,?,?,'web','in_flight','pending',?)").bind(id).bind(conv).bind(run).bind(db::now()).execute(app.db()).await.unwrap();
        }
        assert!(adopt_turns_of_ended_runs(&app, "9999-01-01").await);
        assert_eq!(status_of(&app, &b.turn).await, "in_flight");
        assert_eq!(status_of(&app, "d3b2-cli-ended").await, "failed");
        assert!(adopt_unbound_send_nows(&app, "9999-01-01").await);
        assert_eq!(status_of(&app, "d3b2-api-unbound").await, "in_flight");
        assert_eq!(status_of(&app, "d3b2-cli-unbound").await, "failed");
        sqlx::query("ALTER TABLE bots RENAME TO d3b2_hidden_bots").execute(app.db()).await.unwrap();
        assert!(!adopt_turns_of_ended_runs(&app, "9999-01-01").await);
        assert!(!adopt_unbound_send_nows(&app, "9999-01-01").await);
        sqlx::query("ALTER TABLE d3b2_hidden_bots RENAME TO bots").execute(app.db()).await.unwrap();
        assert_eq!(status_of(&app, &b.turn).await, "in_flight");
    }

    #[tokio::test]
    async fn d3b2_missing_bot_interruption_debt_does_not_block_cli() {
        let b = busy("d3b2-missing-owner").await;
        let app = b.env.app.clone();
        sqlx::query("UPDATE runs SET state='exited',ended_at=? WHERE id=?").bind(db::now()).bind(&b.run).execute(app.db()).await.unwrap();
        let conv = db::conversation_id(app.db(), &b.bot.id).await.unwrap();
        sqlx::query("INSERT INTO turns(id,conversation_id,origin,status,delivery,created_at) VALUES('d3b2-missing-unbound',?,'web','in_flight','pending',?)").bind(&conv).bind(db::now()).execute(app.db()).await.unwrap();
        let cli = tt::claude_bot(&app, &b.env.project_id, "d3b2-valid-cli").await;
        let run = tt::fake_run(&app, &cli.id).await;
        sqlx::query("UPDATE runs SET state='exited',ended_at=? WHERE id=?").bind(db::now()).bind(&run).execute(app.db()).await.unwrap();
        let cli_conv = db::conversation_id(app.db(), &cli.id).await.unwrap();
        for (id, run) in [("d3b2-valid-ended", Some(&run)), ("d3b2-valid-unbound", None)] {
            sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES(?,?,?,'web','in_flight','pending',?)").bind(id).bind(&cli_conv).bind(run).bind(db::now()).execute(app.db()).await.unwrap();
        }
        let mut conn = app.db().acquire().await.unwrap();
        sqlx::query("PRAGMA foreign_keys=OFF").execute(&mut *conn).await.unwrap();
        sqlx::query("DELETE FROM bots WHERE id=?").bind(&b.bot.id).execute(&mut *conn).await.unwrap();
        sqlx::query("PRAGMA foreign_keys=ON").execute(&mut *conn).await.unwrap();
        drop(conn);
        assert!(db::bot(app.db(), &b.bot.id).await.unwrap().is_none());
        assert!(!adopt_turns_of_ended_runs(&app, "9999-01-01").await);
        assert!(!adopt_unbound_send_nows(&app, "9999-01-01").await);
        assert_eq!(status_of(&app, &b.turn).await, "in_flight");
        assert_eq!(status_of(&app, "d3b2-missing-unbound").await, "in_flight");
        assert_eq!(status_of(&app, "d3b2-valid-ended").await, "failed");
        assert_eq!(status_of(&app, "d3b2-valid-unbound").await, "failed");
    }

    /// #156：DB 一直寫不進去時，上一個 run 欠著的那一筆還沒補上、新的 run 又欠一筆——後來的不能蓋掉先前的，兩筆都要補上
    /// （舊 run 已經結束，那一筆不會再有 hook，也不歸閒置 watchdog 管，帳一丟就永遠在飛）。
    #[tokio::test]
    async fn two_owed_closes_for_one_bot_are_both_settled() {
        let b = busy("two-owed").await;
        let app = b.env.app.clone();
        lose_turn_writes(&app).await;
        assert_eq!(mark_run_exited(&app, &b.run, "pane exited").await, RunExit::TurnOwed);

        let second_run = tt::fake_run(&app, &b.bot.id).await;
        let conv = db::conversation_id(app.db(), &b.bot.id).await.unwrap();
        let second = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
            .bind(&second)
            .bind(&conv)
            .bind(&second_run)
            .bind(db::now())
            .execute(app.db())
            .await
            .unwrap();
        assert_eq!(mark_run_exited(&app, &second_run, "pane exited").await, RunExit::TurnOwed);

        heal_turn_writes(&app).await;
        settle(&app, &b.bot.id).await.unwrap();
        assert_eq!(status_of(&app, &b.turn).await, "failed", "先欠的那一筆沒被蓋掉");
        assert_eq!(status_of(&app, &second).await, "failed");
        assert!(pending(&b.bot.id).is_empty());
    }

    /// #147 驗收五：寫失敗之後 hook 一直沒來（claude 對 Esc 不一定送 `StopFailure`）——下一則 prompt 不能
    /// 被那筆幽靈 in-flight 擋成「a turn is already in flight」直到五分鐘後的 watchdog。
    #[tokio::test]
    async fn a_lost_interrupt_write_does_not_wedge_the_next_prompt() {
        let b = busy("esc-no-wedge").await;
        let app = b.env.app.clone();
        lose_turn_writes(&app).await;
        interrupt_bot(&app, &b.bot.id).await.expect_err("寫不進去");
        heal_turn_writes(&app).await;

        match prompt(&app, &b.bot.id, "下一句", "after-esc").await {
            Ok(_) => {}
            Err(LcError::Conflict(body)) => {
                assert_ne!(body["reason"], "a turn is already in flight", "被已經中斷的回合擋住了：{body}");
            }
            Err(e) => panic!("{e:?}"),
        }
        assert_eq!(status_of(&app, &b.turn).await, "failed", "欠著的收尾在下一則 prompt 之前補上");
    }
}
