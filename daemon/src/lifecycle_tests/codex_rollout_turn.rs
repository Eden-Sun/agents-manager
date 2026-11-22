#![allow(unused_imports)]
use crate::lifecycle::codex_rollout::turn_boundary::*;
use crate::lifecycle::codex_rollout::*;

mod ownership_tests {
    use crate::{db, testing as tt};
    use serde_json::json;

    /// Turn replacement uses the existing expected_turn/CAS contract: an old
    /// armed callback cannot assign a reply to a superseded AGM turn.
    #[tokio::test]
    async fn a_stale_fallback_callback_cannot_duplicate_a_reply_after_turn_replacement() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "native-owner-1994").await;
        sqlx::query("UPDATE bots SET kind='codex' WHERE id=?")
            .bind(&bot.id)
            .execute(&app.db)
            .await
            .unwrap();
        let run = tt::fake_run(&app, &bot.id).await;
        let session = "01a18888-1994-7000-8000-000000000001";
        let home = env.dir.join("codex");
        let logs = home.join("sessions/2026/10/25");
        std::fs::create_dir_all(&logs).unwrap();
        sqlx::query("UPDATE bots SET env_json=? WHERE id=?")
            .bind(json!({"CODEX_HOME":home}).to_string())
            .bind(&bot.id)
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("UPDATE runs SET native_session_id=? WHERE id=?")
            .bind(session)
            .bind(&run)
            .execute(&app.db)
            .await
            .unwrap();
        let old = crate::lifecycle::run_state::a_turn(&app, &bot.id, Some(&run), "in_flight").await;
        sqlx::query("UPDATE turns SET prompt_text='original',created_at='2026-10-25T10:00:00Z',delivered_at='2026-10-25T10:01:01Z' WHERE id=?").bind(&old).execute(&app.db).await.unwrap();
        let new = crate::lifecycle::run_state::a_turn(&app, &bot.id, None, "in_flight").await;
        crate::lifecycle::interruption::send_now_interrupted(&app, &bot.id, &run, &old, &new)
            .await
            .unwrap();
        sqlx::query("UPDATE turns SET prompt_text='steered',delivery='ok',created_at='2026-10-25T10:01:02Z',delivered_at='2026-10-25T10:01:02Z' WHERE id=?").bind(&new).execute(&app.db).await.unwrap();
        let inflight: Vec<String> =
            sqlx::query_scalar("SELECT id FROM turns WHERE run_id=? AND status='in_flight'")
                .bind(&run)
                .fetch_all(&app.db)
                .await
                .unwrap();
        assert_eq!(
            inflight,
            vec![new.clone()],
            "new turn must be the sole in-flight owner"
        );
        let event = |t: &str, kind: &str| {
            json!({"type":"event_msg","timestamp":format!("2026-10-25T10:{t}Z"),"payload":{"type":kind,"turn_id":"native-one","last_agent_message":"one complete final reply"}}).to_string()
        };
        let user = |t: &str, text: &str| {
            json!({"type":"response_item","timestamp":format!("2026-10-25T10:{t}Z"),"payload":{"type":"message","role":"user","content":[{"type":"input_text","text":text}]}}).to_string()
        };
        std::fs::write(
            logs.join(format!("rollout-2026-10-25T10-01-00-{session}.jsonl")),
            [
                event("01:00", "task_started"),
                user("01:01", "original"),
                user("01:02", "steered"),
                event("01:03", "task_complete"),
            ]
            .join("\n"),
        )
        .unwrap();
        env.herdr
            .set_screen(&format!("pane-{}", bot.id), "› steered\n• idle\n");
        assert!(
            !crate::lifecycle::poller::test_seams::try_fallback(&app, &run, Some(&old))
                .await
                .unwrap()
        );
        assert!(
            crate::lifecycle::poller::test_seams::try_fallback(&app, &run, Some(&new))
                .await
                .unwrap()
        );
        assert!(
            !crate::lifecycle::poller::test_seams::try_fallback(&app, &run, Some(&old))
                .await
                .unwrap()
        );
        let rows: Vec<(String,String,String,i64)> = sqlx::query_as("SELECT turn_id,content,source,incomplete FROM messages WHERE turn_id IN (?,?) AND role='assistant'").bind(&old).bind(&new).fetch_all(&app.db).await.unwrap();
        assert_eq!(
            rows,
            vec![(
                new,
                "one complete final reply".into(),
                "transcript".into(),
                0
            )]
        );
        let old_state: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?")
            .bind(&old)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(old_state, "failed");
    }
}

