//! codex 的非同步提問（`request_user_input_async`，issue #1879）。
//!
//! 這個工具不擋回合：codex 立刻回 `{"accepted":true}` 繼續做事，題目只排在 TUI 的 `Queued follow-up inputs`，
//! 收成一行 `? 1 question · 7s / shift+← to answer`——題目與選項畫面上看不到，終端視窗也沒有 shift+← 鍵。
//! 題目其實完整寫在 rollout（`function_call` 的 `arguments`），使用者作答時 codex 再寫一則 user message：
//! `<send_user_message_question_reply>[{"answer","question","questionItemId":"[\"request_user_input_async\",\"<call_id>\",<index>]"}]`
//! （codex 0.163.0 真機；每一題各一則）。這裡：
//! - [`pending_async_questions`]：rollout 裡還沒有對應 reply 的題目（只讀、不碰畫面）；
//! - [`async_screen`]：畫面上的收合／展開狀態與題數（作答前每一步用它對帳，認不出就不送鍵）。
//!
//! 按 `^]` 跳過的題目不會寫任何東西進 rollout，所以 rollout 只能說「還沒作答」，不能說「畫面上還排著」；
//! 呼叫端要拿 [`AsyncScreen::count`] 取最後幾題（畫面是這份清單的真相）。

use serde_json::Value;

/// rollout 的檔尾讀這麼多：提問之後 codex 會繼續做事，工具輸出可能把題目推到幾 MB 之前。
pub const TAIL_BYTES: u64 = 8 * 1024 * 1024;

const ASK_TOOL: &str = "request_user_input_async";
const REPLY_TAG: &str = "<send_user_message_question_reply>";

/// 一題（同一個 `function_call` 可以帶好幾題，`index` 是它在 `questions[]` 的位置）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsyncQuestion {
    pub call_id: String,
    pub index: usize,
    pub title: String,
    /// 沒有選項（自由輸入）是空陣列。
    pub options: Vec<String>,
}

/// 還沒作答的題目，照提問順序。一行一筆 JSON；第一行可能被檔尾讀取切壞，解析不了就跳過。
/// 提問與回答只認 `response_item`；`event_msg` 的 `task_complete`／`turn_aborted` 會清掉該回合未答的題目（#1896）。
pub fn pending_async_questions(log: &str) -> Vec<AsyncQuestion> {
    let mut pending: Vec<AsyncQuestion> = Vec::new();
    for line in log.lines() {
        // 檔尾幾 MB 幾乎都是工具輸出：先用子字串過濾，不對每一行做 JSON 解析。
        let ask = line.contains(ASK_TOOL);
        let reply = line.contains("send_user_message_question_reply");
        let ended = line.contains("task_complete") || line.contains("turn_aborted");
        if !ask && !reply && !ended {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line) else { continue };
        // codex 0.163.0 的回合收尾會 take_pending_drafts → clear_pending；只接受完整的外層事件，
        // 工具輸出／引文裡同名的字不算。task_started／turn_context 不是清佇列的證據。
        if entry.get("type").and_then(Value::as_str) == Some("event_msg")
            && matches!(entry.get("payload").and_then(|p| p.get("type")).and_then(Value::as_str), Some("task_complete" | "turn_aborted"))
        {
            pending.clear();
            continue;
        }
        if entry.get("type").and_then(Value::as_str) != Some("response_item") {
            continue;
        }
        let Some(payload) = entry.get("payload") else { continue };
        match payload.get("type").and_then(Value::as_str) {
            Some("function_call") if payload.get("name").and_then(Value::as_str) == Some(ASK_TOOL) => {
                let call_id = payload.get("call_id").and_then(Value::as_str).unwrap_or_default();
                let args = payload.get("arguments").and_then(Value::as_str).and_then(|a| serde_json::from_str::<Value>(a).ok());
                let Some(questions) = args.as_ref().and_then(|a| a.get("questions")).and_then(Value::as_array) else { continue };
                for (index, q) in questions.iter().enumerate() {
                    let title = q.get("title").and_then(Value::as_str).unwrap_or_default().trim().to_string();
                    if title.is_empty() || call_id.is_empty() || pending.iter().any(|p| p.call_id == call_id && p.index == index) {
                        continue;
                    }
                    pending.push(AsyncQuestion { call_id: call_id.to_string(), index, title, options: option_labels(q.get("options")) });
                }
            }
            Some("message") if payload.get("role").and_then(Value::as_str) == Some("user") => {
                for text in payload.get("content").and_then(Value::as_array).into_iter().flatten().filter_map(|c| c.get("text").and_then(Value::as_str)) {
                    for (call_id, index) in answered_items(text) {
                        pending.retain(|p| !(p.call_id == call_id && p.index == index));
                    }
                }
            }
            _ => {}
        }
    }
    pending
}

