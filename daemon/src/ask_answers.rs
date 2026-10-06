//! claude 用 `AskUserQuestion` 問使用者、使用者答完之後，把「問了什麼、答了什麼」記進那一回合的對話
//! （2026-10-02 使用者：wits-pro 問了三題、答完，網頁對話窗裡完全看不到題目跟答案，「對話不完整」）。
//!
//! ## 資料來源：transcript 的 `tool_result.toolUseResult`，PostToolUse 只是更早的一條路
//!
//! * **transcript**（主要）：claude 自己寫的紀錄，答完那一刻就在。`toolUseResult` 是結構化的
//!   （`questions`、`answers: {題目: 答案}`、`annotations`），自訂文字照原文在裡面；取消／Esc 另有固定長相
//!   （`The user did not answer the questions.`、`The user doesn't want to proceed…`）。網頁答的、終端答的、
//!   舊版 claude 的 `The user answered:` 都是同一個結構，不用認文字。**不需要改任何 bot 的設定**：已經在跑的
//!   session 也補得到。本機 daemon 回合結束時自己讀；遠端 transcript 在那台機器上，由 `hook.sh` 在 Stop 前讀出、
//!   放進 payload 的 `agm_asks`（跟 `agm_user_text` 同一套）。
//! * **PostToolUse**（`matcher: "AskUserQuestion"`，新啟動的 bot 才有）：被使用者中斷的回合不會有 Stop，
//!   只有這條路當下就留得下來。取消時 claude 不觸發它，payload 形狀也沒有完整的契約，所以**只在認得出至少一個答案時**
//!   才記；認不出就丟給回合結束的 transcript 補讀，絕不先記一筆「沒有回答」把後面正確的那筆擋掉。
//!
//! ## 冪等
//!
//! 訊息 id 是 `ask:<conversation>:<tool_use_id>`，`INSERT OR IGNORE`：hook 重送、spool replay、Stop 補讀、
//! PostToolUse 與補讀同時到，最後都只有一列。`tool_use_id` 要帶 conversation：fork／resume 會把整份 transcript 帶過去。
//!
//! ## 長相
//!
//! 一則 `role=system`、`source=system` 的訊息（`messages.source` 有 CHECK，新增來源要重建整張表；
//! 這裡不值得），`content` 是 JSON：`{"type":"ask_answers","tool_use_id":…,"answered":bool,"items":[{header,question,answer,notes}]}`。
//! 網頁認 `type`，畫成「Claude 問／你答」；它不是使用者打的 prompt，也不進任何送出或排隊的邏輯。
//! `created_at` 用答完的時間（transcript 的時間戳），排在回合中間而不是 Stop 才補進去的那一刻。

use crate::db;
use crate::state::App;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;

/// 訊息 id 前綴。要在別處排除這類訊息（例如「這回合有沒有系統說明」）時用它認。
pub(crate) const ID_PREFIX: &str = "ask:";
/// `content` 的 `type` 值。
pub(crate) const CONTENT_TYPE: &str = "ask_answers";
/// 遠端 `hook.sh` 塞進 Stop payload 的鍵：本機 transcript 裡已答完的提問（`setup.rs` 的 `REMOTE_HOOK_SH_TEMPLATE`）。
pub(crate) const CARRIED_KEY: &str = "agm_asks";
/// 讀 transcript 最後這麼多位元組：一個回合的提問一定在尾巴裡。
const TAIL_BYTES: u64 = 512 * 1024;
/// 一次最多補這麼多筆（一回合問上十次已經不尋常；擋住壞資料灌爆對話）。
const MAX_RECORDS: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AskItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    pub question: String,
    /// `None`＝這一題沒有回答（整組被取消，或多題裡跳過這一題）。
    pub answer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AskRecord {
    /// `AskUserQuestion` 那個 `tool_use` 的 id。
    pub id: String,
    /// 答完（或取消）的時間，RFC3339。讀不到就是現在。
    #[serde(default)]
    pub at: Option<String>,
    pub items: Vec<AskItem>,
}

impl AskRecord {
    fn answered(&self) -> bool {
        self.items.iter().any(|i| i.answer.is_some())
    }

