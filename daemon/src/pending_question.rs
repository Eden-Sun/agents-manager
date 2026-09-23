//! claude 停在 `AskUserQuestion` 時，題目本身從 transcript 讀，不靠畫面（2026-09-23 使用者：「為什麼看不見題目」）。
//!
//! pane 太矮時 claude 會把自己的選單裁掉：題目那一行根本沒畫出來，選項也只剩捲動中的一段（`↓ 3.`、跳到 `5.`）。
//! 畫面上沒有的東西，網頁怎麼 parse 都拿不到；但 transcript 裡那個 `tool_use` 有完整的題目與選項。
//! 這裡找「最後一個還沒被回答的 `AskUserQuestion`」：有 `tool_use`、之後沒有對應 `tool_use_id` 的 `tool_result`。

use crate::db;
use crate::lifecycle::LcError;
use crate::state::App;
use axum::extract::{Path, State};
use axum::Json;
use serde_json::{json, Value};
use std::sync::Arc;

/// 讀 transcript 最後這麼多位元組就夠：題目是最後幾筆訊息之一，不需要整份（長 session 的檔動輒幾十 MB）。
const TAIL_BYTES: u64 = 512 * 1024;

/// 最後一個還沒被回答的 `AskUserQuestion` 的 `input`（含 `questions`），沒有就 `None`。
///
/// 一行一筆 JSON；第一行可能因為從中間讀而被切壞，解析不了就跳過。
pub(crate) fn pending_ask(log: &str) -> Option<Value> {
    let mut pending: Option<(String, Value)> = None;
    for line in log.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else { continue };
        let Some(content) = entry.pointer("/message/content").and_then(Value::as_array) else { continue };
        for block in content {
            match block.get("type").and_then(Value::as_str) {
                Some("tool_use") if block.get("name").and_then(Value::as_str) == Some("AskUserQuestion") => {
                    let id = block.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
                    pending = Some((id, block.get("input").cloned().unwrap_or(Value::Null)));
                }
                // 答了（或被取消，取消一樣會回一個 tool_result）：這一題不再等人。
                Some("tool_result") => {
                    let answered = block.get("tool_use_id").and_then(Value::as_str);
                    if pending.as_ref().is_some_and(|(id, _)| Some(id.as_str()) == answered) {
                        pending = None;
                    }
                }
                _ => {}
            }
        }
    }
    pending.map(|(_, input)| input).filter(|input| input.get("questions").and_then(Value::as_array).is_some_and(|q| !q.is_empty()))
}

fn read_tail(path: &std::path::Path, max: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(max))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

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
    let input = tokio::task::spawn_blocking(move || read_tail(std::path::Path::new(&path), TAIL_BYTES).and_then(|log| pending_ask(&log)))
        .await
        .ok()
        .flatten();
    Ok(Json(json!({"questions": input.and_then(|i| i.get("questions").cloned())})))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(id: &str, question: &str) -> String {
        json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": id, "name": "AskUserQuestion",
            "input": {"questions": [{"question": question, "header": "連線方式", "multiSelect": false,
                "options": [{"label": "內網 http (Recommended)", "description": "流量留在 VPC"}, {"label": "公網 https", "description": "拿掉白名單"}]}]}}]}})
        .to_string()
    }

    fn answer(id: &str) -> String {
        json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": id, "content": "User answered"}]}}).to_string()
    }

    /// 2026-09-23 console-fetures-fork：pane 只有 14 行，畫面上沒有題目；transcript 裡有。
    #[test]
    fn an_unanswered_question_is_read_from_the_transcript() {
        let log = ["{broken first line cut by the tail read".to_string(), ask("t1", "prod console 連 prod RPA 要走哪條路？")].join("\n");
        let got = pending_ask(&log).expect("有一題在等");
        assert_eq!(got["questions"][0]["question"], "prod console 連 prod RPA 要走哪條路？");
        assert_eq!(got["questions"][0]["options"][1]["label"], "公網 https");
    }

    /// 答過（或取消）就不再是在等的題目；後面又問一題，回的是新的那題。
    #[test]
    fn an_answered_question_is_not_pending() {
        assert_eq!(pending_ask(&[ask("t1", "舊題"), answer("t1")].join("\n")), None);
        let got = pending_ask(&[ask("t1", "舊題"), answer("t1"), ask("t2", "新題")].join("\n")).unwrap();
        assert_eq!(got["questions"][0]["question"], "新題");
    }

    /// 別的工具的 tool_result 不算回答這一題。
    #[test]
    fn a_result_for_another_tool_does_not_answer_the_question() {
        let got = pending_ask(&[ask("t1", "還在等"), answer("other-tool")].join("\n"));
        assert!(got.is_some());
    }

    /// 沒有任何提問、或 input 壞掉（沒有 questions）：None，網頁照舊用畫面。
    #[test]
    fn nothing_to_show_without_a_real_question() {
        assert_eq!(pending_ask(""), None);
        let bad = json!({"message": {"content": [{"type": "tool_use", "id": "t1", "name": "AskUserQuestion", "input": {}}]}}).to_string();
        assert_eq!(pending_ask(&bad), None);
    }
}