/// `options` 是字串陣列；也容忍 `{label}` 物件（之後加說明欄位時不要整題消失）。
fn option_labels(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|o| match o {
            Value::String(s) => Some(s.trim().to_string()),
            Value::Object(m) => m.get("label").and_then(Value::as_str).map(|s| s.trim().to_string()),
            _ => None,
        })
        .filter(|s| !s.is_empty())
        .collect()
}

/// 一則 user message 裡所有 reply 回答的 `(call_id, index)`。
fn answered_items(text: &str) -> Vec<(String, usize)> {
    let Some(start) = text.find(REPLY_TAG) else { return Vec::new() };
    let body = &text[start + REPLY_TAG.len()..];
    let body = body.split("</send_user_message_question_reply>").next().unwrap_or(body);
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(body.trim()) else { return Vec::new() };
    items
        .iter()
        .filter_map(|item| {
            // `questionItemId` 本身是一個 JSON 字串：`["request_user_input_async","<call_id>",<index>]`。
            let id = serde_json::from_str::<Value>(item.get("questionItemId")?.as_str()?).ok()?;
            let parts = id.as_array()?;
            (parts.first()?.as_str()? == ASK_TOOL).then_some(())?;
            Some((parts.get(1)?.as_str()?.to_string(), usize::try_from(parts.get(2)?.as_u64()?).ok()?))
        })
        .collect()
}

/// 畫面上的非同步提問。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AsyncScreen {
    /// 收合：`? N question(s)` ＋ `shift+← to answer`；展開：游標在題目上，底下 `enter submit  ^] skip`。
    pub expanded: bool,
    /// 收合時是畫面上寫的題數；展開時是 `k of n` 的 n，沒有那一行就是 1。
    pub count: usize,
}

/// 畫面上有沒有非同步提問、什麼狀態。只看畫面底下 [`SCAN`] 行：上面的對話紀錄會原樣引用題目文字。
pub fn async_screen(text: &str) -> Option<AsyncScreen> {
    const SCAN: usize = 40;
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    let from = lines.len().saturating_sub(SCAN);
    let lines = &lines[from..];
    for (i, line) in lines.iter().enumerate().rev() {
        // 收合：`? 1 question · 7s`，下一個有字的行是 `shift+← to answer`。
        if let Some(count) = collapsed_count(line) {
            let next = lines[i + 1..].iter().find(|l| !l.is_empty());
            if next.is_some_and(|l| l.starts_with("shift+← to answer") || l.starts_with("⇧← to answer")) {
                return Some(AsyncScreen { expanded: false, count });
            }
        }
        // 展開：腳註 `enter submit   ^] skip   shift+→ main prompt`（多題另加 `shift+← next question`）。
        if line.starts_with("enter submit") && (line.contains("^] skip") || line.contains("⌃] skip")) {
            let count = lines[..i]
                .iter()
                .rev()
                .take(8)
                .find_map(|l| of_total(l))
                .unwrap_or(1);
            return Some(AsyncScreen { expanded: true, count });
        }
    }
    None
}