    /// 存進 `messages.content` 的 JSON。
    pub(crate) fn content(&self) -> String {
        json!({"type": CONTENT_TYPE, "tool_use_id": self.id, "answered": self.answered(), "items": self.items}).to_string()
    }
}

/// `answers` 的值：字串照原文；多選有時是陣列，用「、」接起來。
fn answer_text(v: &Value) -> Option<String> {
    match v {
        Value::String(t) => Some(t.clone()),
        Value::Array(a) => {
            let parts: Vec<&str> = a.iter().filter_map(Value::as_str).collect();
            (!parts.is_empty()).then(|| parts.join("、"))
        }
        _ => None,
    }
}

/// 題目清單（`input.questions` 或 `toolUseResult.questions`）＋ 答案表，組成每題一列。
fn items(questions: &Value, answers: Option<&Value>, annotations: Option<&Value>) -> Vec<AskItem> {
    let Some(questions) = questions.as_array() else { return Vec::new() };
    questions
        .iter()
        .filter_map(|q| {
            let question = q.get("question").and_then(Value::as_str)?.to_string();
            let answer = answers.and_then(|a| a.get(&question)).and_then(answer_text);
            let notes = annotations
                .and_then(|a| a.get(&question))
                .and_then(|a| a.get("notes"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(str::to_owned);
            Some(AskItem { header: q.get("header").and_then(Value::as_str).map(str::to_owned), question, answer, notes })
        })
        .collect()
}

/// 這個 `tool_result` 是不是「使用者取消／拒絕回答」。不是明確的取消就是 `false`——認不出來的形狀寧可不記，也不要記成「沒有回答」。
fn is_cancellation(entry: &Value, block: &Value) -> bool {
    if entry.get("toolUseResult").and_then(Value::as_str).is_some_and(|t| t.to_ascii_lowercase().contains("reject")) {
        return true;
    }
    let text = match block.get("content") {
        Some(Value::String(t)) => t.clone(),
        Some(Value::Array(parts)) => parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    };
    text.starts_with("The user did not answer") || text.starts_with("The user doesn't want to proceed")
}

/// transcript 裡每一個**已經結束**（有對應 `tool_result`）的 `AskUserQuestion`，照出現順序。
///
/// 還在等人的題目沒有 `tool_result`，不在這裡（那是 `pending_question` 的事）。一行一筆 JSON；
/// 從中間讀時第一行可能是斷的，解析不了就跳過。
pub(crate) fn asks_from_transcript(log: &str) -> Vec<AskRecord> {
    let mut asked: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    let mut out = Vec::new();
    for line in log.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else { continue };
        let Some(content) = entry.pointer("/message/content").and_then(Value::as_array) else { continue };
        for block in content {
            match block.get("type").and_then(Value::as_str) {
                Some("tool_use") if block.get("name").and_then(Value::as_str) == Some("AskUserQuestion") => {
                    if let (Some(id), Some(input)) = (block.get("id").and_then(Value::as_str), block.get("input")) {
                        asked.insert(id.to_string(), input.clone());
                    }
                }
                Some("tool_result") => {
                    let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else { continue };
                    let Some(input) = asked.get(id) else { continue };
                    let result = entry.get("toolUseResult");
                    let from_result = result.and_then(|r| r.get("answers")).filter(|a| a.is_object());
                    let rec_items = if let Some(answers) = from_result {
                        let questions = result.and_then(|r| r.get("questions")).filter(|q| q.is_array()).unwrap_or_else(|| input.get("questions").unwrap_or(&Value::Null));
                        items(questions, Some(answers), result.and_then(|r| r.get("annotations")))
                    } else if is_cancellation(&entry, block) {
                        items(input.get("questions").unwrap_or(&Value::Null), None, None)
                    } else {
                        continue;
                    };
                    if rec_items.is_empty() {
                        continue;
                    }
                    out.push(AskRecord { id: id.to_string(), at: entry.get("timestamp").and_then(Value::as_str).map(str::to_owned), items: rec_items });
                }
                _ => {}
            }
        }
    }
    out
}

/// PostToolUse（`tool_name == AskUserQuestion`）的 payload → 一筆紀錄。認不出任何答案就是 `None`（見模組說明）。
///
/// 答案先找 `tool_response.answers`（跟 transcript 的 `toolUseResult` 同一個形狀），再退回
/// `tool_response` 本身就是答案表的形狀。題目一律用 `tool_input.questions`（有 header）。
pub(crate) fn from_post_tool_use(payload: &Value) -> Option<AskRecord> {
    if payload.get("tool_name").and_then(Value::as_str) != Some("AskUserQuestion") {
        return None;
    }
    let id = payload.get("tool_use_id").and_then(Value::as_str)?.to_string();
    let input = payload.get("tool_input")?;
    let response = payload.get("tool_response")?;
    let answers = response.get("answers").filter(|a| a.is_object())?;
    let it = items(input.get("questions")?, Some(answers), response.get("annotations"));
    let rec = AskRecord { id, at: None, items: it };
    (rec.answered()).then_some(rec)
}

/// 遠端 `hook.sh` 帶來的 `agm_asks`（格式同 [`AskRecord`]）。壞掉的整筆丟掉。
pub(crate) fn carried(payload: &Value) -> Vec<AskRecord> {
    let Some(arr) = payload.get(CARRIED_KEY).and_then(Value::as_array) else { return Vec::new() };
    arr.iter()
        .take(MAX_RECORDS)
        .filter_map(|v| serde_json::from_value::<AskRecord>(v.clone()).ok())
        .filter(|r| !r.id.is_empty() && !r.items.is_empty())
        .collect()
}

/// 本機 transcript 尾巴裡已結束的提問。讀不到＝沒有。
pub(crate) async fn from_local_transcript(path: &str) -> Vec<AskRecord> {
    let path = std::path::PathBuf::from(path);
    let mut recs = tokio::task::spawn_blocking(move || crate::pending_question::read_tail(&path, TAIL_BYTES).map(|log| asks_from_transcript(&log)))
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    if recs.len() > MAX_RECORDS {
        recs.drain(..recs.len() - MAX_RECORDS);
    }
    recs
}

fn normalize(at: &str) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(at).ok().map(|t| t.with_timezone(&chrono::Utc).to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

async fn record_on_turn(
    app: &(impl crate::capabilities::Db + crate::events::ports::TurnCommands),
    bot_id: &str,
    conversation_id: &str,
    turn_id: &str,
    turn_started: &str,
    records: Vec<AskRecord>,
) -> Result<usize> {
    if records.is_empty() {
        return Ok(0);
    }
    // 兩台機器的時鐘只比到秒：留一點餘裕，寧可多收到剛好壓線的一筆（冪等會擋重複）。
    let floor = chrono::DateTime::parse_from_rfc3339(turn_started).ok().map(|t| t.timestamp() - 2);
    let mut added = 0;
    for rec in records {
        let at = rec.at.as_deref().and_then(normalize);
        if let (Some(floor), Some(at)) = (floor, at.as_deref().and_then(|a| chrono::DateTime::parse_from_rfc3339(a).ok())) {
            if at.timestamp() < floor {
                continue;
            }
        }
        let created_at = at.unwrap_or_else(db::now);
        let id = format!("{ID_PREFIX}{conversation_id}:{}", rec.id);
        let done = sqlx::query(
            "INSERT OR IGNORE INTO messages (id, conversation_id, turn_id, role, content, source, incomplete, created_at)
             VALUES (?,?,?,'system',?,'system',0,?)",
        )
        .bind(&id)
        .bind(conversation_id)
        .bind(turn_id)
        .bind(rec.content())
        .bind(&created_at)
        .execute(app.db())
        .await?;
        if done.rows_affected() == 0 {
            continue;
        }
        added += 1;
        let m = sqlx::query_as::<_, db::Message>("SELECT *, rowid AS seq FROM messages WHERE id = ?").bind(&id).fetch_one(app.db()).await?;
        app.emit_message_added(bot_id, m).await;
    }
    Ok(added)
}

/// 把回合已結束時讀到的提問記進對話裡最新已開始的回合。回傳新增了幾則。
///
/// Stop 也可能替終端手打的外部回合剛建立新 turn，因此這條路要在 `hookrecv::process` 收尾時才挑回合。
pub(crate) async fn record(app: &(impl crate::capabilities::Db + crate::events::ports::TurnCommands), bot_id: &str, conversation_id: &str, records: Vec<AskRecord>) -> Result<usize> {
    let Some((turn_id, turn_started)) = sqlx::query_as::<_, (String, String)>(
        "SELECT id, created_at FROM turns WHERE conversation_id = ? AND status <> 'queued' ORDER BY created_at DESC, rowid DESC LIMIT 1",
    )
    .bind(conversation_id)
    .fetch_optional(app.db())
    .await?
    else {
        return Ok(0);
    };
    record_on_turn(app, bot_id, conversation_id, &turn_id, &turn_started, records).await
}

/// PostToolUse arrives before Stop creates a terminal-typed external turn. Attach its early copy only
/// to a turn that is actually in flight; if there is none, Stop will record it from the transcript.
pub(crate) async fn record_in_flight(
    app: &(impl crate::capabilities::Db + crate::events::ports::TurnCommands),
    bot_id: &str,
    conversation_id: &str,
    records: Vec<AskRecord>,
) -> Result<usize> {
    let Some((turn_id, turn_started)) = sqlx::query_as::<_, (String, String)>(
        "SELECT id, created_at FROM turns WHERE conversation_id = ? AND status = 'in_flight' ORDER BY created_at DESC, rowid DESC LIMIT 1",
    )
    .bind(conversation_id)
    .fetch_optional(app.db())
    .await?
    else {
        return Ok(0);
    };
    record_on_turn(app, bot_id, conversation_id, &turn_id, &turn_started, records).await
}

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

    /// 2026-10-02 wits-pro 真 transcript 的形狀：`toolUseResult.answers` 是 {題目: 答案}，自訂文字（Other）照原文。
    #[test]
    fn answers_come_from_the_structured_tool_use_result_in_question_order() {
        let log = [
            ask("t1", qs()),
            answered("t1", "2026-10-02T08:34:54.635Z", json!({"go API 放哪？": "自己寫的：放 gateway\n第二行", "拋單怎麼處理？": "拿掉拋單"})),
        ]
        .join("\n");
        let got = asks_from_transcript(&log);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "t1");
        assert_eq!(got[0].at.as_deref(), Some("2026-10-02T08:34:54.635Z"));
        let q: Vec<_> = got[0].items.iter().map(|i| (i.header.as_deref().unwrap(), i.question.as_str(), i.answer.as_deref().unwrap())).collect();
        assert_eq!(q, [("拋單倉庫", "拋單怎麼處理？", "拿掉拋單"), ("go API", "go API 放哪？", "自己寫的：放 gateway\n第二行")]);
        assert!(got[0].answered());
    }

    /// 取消（Cancel／Esc）的兩種長相都記成沒有回答；題目還是列出來。
    #[test]
    fn a_cancelled_question_is_recorded_as_not_answered() {
        let declined = json!({"type": "user", "timestamp": "2026-10-02T08:40:00.000Z",
            "message": {"content": [{"type": "tool_result", "tool_use_id": "t1", "content": "The user did not answer the questions."}]},
            "toolUseResult": {"questions": qs()}})
        .to_string();
        let rejected = json!({"type": "user", "timestamp": "2026-10-02T08:41:00.000Z",
            "message": {"content": [{"type": "tool_result", "is_error": true, "tool_use_id": "t2", "content": "The user doesn't want to proceed with this tool use."}]},
            "toolUseResult": "User rejected tool use"})
        .to_string();
        let got = asks_from_transcript(&[ask("t1", qs()), declined, ask("t2", qs()), rejected].join("\n"));
        assert_eq!(got.len(), 2);
        for r in &got {
            assert!(!r.answered());
            assert_eq!(r.items.len(), 2);
            assert!(r.items.iter().all(|i| i.answer.is_none()));
        }
        assert!(serde_json::from_str::<Value>(&got[0].content()).unwrap()["answered"] == json!(false));
    }

    /// 多題只答一部分：沒答的那題是 None，答了的照記；notes 一併帶。
    #[test]
    fn a_partly_answered_set_keeps_the_gaps_and_the_notes() {
        let entry = json!({"type": "user", "timestamp": "2026-10-02T08:34:54.635Z",
            "message": {"content": [{"type": "tool_result", "tool_use_id": "t1", "content": "x"}]},
            "toolUseResult": {"questions": qs(), "answers": {"拋單怎麼處理？": "保留拋單"}, "annotations": {"拋單怎麼處理？": {"notes": "先保留，之後再議"}}}})
        .to_string();
        let got = asks_from_transcript(&[ask("t1", qs()), entry].join("\n"));
        assert_eq!(got[0].items[0].answer.as_deref(), Some("保留拋單"));
        assert_eq!(got[0].items[0].notes.as_deref(), Some("先保留，之後再議"));
        assert_eq!(got[0].items[1].answer, None);
        assert!(got[0].answered());
    }

    /// 還在等人的題目、別的工具的結果、認不出的形狀、斷掉的第一行：都不記（尤其不能把認不出的記成「沒有回答」）。
    #[test]
    fn pending_foreign_or_unrecognised_results_are_not_recorded() {
        assert!(asks_from_transcript(&ask("t1", qs())).is_empty(), "沒有 tool_result＝還在等");
        let other = answered("other-tool", "2026-10-02T08:34:54.635Z", json!({"拋單怎麼處理？": "x"}));
        assert!(asks_from_transcript(&[ask("t1", qs()), other].join("\n")).is_empty());
        let odd = json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "t1", "content": "something new claude says"}]}}).to_string();
        assert!(asks_from_transcript(&[ask("t1", qs()), odd].join("\n")).is_empty());
        assert!(asks_from_transcript("{cut off first line\n").is_empty());
    }

    /// PostToolUse：只在認得出答案時才記；取消、沒帶 answers、別的工具都是 None。
    #[test]
    fn post_tool_use_needs_a_recognisable_answer() {
        let p = json!({"hook_event_name": "PostToolUse", "tool_name": "AskUserQuestion", "tool_use_id": "t1",
            "tool_input": {"questions": qs()}, "tool_response": {"questions": qs(), "answers": {"拋單怎麼處理？": "拿掉拋單"}}});
        let rec = from_post_tool_use(&p).expect("有答案");
        assert_eq!((rec.id.as_str(), rec.items.len(), rec.items[0].answer.as_deref(), rec.items[1].answer.clone()), ("t1", 2, Some("拿掉拋單"), None));
        let mut no_answers = p.clone();
        no_answers["tool_response"] = json!("The user did not answer the questions.");
        assert_eq!(from_post_tool_use(&no_answers), None);
        let mut empty = p.clone();
        empty["tool_response"]["answers"] = json!({});
        assert_eq!(from_post_tool_use(&empty), None);
        let mut bash = p.clone();
        bash["tool_name"] = json!("Bash");
        assert_eq!(from_post_tool_use(&bash), None);
    }

    /// 遠端帶來的 `agm_asks`：格式對得上的收，壞的丟。
    #[test]
    fn carried_records_skip_malformed_entries() {
        let p = json!({"agm_asks": [
            {"id": "t1", "at": "2026-10-02T08:34:54Z", "items": [{"header": "h", "question": "q", "answer": "a"}]},
            {"id": "", "items": [{"question": "q", "answer": null}]},
            {"nonsense": true},
        ]});
        let got = carried(&p);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].items[0].answer.as_deref(), Some("a"));
        assert!(carried(&json!({})).is_empty());
    }

    /// Stop's follow-up transcript read must apply the same bot-owned path check as hookrecv;
    /// otherwise a token holder can import an arbitrary local JSONL file into this conversation.
    #[tokio::test]
    async fn a_stop_does_not_import_ask_answers_from_outside_the_bots_projects_dir() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "ask-path-guard").await;
        crate::testing::fake_run(&app, &bot.id).await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let turn = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at) VALUES (?,?, 'web','in_flight','ok',?)")
            .bind(&turn)
            .bind(&conv)
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
