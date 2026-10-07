//! `ask_answers` runner 與 hook 後置處理。

use crate::ask_answers::{carried, from_local_transcript, record};
use crate::db;
use crate::state::App;
use serde_json::Value;
use std::sync::Arc;

/// 回合結束（`Stop`／`StopFailure`）的 hook 處理完之後補讀：這一回合內已經答完（或取消）的提問，一筆都不漏。
///
/// 放在 `hookrecv::process` 收尾而不是 hook 處理中間：終端打字開的外部回合要等 Stop 才建立，這時候「最新的回合」才是對的那一個。
/// 遠端 bot 用 `hook.sh` 帶來的 `agm_asks`；本機 bot 讀自己這台的 transcript（payload 的路徑，沒有就用 run 記的）。
/// 記不成不能害這則 hook 失敗：回合收尾比對話裡多一則紀錄重要，所以只記 warn（下一則 hook 或重送會再補）。
pub(crate) async fn after_turn_end(app: &Arc<App>, body: &crate::hook_body::HookBody) {
    let p = &body.payload;
    if !body.provider.eq_ignore_ascii_case("claude") || !matches!(p.get("hook_event_name").and_then(Value::as_str), Some("Stop" | "StopFailure")) {
        return;
    }
    let Ok(Some(bot)) = db::bot(&app.db, &body.bot_id).await else { return };
    if bot.deleted_at.is_some() || bot.kind != "claude" {
        return;
    }
    let (Ok(conv), Ok(run)) = (db::conversation_id(&app.db, &bot.id).await, db::active_run(&app.db, &bot.id).await) else { return };
    let mut records = carried(p);
    if records.is_empty() {
        let local = db::bot_host(&app.db, &bot.id).await.is_ok_and(|h| h == crate::config::LOCAL_HOST);
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
    if let Err(e) = record(app, &bot.id, &conv, records).await {
        tracing::warn!(bot = %bot.name, error = %e, "could not record the AskUserQuestion answers");
    }
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
        after_turn_end(&app, &body).await;

        let imported: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id=? AND content LIKE '%FOREIGN-ANSWER-3187%'")
            .bind(&conv)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(imported, 0, "外部 JSONL 的答案不能混進本機 bot 對話");
        let still_here: Option<String> = sqlx::query_scalar("SELECT id FROM turns WHERE id=?").bind(&turn).fetch_optional(&app.db).await.unwrap();
        assert_eq!(still_here.as_deref(), Some(turn.as_str()));
    }
}