/// `? 3 questions · 12s` → 3。
fn collapsed_count(line: &str) -> Option<usize> {
    let rest = line.strip_prefix('?')?.trim_start();
    let (n, tail) = rest.split_once(' ')?;
    let n: usize = n.parse().ok()?;
    let tail = tail.strip_prefix("question")?;
    let tail = tail.strip_prefix('s').unwrap_or(tail);
    (tail.is_empty() || tail.starts_with(' ')).then_some(n).filter(|n| *n > 0)
}

/// `1 of 2` → 2。
fn of_total(line: &str) -> Option<usize> {
    let (k, n) = line.split_once(" of ")?;
    k.parse::<usize>().ok()?;
    n.parse::<usize>().ok().filter(|n| *n > 0)
}

/// 排隊面板的標題行；它上面是對話紀錄（會原樣引用題目），它底下才是提問本身。
const QUEUE_HEADER: &str = "Queued follow-up inputs";

fn squash(s: &str) -> String {
    s.split_whitespace().collect()
}

/// 畫面上對話紀錄那一段（排隊面板標題以上）與面板那一段（標題以下、腳註以上）。沒有標題行就整張當對話紀錄、面板空。
fn split_panel(text: &str) -> (String, String) {
    let lines: Vec<&str> = text.lines().collect();
    let Some(head) = lines.iter().rposition(|l| l.trim().trim_start_matches('•').trim() == QUEUE_HEADER) else { return (text.to_string(), String::new()) };
    let tail = &lines[head + 1..];
    let end = tail.iter().position(|l| l.trim().starts_with("enter submit")).unwrap_or(tail.len());
    (lines[..head].join("\n"), tail[..end].join("\n"))
}

/// 去掉行首的項目符號與縮排、拿掉所有空白（折行與縮排不計）。
fn norm_line(l: &str) -> String {
    squash(l.trim_start_matches(|c: char| c == '•' || c == '·' || c == '-' || c.is_whitespace()))
}

/// 這題的原文**整段**出現在畫面上：連續幾行（行首項目符號不計）拼起來完整等於原文。不是子字串——舊題「Name?」
/// 不會對上畫面上的「Full Name?」（issue #1879 複審）。
pub fn title_in(title: &str, text: &str) -> bool {
    let want = squash(title);
    if want.is_empty() {
        return false;
    }
    let lines: Vec<String> = text.lines().map(norm_line).collect();
    for start in 0..lines.len() {
        let mut acc = String::new();
        for line in &lines[start..] {
            acc.push_str(line);
            if acc == want {
                return true;
            }
            if !want.starts_with(&acc) {
                break;
            }
        }
    }
    false
}

/// 展開的面板開頭的題目行（略過多題時的 `1 of 2` 頁碼行）完整等於這題原文。
pub fn panel_title_is(title: &str, panel: &str) -> bool {
    let want = squash(title);
    if want.is_empty() {
        return false;
    }
    let mut lines = panel.lines().map(str::trim).filter(|l| !l.is_empty()).peekable();
    if lines.peek().is_some_and(|l| of_total(l).is_some()) {
        lines.next();
    }
    let mut acc = String::new();
    for line in lines {
        acc.push_str(&squash(line));
        if acc == want {
            return true;
        }
        if !want.starts_with(&acc) {
            return false;
        }
    }
    false
}

