//! `ask_answers` runner 與 hook 後置處理。

use crate::ask_answers::{carried, from_local_transcript};
use crate::db;
use crate::events::ports::{HandoffRepo, ProviderPort};
use crate::state::App;
use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;

/// 回合結束（`Stop`／`StopFailure`）的 hook 處理完之後補讀：這一回合內已經答完（或取消）的提問，一筆都不漏。
///
/// 放在 `hookrecv::process` 收尾：外部回合也要等 Stop 建立後，才能依核心接受的 turn id 補記。
/// 遠端 bot 用 `hook.sh` 帶來的 `agm_asks`；本機 bot 讀自己這台的 transcript（payload 的路徑，沒有就用 run 記的）。
/// 寫入失敗要回報給 durable inbox，讓它重試；`source_event_id` 會把已完成的回合交易去重。
pub(crate) async fn after_turn_end(app: &Arc<App>, body: &crate::hook_body::HookBody, run_id: Option<&str>, turn_id: &str) -> Result<()> {
    let p = &body.payload;
    if !body.provider.eq_ignore_ascii_case("claude") || !matches!(p.get("hook_event_name").and_then(Value::as_str), Some("Stop" | "StopFailure")) {
        return Ok(());
    }
    let Some(bot) = db::bot(&app.db, &body.bot_id).await? else { return Ok(()) };
    if bot.deleted_at.is_some() || bot.kind != "claude" {
        return Ok(());
    }
    // Recheck the admission boundary after the core hook transaction and bind recording to its turn.
    if app.db.bot_handed_off_to(&bot.id).await?.is_some() {
        return Ok(());
    }
    let run = if let Some(run_id) = run_id {
        let Some(stored_run) = db::run(&app.db, run_id).await? else { return Ok(()) };
        if stored_run.bot_id != bot.id {
            return Ok(());
        }
        let active = db::active_run(&app.db, &bot.id).await?;
        if active.as_ref().is_some_and(|r| r.id != run_id) {
            return Ok(());
        }
        let host_generation = db::run_host_generation(&app.db, run_id).await?;
        if let Some(was) = host_generation {
            if app.admitted_host_fence(&bot).await.is_some_and(|f| f.generation() as i64 != was) {
                return Ok(());
            }
        }
        Some(stored_run)
    } else if db::active_run(&app.db, &bot.id).await?.is_some() {
        return Ok(());
    } else {
        None
    };
    let conv = db::conversation_id(&app.db, &bot.id).await?;
    let mut records = carried(p);
    if records.is_empty() {
        let local = db::bot_host(&app.db, &bot.id).await? == crate::config::LOCAL_HOST;
        let path = ["transcript_path", "transcriptPath"]
            .iter()
            .find_map(|k| p.get(*k).and_then(Value::as_str).map(str::to_owned))
            .or_else(|| run.as_ref().and_then(|r| r.transcript_path.clone()))
            .filter(|p| !p.trim().is_empty());
        if let (true, Some(path)) = (local, path) {
            if crate::app_ports_p5::local_transcript_allowed(app, &bot, &path).await {
                records = from_local_transcript(&path).await;
            }
        }
    }
    crate::ask_answers::record_for_turn(app, &bot.id, &conv, run_id, turn_id, records).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ask(id: &str, questions: Value) -> String {
        json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": id, "name": "AskUserQuestion", "input": {"questions": questions}}]}}).to_string()
    }

    fn qs() -> Value {
        json!([
            {"question": "拋單怎麼處理？", "header": "拋單倉庫", "multiSelect": false, "options": [{"label": "保留拋單"}, {"label": "拿掉拋單"}]},
            {"question": "go API 放哪？", "header": "go API", "multiSelect": false, "options": [{"label": "stock-server"}]},
        ])
    }

    fn answered(id: &str, at: &str, answers: Value) -> String {
        json!({"type": "user", "timestamp": at, "message": {"content": [{"type": "tool_result", "tool_use_id": id, "content": "Your questions have been answered: …"}]},
               "toolUseResult": {"questions": qs(), "answers": answers, "annotations": {}}})
        .to_string()
    }

    /// Path guard test for transcript path injection.
    #[tokio::test]
    async fn foreign_transcript_path_is_rejected_on_stop() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "ask-path-guard").await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let run_id = crate::testing::fake_run(&app, &bot.id).await;
        let turn = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?, 'web', 'completed', 'ok', ?)")
            .bind(&turn)
            .bind(&conv)
            .bind(&run_id)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();

        let scratch = crate::testing::track(std::env::temp_dir().join(format!("am-ask-path-{}", db::ulid())));
        std::fs::create_dir_all(&scratch).unwrap();
        let outside = scratch.join("foreign.jsonl");
        let log = [ask("foreign-tool-id", qs()), answered("foreign-tool-id", &db::now(), json!({"拋單怎麼處理？": "FOREIGN-ANSWER-3187"}))].join("\n");
        std::fs::write(&outside, log).unwrap();

        let body = crate::hookrecv::HookBody {
            bot_id: bot.id.clone(),
            provider: "claude".into(),
            payload: json!({"hook_event_name":"Stop", "transcript_path":outside.to_string_lossy()}),
            received_at: None,
            truncated: false,
            run_id: None,
        };
        after_turn_end(&app, &body, Some(&run_id), &turn).await.unwrap();

        let imported: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id=? AND content LIKE '%FOREIGN-ANSWER-3187%'")
            .bind(&conv)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(imported, 0, "外部 JSONL 的答案不能混進本機 bot 對話");
        let still_here: Option<String> = sqlx::query_scalar("SELECT id FROM turns WHERE id=?").bind(&turn).fetch_optional(&app.db).await.unwrap();
        assert_eq!(still_here.as_deref(), Some(turn.as_str()));
    }

    #[tokio::test]
    async fn stale_stop_does_not_attach_carried_answers_to_the_current_run() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "ask-stale-stop").await;
        let old_run = crate::testing::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE runs SET state='stopped' WHERE id=?").bind(&old_run).execute(&app.db).await.unwrap();
        let current_run = crate::testing::fake_run(&app, &bot.id).await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let turn = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?, 'web', 'completed', 'ok', ?)")
            .bind(&turn)
            .bind(&conv)
            .bind(&current_run)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let body = crate::hookrecv::HookBody {
            bot_id: bot.id.clone(),
            provider: "claude".into(),
            payload: json!({
                "hook_event_name": "Stop",
                "agm_asks": [{"id":"old-question", "at":null, "items":[{"question":"old?","answer":"STALE-RUN-ANSWER-1317"}] }]
            }),
            received_at: None,
            truncated: false,
            run_id: Some(old_run),
        };

        crate::hookrecv::process_for(&app, &body, None).await.unwrap();

        let imported: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id=? AND content LIKE '%STALE-RUN-ANSWER-1317%'")
            .bind(&conv)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(imported, 0, "stale Stop answers must not be attached to the current run's turn");
        let still_here: Option<String> = sqlx::query_scalar("SELECT id FROM turns WHERE id=?").bind(&turn).fetch_optional(&app.db).await.unwrap();
        assert_eq!(still_here.as_deref(), Some(turn.as_str()));
    }

    #[tokio::test]
    async fn inbox_retries_when_answer_persistence_fails_without_replaying_the_turn() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "ask-retry-stop").await;
        let run_id = crate::testing::fake_run(&app, &bot.id).await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let turn = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?, 'web', 'in_flight', 'ok', ?)")
            .bind(&turn)
            .bind(&conv)
            .bind(&run_id)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let body = crate::hookrecv::HookBody {
            bot_id: bot.id.clone(),
            provider: "claude".into(),
            payload: json!({
                "hook_event_name": "Stop",
                "session_id": "session-1319",
                "prompt_id": "turn-1319",
                "last_assistant_message": "done",
                "agm_asks": [{"id":"question-1319", "at":null, "items":[{"question":"retry?","answer":"ANSWER-1319"}] }]
            }),
            received_at: Some(db::now()),
            truncated: false,
            run_id: Some(run_id.clone()),
        };
        assert!(crate::hook_inbox::accept(&app.db, &body, crate::hook_inbox::Source::Http).await.unwrap().is_new());
        sqlx::query(
            "CREATE TRIGGER fail_ask_answer BEFORE INSERT ON messages
             WHEN NEW.content LIKE '%ANSWER-1319%'
             BEGIN SELECT RAISE(ABORT, 'injected answer write failure'); END",
        )
        .execute(&app.db)
        .await
        .unwrap();

        crate::runners::hook_inbox::drain_once(&app).await.unwrap();
        let processed: Option<String> = sqlx::query_scalar("SELECT processed_at FROM hook_events WHERE bot_id=?")
            .bind(&bot.id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert!(processed.is_none(), "failed answer write leaves the durable event pending");
        let closed: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(&turn).fetch_one(&app.db).await.unwrap();
        assert_eq!(closed, "completed", "primary Stop effects remain committed");

        sqlx::query("DROP TRIGGER fail_ask_answer").execute(&app.db).await.unwrap();
        sqlx::query("UPDATE hook_events SET next_attempt_at=NULL WHERE bot_id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        crate::runners::hook_inbox::drain_once(&app).await.unwrap();

        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=?").bind(&conv).fetch_one(&app.db).await.unwrap();
        assert_eq!(turns, 1, "retry does not replay the accepted main Stop into another turn");
        let answer: Option<String> = sqlx::query_scalar("SELECT turn_id FROM messages WHERE conversation_id=? AND content LIKE '%ANSWER-1319%'")
            .bind(&conv)
            .fetch_optional(&app.db)
            .await
            .unwrap();
        assert_eq!(answer.as_deref(), Some(turn.as_str()), "retry records the answer once on the original turn");
        let processed: Option<String> = sqlx::query_scalar("SELECT processed_at FROM hook_events WHERE bot_id=?")
            .bind(&bot.id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert!(processed.is_some(), "recovered event is marked processed");
    }
}
