//! `pending_question` runner 與 API 處理器。

use crate::db;
use crate::lc_error::LcError;
use crate::pending_question::{pending_ask, read_tail, TAIL_BYTES};
use crate::state::App;
use axum::extract::{Path, State};
use axum::Json;
use serde_json::{json, Value};
use std::sync::Arc;

/// `GET /api/bots/{id}/pending-question`：`200 {"questions": [...] | null}`。
///
/// 只有本機的 claude bot 讀得到（transcript 在這台）；遠端、非 claude、沒有 transcript、沒在等題目都回 `null`，
/// 不是錯誤——網頁拿不到就照舊用畫面。bot 或 run 不存在 404。
pub async fn get_pending_question(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Json<Value>, LcError> {
    let up = |e: anyhow::Error| LcError::Upstream(e.to_string());
    let bot = db::bot(&app.db, &id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let run = db::active_run(&app.db, &id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("run".into()))?;
    if bot.kind != "claude" || db::bot_host(&app.db, &id).await.map_err(up)? != crate::config::LOCAL_HOST {
        return Ok(Json(json!({"questions": null})));
    }
    let Some(path) = run.transcript_path.clone().filter(|p| !p.trim().is_empty()) else {
        return Ok(Json(json!({"questions": null})));
    };
    if !crate::app_ports_p5::local_transcript_allowed(&app, &bot, &path).await {
        return Ok(Json(json!({"questions": null})));
    }
    let input = tokio::task::spawn_blocking(move || read_tail(std::path::Path::new(&path), TAIL_BYTES).and_then(|log| pending_ask(&log)))
        .await
        .ok()
        .flatten();
    Ok(Json(json!({"questions": input.and_then(|i| i.get("questions").cloned())})))
}

#[cfg(test)]
mod path_guard_tests {
    use super::*;

    /// The read endpoint must reject a legacy or poisoned DB path outside this bot's projects
    /// tree, even when that path points to a valid JSONL file on the daemon host.
    #[tokio::test]
    async fn a_pending_question_is_not_read_from_outside_the_bots_projects_dir() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "pending-path-guard").await;
        let run_id = crate::testing::fake_run(&app, &bot.id).await;
        let scratch = crate::testing::track(std::env::temp_dir().join(format!("am-pending-path-{}", db::ulid())));
        std::fs::create_dir_all(&scratch).unwrap();
        let outside = scratch.join("foreign.jsonl");
        std::fs::write(&outside, json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"foreign-question","name":"AskUserQuestion","input":{"questions":[{"question":"FOREIGN-QUESTION-7741","options":[{"label":"secret"}]}]}}]}}).to_string()).unwrap();
        sqlx::query("UPDATE runs SET transcript_path=? WHERE id=?")
            .bind(outside.to_string_lossy())
            .bind(&run_id)
            .execute(&app.db)
            .await
            .unwrap();

        let Json(body) = get_pending_question(State(app), Path(bot.id)).await.unwrap();
        assert_eq!(body["questions"], Value::Null, "未驗證的舊路徑不能讓任意本機 JSONL 內容出現在 API 回應");
    }
}
