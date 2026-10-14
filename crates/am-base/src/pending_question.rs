//! claude 停在 `AskUserQuestion` 時，題目本身從 transcript 讀，不靠畫面（2026-09-23 使用者：「為什麼看不見題目」）。
//!
//! pane 太矮時 claude 會把自己的選單裁掉：題目那一行根本沒畫出來，選項也只剩捲動中的一段（`↓ 3.`、跳到 `5.`）。
//! 畫面上沒有的東西，網頁怎麼 parse 都拿不到；但 transcript 裡那個 `tool_use` 有完整的題目與選項。
//! 這裡找「最後一個還沒被回答的 `AskUserQuestion`」：有 `tool_use`、之後沒有對應 `tool_use_id` 的 `tool_result`。

use serde_json::Value;

/// 讀 transcript 最後這麼多位元組就夠：題目是最後幾筆訊息之一，不需要整份（長 session 的檔動輒幾十 MB）。
pub const TAIL_BYTES: u64 = 512 * 1024;

/// 最後一個還沒被回答的 `AskUserQuestion` 的 `input`（含 `questions`），沒有就 `None`。
///
/// 一行一筆 JSON；第一行可能因為從中間讀而被切壞，解析不了就跳過。
pub fn pending_ask(log: &str) -> Option<Value> {
    pending_ask_entry(log).map(|(_, input)| input)
}

/// 同 [`pending_ask`]，另帶那個 `tool_use` 的 id：同一題在同一個 run 裡再問一次是另一次提問，
/// 要拿 id 區分（Jev 的選項建議每個 `tool_use` 問一次，issue #1475）。
pub fn pending_ask_entry(log: &str) -> Option<(String, Value)> {
    let mut pending: Option<(String, Value)> = None;
    for line in log.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else { continue };
        // Sidechain tool calls are internal branches and must not replace or
        // answer the question visible in the main conversation.
        if entry.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        // claude 2.1.300 起每次 API 請求另記 `api-request-blob`，也帶 `message.content`（請求裡送出的訊息副本），
        // 不是對話本身；只認對話的那兩種，副本不能重新問一次或答掉一題（issue #1771）。
        if entry.get("type").and_then(Value::as_str).is_some_and(|t| t != "assistant" && t != "user") {
            continue;
        }
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
    pending.filter(|(_, input)| input.get("questions").and_then(Value::as_array).is_some_and(|q| !q.is_empty()))
}

pub fn read_tail(path: &std::path::Path, max: u64) -> Option<String> {
    crate::transcript_read::read_tail(path, max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ask(id: &str, question: &str) -> String {
        json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "tool_use", "id": id, "name": "AskUserQuestion",
            "input": {"questions": [{"question": question, "header": "連線方式", "multiSelect": false,
                "options": [{"label": "內網 http (Recommended)", "description": "流量留在 VPC"}, {"label": "公網 https", "description": "拿掉白名單"}]}]}}]}})
        .to_string()
    }

    fn answer(id: &str) -> String {
        json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": id, "content": "User answered"}]}}).to_string()
    }

    fn sidechain(entry: String) -> String {
        let mut value: serde_json::Value = serde_json::from_str(&entry).unwrap();
        value["isSidechain"] = json!(true);
        value.to_string()
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

    /// 帶 id 的版本回的是同一個 `tool_use` 的 id；同一題再問一次，id 不同。
    #[test]
    fn the_entry_carries_the_tool_use_id() {
        let (id, input) = pending_ask_entry(&[ask("t1", "舊題"), answer("t1"), ask("t2", "舊題")].join("\n")).unwrap();
        assert_eq!(id, "t2");
        assert_eq!(input["questions"][0]["question"], "舊題");
    }

    /// 別的工具的 tool_result 不算回答這一題。
    #[test]
    fn a_result_for_another_tool_does_not_answer_the_question() {
        let got = pending_ask(&[ask("t1", "還在等"), answer("other-tool")].join("\n"));
        assert!(got.is_some());
    }

    #[test]
    fn sidechain_questions_do_not_replace_or_answer_mainline_questions() {
        let mainline = ask("main", "主線問題");
        let sidechain_question = sidechain(ask("side", "旁支問題"));
        let got = pending_ask(&[mainline.clone(), sidechain_question].join("\n")).unwrap();
        assert_eq!(got["questions"][0]["question"], "主線問題");

        let sidechain_answer = sidechain(answer("main"));
        let got = pending_ask(&[mainline, sidechain_answer].join("\n")).unwrap();
        assert_eq!(got["questions"][0]["question"], "主線問題");
    }

    /// 沒有任何提問、或 input 壞掉（沒有 questions）：None，網頁照舊用畫面。
    #[test]
    fn nothing_to_show_without_a_real_question() {
        assert_eq!(pending_ask(""), None);
        let bad = json!({"message": {"content": [{"type": "tool_use", "id": "t1", "name": "AskUserQuestion", "input": {}}]}}).to_string();
        assert_eq!(pending_ask(&bad), None);
    }

    /// claude 2.1.300 實錄的請求紀錄（字串已遮蔽，結構與大小照原樣）。
    const API_REQUEST_LOG: &str = include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/claude_2.1.300_api_request.jsonl");

    /// issue #1771：題目後面夾著一回合的請求紀錄，固定大小的檔尾仍找得到；`api-request-blob` 裡的訊息副本不算提問也不算回答。
    #[test]
    fn api_request_records_after_the_question_do_not_hide_or_answer_it() {
        assert!(API_REQUEST_LOG.lines().any(|l| l.contains("\"type\":\"api-request-shape\"")));
        let blob = |m: serde_json::Value| json!({"type": "api-request-blob", "hash": "h", "sessionId": "s", "message": m}).to_string();
        let copy_answer = blob(serde_json::from_str::<serde_json::Value>(&answer("t1")).unwrap()["message"].clone());
        let copy_ask = blob(serde_json::from_str::<serde_json::Value>(&ask("t9", "副本")).unwrap()["message"].clone());
        // 一回合十幾次請求（各帶新的 shape 與 blob）都塞在題目之後。
        let mut lines = vec![ask("t1", "還在等")];
        for _ in 0..4 {
            lines.extend(API_REQUEST_LOG.lines().map(str::to_string));
        }
        lines.extend([copy_answer, copy_ask]);
        let log = lines.join("\n");
        assert!(log.len() < TAIL_BYTES as usize, "實測一回合遠小於檔尾：{}", log.len());
        let (id, input) = pending_ask_entry(&log).expect("題目還在等");
        assert_eq!(id, "t1");
        assert_eq!(input["questions"][0]["question"], "還在等");
        assert!(pending_ask(&[log, answer("t1")].join("\n")).is_none());
    }
}
