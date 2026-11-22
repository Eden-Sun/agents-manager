#![allow(unused_imports)]
mod tests {
    use crate::lifecycle::api_error_nudge::*;
    use crate::lifecycle::*;
    use crate::lifecycle_tests::prelude::*;
    use crate::capabilities::Db;
    use crate::testing::{claude_bot, env, Env};

    const ERR: &str = "API Error: Connection lost mid-response. Please try again.";

    /// 一個 running／idle 的 run，釘著 `err`，最後一回合是 `crid`（failed）。回 (run, turn)。
    async fn cut(e: &Env, bot_id: &str, err: &str, crid: Option<&str>) -> (String, String) {
        let (run, turn, conv) = (db::ulid(), db::ulid(), db::conversation_id(e.app.db(), bot_id).await.unwrap());
        sqlx::query("INSERT INTO runs (id, bot_id, state, agent_status, started_at, turn_error) VALUES (?,?,'running','idle','2026-11-04T00:00:00Z',?)")
            .bind(&run)
            .bind(bot_id)
            .bind(err)
            .execute(e.app.db())
            .await
            .unwrap();
        add_turn(e, &conv, &run, &turn, "failed", crid).await;
        (run, turn)
    }

    async fn add_turn(e: &Env, conv: &str, run: &str, id: &str, status: &str, crid: Option<&str>) {
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, client_request_id, created_at) VALUES (?,?,?,'web',?,'ok',?,?)")
            .bind(id)
            .bind(conv)
            .bind(run)
            .bind(status)
            .bind(crid)
            .bind(db::now())
            .execute(e.app.db())
            .await
            .unwrap();
    }

    async fn queued(e: &Env, bot_id: &str) -> Vec<(String, Option<String>)> {
        let conv = db::conversation_id(e.app.db(), bot_id).await.unwrap();
        sqlx::query_as(
            "SELECT m.content, m.relay_from FROM turns t JOIN messages m ON m.turn_id = t.id WHERE t.conversation_id = ? AND t.status = 'queued'",
        )
        .bind(&conv)
        .fetch_all(e.app.db())
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn a_transient_cut_is_nudged_once_with_the_daemon_as_sender() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "ae-ok").await;
        let (run, turn) = cut(&e, &bot.id, ERR, None).await;
        arm(&e.app, &bot.id, &run).await;
        assert!(check(&e.app, &bot.id).await);
        let q = queued(&e, &bot.id).await;
        assert_eq!(q.len(), 1);
        assert!(q[0].0.contains("API 斷線截斷") && q[0].0.contains("Connection lost"), "{}", q[0].0);
        assert_eq!(q[0].1.as_deref(), Some("daemon"));
        let crid: String = sqlx::query_scalar("SELECT client_request_id FROM turns WHERE status='queued'")
            .fetch_one(e.app.db())
            .await
            .unwrap();
        assert_eq!(crid, format!("{CRID_PREFIX}{turn}"));
        arm(&e.app, &bot.id, &run).await;
        assert!(!check(&e.app, &bot.id).await);
    }

    #[tokio::test]
    async fn non_transient_errors_are_not_armed() {
        let e = env().await;
        for (i, err) in ["API Error: 400 invalid_request_error", "You've reached your Fable limit. Run /usage-credits", "API Error: 403 Forbidden"].iter().enumerate() {
            let bot = claude_bot(&e.app, &e.project_id, &format!("ae-no-{i}")).await;
            let (run, _) = cut(&e, &bot.id, err, None).await;
            arm(&e.app, &bot.id, &run).await;
            assert!(!check(&e.app, &bot.id).await, "{err}");
            assert!(queued(&e, &bot.id).await.is_empty());
        }
    }

    #[tokio::test]
    async fn a_cut_nudge_is_not_nudged_again() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "ae-loop").await;
        let (run, _) = cut(&e, &bot.id, ERR, Some("api-error-nudge:prev")).await;
        arm(&e.app, &bot.id, &run).await;
        assert!(!check(&e.app, &bot.id).await);
        assert!(queued(&e, &bot.id).await.is_empty());
    }

    #[tokio::test]
    async fn activity_in_the_window_cancels() {
        let e = env().await;
        for case in ["working", "blocked", "new_turn", "queued", "in_flight", "interrupt", "cleared", "other_run"] {
            let bot = claude_bot(&e.app, &e.project_id, &format!("ae-{case}")).await;
            let (run, _) = cut(&e, &bot.id, ERR, None).await;
            arm(&e.app, &bot.id, &run).await;
            let conv = db::conversation_id(e.app.db(), &bot.id).await.unwrap();
            match case {
                "working" | "blocked" => {
                    sqlx::query("UPDATE runs SET agent_status=? WHERE id=?").bind(case).bind(&run).execute(e.app.db()).await.unwrap();
                }
                "new_turn" => add_turn(&e, &conv, &run, &db::ulid(), "completed", None).await,
                "queued" => add_turn(&e, &conv, &run, &db::ulid(), "queued", None).await,
                "in_flight" => add_turn(&e, &conv, &run, &db::ulid(), "in_flight", None).await,
                "interrupt" => crate::lifecycle::interrupt_grace::note_user_interrupt(&bot.id),
                "cleared" => {
                    sqlx::query("UPDATE runs SET turn_error=NULL WHERE id=?").bind(&run).execute(e.app.db()).await.unwrap();
                }
                _ => {
                    sqlx::query("UPDATE runs SET state='exited' WHERE id=?").bind(&run).execute(e.app.db()).await.unwrap();
                }
            }
            let before = queued(&e, &bot.id).await.len();
            assert!(!check(&e.app, &bot.id).await, "{case}");
            assert_eq!(queued(&e, &bot.id).await.len(), before, "{case}");
        }
    }
}