mod codex_steer_tests {
    use crate::{db, testing as tt};
    use serde_json::json;

    #[tokio::test]
    async fn real_codex_steer_keeps_one_agm_turn_and_one_complete_reply() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "codex-steer-owner-1994").await;
        sqlx::query("UPDATE bots SET kind='codex' WHERE id=?")
            .bind(&bot.id)
            .execute(&app.db)
            .await
            .unwrap();
        let run = tt::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE runs SET agent_status='working',codex_instant_interrupt=1,native_session_id=NULL,transcript_path=NULL WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
        crate::codex_update::remember_running(&run, "│ >_ OpenAI Codex (v0.164.0)   │", None);
        app.cfg
            .update(|c| {
                c.codex.instant_interrupt = true;
                Ok(())
            })
            .await
            .unwrap();
        env.herdr.live_pane(
            &format!("pane-{}", bot.id),
            tt::LivePane {
                width: Some(120),
                codex: true,
                ..Default::default()
            },
        );
        let turn =
            crate::lifecycle::run_state::a_turn(&app, &bot.id, Some(&run), "in_flight").await;
        sqlx::query("UPDATE turns SET prompt_text='original',created_at='2026-10-25T10:00:00Z',delivered_at='2026-10-25T10:01:01Z' WHERE id=?").bind(&turn).execute(&app.db).await.unwrap();
        let steered = crate::lifecycle::prompt::prompt_send_now(
            &app,
            &bot.id,
            "supplement",
            "native-owner-1994",
            &[],
            None,
        )
        .await
        .unwrap();
        assert_eq!(steered.turn_id, turn);
        assert_eq!(steered.send_now, Some("steered"));
        let supplement: (String, String) =
            sqlx::query_as("SELECT turn_id,sent_via FROM messages WHERE id=?")
                .bind(&steered.message_id)
                .fetch_one(&app.db)
                .await
                .unwrap();
        assert_eq!(supplement, (turn.clone(), "supplement".into()));
        let session = "01a18888-1994-7000-8000-000000000002";
        let home = env.dir.join("codex");
        let logs = home.join("sessions/2026/10/25");
        std::fs::create_dir_all(&logs).unwrap();
        sqlx::query("UPDATE bots SET env_json=? WHERE id=?")
            .bind(json!({"CODEX_HOME":home}).to_string())
            .bind(&bot.id)
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("UPDATE runs SET native_session_id=? WHERE id=?")
            .bind(session)
            .bind(&run)
            .execute(&app.db)
            .await
            .unwrap();
        let event = |t: &str, kind: &str| {
            json!({"type":"event_msg","timestamp":format!("2026-10-25T10:{t}Z"),"payload":{"type":kind,"turn_id":"native-steer","last_agent_message":"one final after steering"}}).to_string()
        };
        let user = |t: &str, text: &str| {
            json!({"type":"response_item","timestamp":format!("2026-10-25T10:{t}Z"),"payload":{"type":"message","role":"user","content":[{"type":"input_text","text":text}]}}).to_string()
        };
        std::fs::write(
            logs.join(format!("rollout-2026-10-25T10-01-00-{session}.jsonl")),
            [
                event("01:00", "task_started"),
                user("01:01", "original"),
                user("01:02", "supplement"),
                event("01:03", "task_complete"),
            ]
            .join("\n"),
        )
        .unwrap();
        env.herdr
            .set_screen(&format!("pane-{}", bot.id), "› supplement\n• idle\n");
        assert!(
            crate::lifecycle::poller::test_seams::try_fallback(&app, &run, Some(&turn))
                .await
                .unwrap()
        );
        assert!(
            !crate::lifecycle::poller::test_seams::try_fallback(&app, &run, Some(&turn))
                .await
                .unwrap()
        );
        let replies: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT content,source,incomplete FROM messages WHERE turn_id=? AND role='assistant'",
        )
        .bind(&turn)
        .fetch_all(&app.db)
        .await
        .unwrap();
        assert_eq!(
            replies,
            vec![("one final after steering".into(), "transcript".into(), 0)]
        );
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE run_id=?")
            .bind(&run)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(count, 1, "Codex steer must not create a second AGM turn");
    }
}