/// 畫面上排著的是 rollout 清單裡的哪幾題；**對不上就回空陣列（降級：只講題數）**，寧可降級、不對錯題（issue #1879 審查）。
///
/// rollout 只知道「沒有 reply」，不知道使用者在 TUI 按 `^]` 跳過了哪一題：
/// - 收合：只有「沒作答的題數剛好等於畫面題數」才顯示題目——有題目被跳過時沒作答的題數一定多於畫面題數，所以這條同時保證
///   沒有任何跳過歧義。多於畫面題數一律降級：對話紀錄裡看得到原文**不是**證據（Q1 待答但已捲出、Q2 剛跳過但原文可見時，
///   會錯認 Q2）。
/// - 展開：面板裡的題目原文逐一對得上的那幾題（通常一題），面板就是畫面上的真相；對不上降級。
pub fn resolve(pending: Vec<AsyncQuestion>, screen: &AsyncScreen, text: &str) -> Vec<AsyncQuestion> {
    if screen.expanded {
        let (_, panel) = split_panel(text);
        return pending.into_iter().filter(|q| panel_title_is(&q.title, &panel)).collect();
    }
    if pending.len() == screen.count {
        return pending;
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ask(call_id: &str, questions: Value) -> String {
        json!({"type": "response_item", "payload": {"type": "function_call", "name": ASK_TOOL, "call_id": call_id,
            "arguments": json!({"questions": questions}).to_string()}})
        .to_string()
    }

    fn reply(call_id: &str, index: usize, answer: &str) -> String {
        let items = json!([{"answer": answer, "question": "q", "questionItemId": json!([ASK_TOOL, call_id, index]).to_string()}]);
        json!({"type": "response_item", "payload": {"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": format!("{REPLY_TAG}\n{items}\n</send_user_message_question_reply>")}]}})
        .to_string()
    }

    // #1896：m4p codex 0.163.0 真實測試 rollout 的去敏片段（顏色／水果／城市）。
    // 原行號 1、2、8、14、62、64、65、70、112、114、115、120、170–174、179；
    // 只留相關事件，移除路徑／帳號等 metadata，turn/call id 改成別名，沒有捏造 resume 事件。
    const STALE_TURNS: &str = r#"{"timestamp":"2026-10-22T07:58:00.782Z","type":"session_meta","payload":{"cli_version":"0.163.0","source":"cli"}}
{"timestamp":"2026-10-22T07:58:00.782Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}
{"timestamp":"2026-10-22T07:58:01.261Z","type":"turn_context","payload":{"turn_id":"turn-1"}}
{"timestamp":"2026-10-22T07:58:03.975Z","type":"response_item","payload":{"type":"function_call","name":"request_user_input_async","call_id":"call-1","arguments":"{\"questions\":[{\"title\":\"測試題：要選哪個顏色？\",\"options\":[\"紅色\",\"藍色\"]}]}"}}
{"timestamp":"2026-10-22T08:00:09.193Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1"}}
{"timestamp":"2026-10-22T08:01:57.146Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-2"}}
{"timestamp":"2026-10-22T08:01:57.160Z","type":"turn_context","payload":{"turn_id":"turn-2"}}
{"timestamp":"2026-10-22T08:02:00.057Z","type":"response_item","payload":{"type":"function_call","name":"request_user_input_async","call_id":"call-2","arguments":"{\"questions\":[{\"title\":\"測試題：要選哪個顏色？\",\"options\":[\"紅色\",\"藍色\"]}]}"}}
{"timestamp":"2026-10-22T08:05:05.193Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-2"}}
{"timestamp":"2026-10-22T08:05:43.976Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-3"}}
{"timestamp":"2026-10-22T08:05:43.988Z","type":"turn_context","payload":{"turn_id":"turn-3"}}
{"timestamp":"2026-10-22T08:05:46.211Z","type":"response_item","payload":{"type":"function_call","name":"request_user_input_async","call_id":"call-3","arguments":"{\"questions\":[{\"title\":\"測試題二：要選哪個水果？\",\"options\":[\"蘋果\",\"香蕉\"]}]}"}}
{"timestamp":"2026-10-22T08:09:50.480Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-3"}}
{"timestamp":"2026-10-22T12:37:00.472Z","type":"event_msg","payload":{"type":"thread_settings_applied"}}
{"timestamp":"2026-10-22T12:37:05.354Z","type":"event_msg","payload":{"type":"thread_settings_applied"}}
{"timestamp":"2026-10-22T12:37:05.360Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-4"}}
{"timestamp":"2026-10-22T12:37:05.649Z","type":"turn_context","payload":{"turn_id":"turn-4"}}
{"timestamp":"2026-10-22T12:37:08.534Z","type":"response_item","payload":{"type":"function_call","name":"request_user_input_async","call_id":"call-4","arguments":"{\"questions\":[{\"title\":\"實測題：要選哪個城市？\",\"options\":[\"台北\",\"高雄\"]}]}"}}
"#;

    #[test]
    fn completed_old_turns_do_not_hide_the_current_question() {
        let meta: Value = serde_json::from_str(STALE_TURNS.lines().next().unwrap()).unwrap();
        assert_eq!(meta["payload"]["cli_version"], "0.163.0");
        let pending = pending_async_questions(STALE_TURNS);
        assert_eq!(pending, vec![AsyncQuestion {
            call_id: "call-4".into(), index: 0, title: "實測題：要選哪個城市？".into(), options: vec!["台北".into(), "高雄".into()],
        }]);
        let (screen, text) = collapsed("", 1);
        assert_eq!(resolve(pending, &screen, &text).len(), 1);
    }

    #[test]
    fn resumed_history_keeps_ended_questions_cleared_without_a_new_session_marker() {
        // 真實的同檔續寫：只有第一次有 session_meta；恢復後先更新設定，再開始城市回合。
        let before_new_question = STALE_TURNS.lines().take(17).collect::<Vec<_>>().join("\n");
        assert_eq!(before_new_question.matches("session_meta").count(), 1);
        assert!(pending_async_questions(&before_new_question).is_empty());
        assert_eq!(pending_async_questions(STALE_TURNS)[0].call_id, "call-4");
        // 原 rollout 第 241 行：城市回合也已結束時，城市題同樣失效。
        let finished = format!("{STALE_TURNS}{}", json!({"type": "event_msg", "payload": {"type": "task_complete", "turn_id": "turn-4"}}));
        assert!(pending_async_questions(&finished).is_empty());
    }

    #[test]
    fn completion_and_interruption_clear_unanswered_questions() {
        for kind in ["task_complete", "turn_aborted"] {
            let log = [
                ask("old", json!([{"title": "Old?"}])),
                json!({"type": "event_msg", "payload": {"type": kind, "turn_id": "old-turn"}}).to_string(),
            ].join("\n");
            assert!(pending_async_questions(&log).is_empty(), "{kind}");
            let log = format!("{log}\n{}", ask("new", json!([{"title": "New?"}])));
            assert_eq!(pending_async_questions(&log), vec![q("new", "New?")]);
        }
    }

    #[test]
    fn answering_one_question_keeps_the_other_in_the_current_turn() {
        let log = [
            json!({"type": "event_msg", "payload": {"type": "task_started", "turn_id": "current"}}).to_string(),
            ask("current", json!([{"title": "First?"}, {"title": "Second?"}])),
            json!({"type": "turn_context", "payload": {"turn_id": "current"}}).to_string(),
            reply("current", 0, "answer"),
        ].join("\n");
        let pending = pending_async_questions(&log);
        assert_eq!(pending.len(), 1);
        assert_eq!((pending[0].call_id.as_str(), pending[0].index, pending[0].title.as_str()), ("current", 1, "Second?"));
        let (screen, text) = collapsed("", 1);
        assert_eq!(resolve(pending, &screen, &text).len(), 1);
    }

    #[test]
    fn a_cut_first_boundary_line_is_ignored_and_an_unproven_boundary_does_not_clear_questions() {
        for kind in ["task_complete", "turn_aborted"] {
            let boundary = json!({"type": "event_msg", "payload": {"type": kind}}).to_string();
            let cut = &boundary[1..];
            let current = ask("new", json!([{"title": "New?"}]));
            assert_eq!(pending_async_questions(&format!("{cut}\n{current}")), vec![q("new", "New?")]);
            // 中途有壞掉／截斷的事件時，不能只靠 type 的子字串把既有題目消掉而誤通過題數比對。
            let log = format!("{}\n{cut}\n{current}", ask("old", json!([{"title": "Old?"}])));
            let pending = pending_async_questions(&log);
            assert_eq!(pending.len(), 2);
            let (screen, text) = collapsed("", 1);
            assert!(resolve(pending, &screen, &text).is_empty());
        }
    }

    #[test]
    fn quoted_or_nested_terminal_events_are_not_boundaries() {
        let terminal = json!({"type": "event_msg", "payload": {"type": "task_complete"}});
        let log = [
            ask("old", json!([{"title": "Old?"}])),
            json!({"type": "response_item", "payload": {"type": "function_call_output", "output": terminal.to_string()}}).to_string(),
            json!({"type": "event_msg", "payload": {"type": "item_completed", "item": terminal}}).to_string(),
            json!({"type": "turn_context", "payload": {"type": "turn_aborted"}}).to_string(),
        ].join("\n");
        assert_eq!(pending_async_questions(&log), vec![q("old", "Old?")]);
    }

    #[test]
    fn clearing_a_completed_turn_does_not_guess_past_a_current_turn_skip() {
        let log = [
            STALE_TURNS.lines().take(13).collect::<Vec<_>>().join("\n"),
            ask("current", json!([{"title": "Skipped?"}, {"title": "Still pending?"}])),
        ].join("\n");
        let pending = pending_async_questions(&log);
        assert_eq!(pending.len(), 2);
        let (screen, text) = collapsed("", 1);
        assert!(resolve(pending, &screen, &text).is_empty(), "不取最後 N 題，當前回合 skip 歧義仍降級");
    }

    /// synthetic：形狀照 codex 0.163.0 真機 rollout（2026-10-21 隔離 session）寫的。
    #[test]
    fn an_unanswered_question_is_read_from_the_rollout() {
        let log = ["{cut first line".to_string(), ask("c1", json!([{"title": "Which color?", "options": ["red", "blue", "green"]}]))].join("\n");
        let got = pending_async_questions(&log);
        assert_eq!(got, vec![AsyncQuestion { call_id: "c1".into(), index: 0, title: "Which color?".into(), options: vec!["red".into(), "blue".into(), "green".into()] }]);
    }

    /// 沒有 `options`（自由輸入）是空陣列；`options: null` 同。
    #[test]
    fn a_free_text_question_has_no_options() {
        let got = pending_async_questions(&ask("c1", json!([{"title": "你的名字？"}, {"title": "地址？", "options": null}])));
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|q| q.options.is_empty()));
        assert_eq!(got[1].index, 1);
    }

    /// 每題各自一則 reply：答第 0 題，第 1 題還在等；別的 call 的 reply 不算。
    #[test]
    fn a_reply_answers_only_its_own_question() {
        let log = [
            ask("c1", json!([{"title": "Pick a size?", "options": ["S", "M"]}, {"title": "Your name?"}])),
            reply("other", 0, "x"),
            reply("c1", 0, "S"),
        ]
        .join("\n");
        let got = pending_async_questions(&log);
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].call_id.as_str(), got[0].index, got[0].title.as_str()), ("c1", 1, "Your name?"));
        assert!(pending_async_questions(&[log, reply("c1", 1, "Bob")].join("\n")).is_empty());
    }

    /// `event_msg` 的 `item_completed` 是同一題的副本：不能讓一題出現兩次、也不能讓副本重新問一次已答的題。
    #[test]
    fn event_copies_are_ignored() {
        let copy = json!({"type": "event_msg", "payload": {"type": "item_completed", "item": {"type": "AgentMessage", "id": "c1", "delivery": "async",
            "questions": [{"title": "Which color?", "options": ["red"]}]}}})
        .to_string();
        let log = [ask("c1", json!([{"title": "Which color?"}])), copy.clone(), reply("c1", 0, "red"), copy].join("\n");
        assert!(pending_async_questions(&log).is_empty());
    }

    /// 一般 user message（含提到工具名的）不算回答。
    #[test]
    fn an_ordinary_user_message_does_not_answer() {
        let chat = json!({"type": "response_item", "payload": {"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "request_user_input_async 那題我晚點回"}]}})
        .to_string();
        assert_eq!(pending_async_questions(&[ask("c1", json!([{"title": "Q?"}])), chat].join("\n")).len(), 1);
    }

    // 以下畫面是 codex 0.163.0 的真畫面（2026-10-21 隔離 tmux session 抓的，路徑已拿掉）。
    const COLLAPSED: &str = "\
• Which color?
  • red
  • blue
  • green
• Working (39s • esc to interrupt)
• Queued follow-up inputs
  ? 1 question · 7s
    shift+← to answer
› Ask Codex to do anything
  GPT-6.1-Sol default · ~/proj · Ask for color choice
  ← for agents · ? for shortcuts
";
    const EXPANDED: &str = "\
• Working (44s • esc to interrupt)
• Queued follow-up inputs
  Which color?
  › 1. red
    2. blue
    3. green
    4. Other
  enter submit   ^] skip   shift+→ main prompt
";
    const EXPANDED_OF_TWO: &str = "\
• Queued follow-up inputs
  1 of 2
  Pick a size?
  › 1. S
    2. M
    3. Other
  enter submit   ^] skip   shift+→ main prompt   shift+← next question
";

    #[test]
    fn the_screen_state_is_read_from_the_real_screens() {
        assert_eq!(async_screen(COLLAPSED), Some(AsyncScreen { expanded: false, count: 1 }));
        assert_eq!(async_screen(EXPANDED), Some(AsyncScreen { expanded: true, count: 1 }));
        assert_eq!(async_screen(EXPANDED_OF_TWO), Some(AsyncScreen { expanded: true, count: 2 }));
        assert_eq!(async_screen(&COLLAPSED.replace("? 1 question · 7s", "? 3 questions · 12s")), Some(AsyncScreen { expanded: false, count: 3 }));
    }

    /// The frontend reads this same fixture manifest, so Linux/macOS glyph support stays paired.
    #[test]
    fn shared_frontend_fixtures_have_the_same_async_screen_states() {
        let cases: Vec<Value> = serde_json::from_str(include_str!("../../../web/src/lib/__fixtures__/codex-async-screen-cases.json")).unwrap();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/src/lib/__fixtures__");
        for case in cases {
            let name = case["file"].as_str().unwrap();
            let text = std::fs::read_to_string(root.join(name)).unwrap();
            let got = async_screen(&text).map(|screen| json!({"expanded": screen.expanded, "count": screen.count}));
            assert_eq!(got.unwrap_or(Value::Null), case["expected"], "{name}");
        }
    }

    /// 對話紀錄裡引用的那幾行字、沒有 `shift+← to answer` 的問號行，都不算畫面上有提問。
    #[test]
    fn quoted_text_and_lookalikes_are_not_the_prompt() {
        assert_eq!(async_screen("• 使用者看到 `? 1 question` 和 shift+← to answer\n› Ask Codex to do anything\n"), None);
        assert_eq!(async_screen("  ? 1 question · 7s\n  something else\n"), None);
        assert_eq!(async_screen("  ? 0 questions\n  shift+← to answer\n"), None);
        assert_eq!(async_screen("  ? for shortcuts\n"), None);
        assert_eq!(async_screen(""), None);
    }

    fn q(call: &str, title: &str) -> AsyncQuestion {
        AsyncQuestion { call_id: call.into(), index: 0, title: title.into(), options: vec![] }
    }

    fn collapsed(chat: &str, count: usize) -> (AsyncScreen, String) {
        (AsyncScreen { expanded: false, count }, format!("{chat}\n• Queued follow-up inputs\n  ? {count} question · 7s\n    shift+← to answer\n› Ask Codex to do anything\n"))
    }

    /// 沒有跳過的題目：沒作答的題數等於畫面題數，不看對話紀錄也認（原文可能早就捲出畫面）。
    #[test]
    fn all_pending_questions_are_the_ones_on_screen_when_the_counts_agree() {
        let (screen, text) = collapsed("（原文已捲出畫面）", 2);
        assert_eq!(resolve(vec![q("a", "Q1?"), q("b", "Q2?")], &screen, &text).len(), 2);
    }

    /// 審查反例：同一個 call 問了 Q1、Q2，使用者切到 Q2 跳過，畫面剩 1 題＝Q1。兩題原文都在對話紀錄裡，分不出剩哪一題：降級，不能回 Q2。
    #[test]
    fn a_skipped_new_question_is_never_guessed() {
        let (screen, text) = collapsed("• Pick a size?\n  • S\n  Your name?", 1);
        assert!(resolve(vec![q("a", "Pick a size?"), q("a2", "Your name?")], &screen, &text).is_empty());
    }

    /// 複審反例：Q1 待答但已捲出畫面，Q2 剛跳過、原文還看得到；畫面剩 1 題（其實是 Q1）。對話紀錄看得到的是 Q2，
    /// 不能因此回 Q2：沒作答 2 題、畫面 1 題＝有跳過歧義，降級。
    #[test]
    fn a_visible_skipped_question_is_not_evidence_for_what_is_pending() {
        let (screen, text) = collapsed("• Q2?\n  • a\n  • b", 1);
        assert!(resolve(vec![q("a", "Q1?"), q("b", "Q2?")], &screen, &text).is_empty());
    }

    /// 早先跳過的舊題讓這個 session 的收合畫面一律降級（寧可降級）；展開後以面板原文為準。
    #[test]
    fn an_old_skipped_question_degrades_the_collapsed_state_but_not_the_expanded_panel() {
        let (screen, text) = collapsed("• 現在這題？\n  • 是", 1);
        assert!(resolve(vec![q("old", "早先跳過的？"), q("new", "現在這題？")], &screen, &text).is_empty());
        let expanded = "• Queued follow-up inputs\n  現在這題？\n  Type your answer\n  enter submit   ^] skip   shift+→ main prompt\n";
        let got = resolve(vec![q("old", "早先跳過的？"), q("new", "現在這題？")], &AsyncScreen { expanded: true, count: 1 }, expanded);
        assert_eq!(got.iter().map(|q| q.call_id.as_str()).collect::<Vec<_>>(), ["new"]);
    }

    /// rollout 的題數比畫面少（別處已作答、換了 session）：降級。
    #[test]
    fn fewer_pending_than_on_screen_degrades() {
        let (screen, text) = collapsed("", 3);
        assert!(resolve(vec![q("a", "Q?")], &screen, &text).is_empty());
    }

    /// 複審 P2：子字串不算同題——畫面上是「Full Name?」時，舊題「Name?」不能對上；反過來也不行。
    #[test]
    fn a_title_must_match_in_full_not_as_a_substring() {
        let panel = |t: &str| format!("• Queued follow-up inputs\n  {t}\n  Type your answer\n  enter submit   ^] skip   shift+→ main prompt\n");
        let screen = AsyncScreen { expanded: true, count: 1 };
        assert!(resolve(vec![q("a", "Name?")], &screen, &panel("Full Name?")).is_empty());
        assert!(resolve(vec![q("a", "Full Name?")], &screen, &panel("Name?")).is_empty());
        assert_eq!(resolve(vec![q("a", "Full Name?")], &screen, &panel("1 of 2\n  Full Name?")).len(), 1, "頁碼行不是題目的一部分");
        assert!(title_in("Full Name?", "• Full Name?\n  • a\n"));
        assert!(!title_in("Name?", "• Full Name?\n  • a\n"));
        assert!(!title_in("Full Name?", "• Name?\n"));
        assert!(title_in("這是很長的題目折成兩行？", "• 這是很長的題目\n  折成兩行？\n  • a\n"), "折行拼回來算");
    }

    /// 展開：面板裡的題目原文對得上才算；對話紀錄裡引用的別題、舊題都不算。
    #[test]
    fn an_expanded_panel_matches_questions_by_their_text() {
        let text = "• Old question?\n• Queued follow-up inputs\n  Your name?\n  Type your answer\n  enter submit   ^] skip   shift+→ main prompt\n";
        let screen = AsyncScreen { expanded: true, count: 1 };
        let got = resolve(vec![q("a", "Old question?"), q("b", "Your name?")], &screen, text);
        assert_eq!(got.iter().map(|q| q.call_id.as_str()).collect::<Vec<_>>(), ["b"]);
        assert!(resolve(vec![q("a", "Unrelated?")], &screen, text).is_empty());
    }
}
