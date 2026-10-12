//! `AskUserQuestion` 的選項建議（issue #1475，SPEC §4.3c 第五個場景）。
//!
//! claude bot 停在 `AskUserQuestion` 等人選時，背景問 Jev 一次：**一個請求、每個選項一題 Noul**
//!（「依這顆 bot 目前的任務與使用者最近的指示，使用者會選這一項嗎？」）。最高分 ≥ [`SUGGEST_THRESHOLD`]
//! 且比第二名高 ≥ [`SUGGEST_MARGIN`] 才算有建議；否則只留分數、不標。
//!
//! **只提示，不代答、不自動送出**：這裡不碰按鍵、不寫對話、不改 bot 狀態。逾時、非 2xx、key 讀不到、
//! 超過每小時名額都只表現成「沒有建議」，問題照常顯示，也不重試。
//!
//! 題目不是 daemon 存的東西——它是 transcript 裡那個還沒被回答的 `tool_use`，每次 `GET /bots/{id}/pending-question`
//! 才讀出來。所以觸發點就在那支讀取端點（[`hint_for`]）：第一次看到某個 `tool_use` 丟一個背景 task 去問，
//! 之後的讀取只查帳本。帳本列 `regex_verdict='ask_option'`、`run_id`＝這一輪的 run，`matched_line` 是
//! `{tool_use_id, turn_id, questions:[{index, scores, suggested}]}`（答案回來之前只有 `tool_use_id`）。**不存題目與選項原文**。

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

/// `judge_shadow.regex_verdict`。`cleared_at` 只蓋 `limit_hit`，這一類不蓋。
pub const VERDICT: &str = "ask_option";
/// 建議要過的絕對分數。
pub const SUGGEST_THRESHOLD: f64 = 0.7;
/// 最高分要比第二名高出這麼多：兩個選項都高分＝使用者的指示沒有決定哪一個，不標。
pub const SUGGEST_MARGIN: f64 = 0.15;
/// 答案落帳之後發的 WS 事件：`{bot_id, run_id, tool_use_id}`。網頁收到就重讀 `pending-question`。
/// 發送端寫字面值（`ws_event_docs_tests` 靠字面值掃文件），測試拿這個常數對。
pub const EVENT: &str = "pending_question_hint";

/// `AskUserQuestion` 一次最多四題；多的不問。
const MAX_QUESTIONS: usize = 4;
/// 一題最多幾個選項（工具本身上限 4，留餘裕）。超過的整題不問，不是只問前幾個——少了選項分數就不可比。
const MAX_OPTIONS: usize = 8;
/// 送出去的使用者 prompt 尾段上限（字元）。
const PROMPT_CHARS: usize = 2000;
/// 遮罩前先截到這麼多（只為了別對 MB 級的 prompt 逐行遮罩）。
const PROMPT_READ_CHARS: usize = 200_000;
const QUESTION_CHARS: usize = 400;
const LABEL_CHARS: usize = 120;
const DESCRIPTION_CHARS: usize = 300;

/// 問得出建議的選項（label 與 description 原文，送出前才遮罩）。
#[derive(Debug, Clone, PartialEq)]
pub struct AskedOption {
    pub label: String,
    pub description: String,
}

/// 問得出建議的一題：單選、2–[`MAX_OPTIONS`] 個選項。`index` 是它在 `AskUserQuestion.questions` 裡的位置。
#[derive(Debug, Clone, PartialEq)]
pub struct AskedQuestion {
    pub index: usize,
    pub question: String,
    pub options: Vec<AskedOption>,
}

/// 從 `AskUserQuestion` 的 `input` 挑出問得出建議的題目。多選題不標（沒有「唯一那一個」可指）。
pub fn askable(input: &Value) -> Vec<AskedQuestion> {
    let Some(questions) = input.get("questions").and_then(Value::as_array) else { return Vec::new() };
    questions
        .iter()
        .enumerate()
        .take(MAX_QUESTIONS)
        .filter_map(|(index, q)| {
            if q.get("multiSelect").and_then(Value::as_bool).unwrap_or(false) {
                return None;
            }
            let question = q.get("question").and_then(Value::as_str)?.trim();
            let raw = q.get("options").and_then(Value::as_array)?;
            if question.is_empty() || !(2..=MAX_OPTIONS).contains(&raw.len()) {
                return None;
            }
            let options = raw
                .iter()
                .map(|o| {
                    let label = o.get("label").and_then(Value::as_str)?.trim();
                    (!label.is_empty()).then(|| AskedOption {
                        label: label.to_string(),
                        description: o.get("description").and_then(Value::as_str).unwrap_or_default().trim().to_string(),
                    })
                })
                .collect::<Option<Vec<_>>>()?;
            Some(AskedQuestion { index, question: question.to_string(), options })
        })
        .collect()
}

fn milli(x: f64) -> i64 {
    (x * 1000.0).round() as i64
}

/// 這一題有沒有建議：回建議的選項位置。比的是千分位——`0.7 - 0.55` 在浮點上是 `0.1499…`，
/// 但「差 0.15」就是差 0.15。分數同分（含兩個都是最高）一律不標。
pub fn suggest(scores: &[f64]) -> Option<usize> {
    if scores.len() < 2 {
        return None;
    }
    let (top_at, top) = scores.iter().copied().enumerate().max_by(|a, b| a.1.total_cmp(&b.1))?;
    let second = scores.iter().copied().enumerate().filter(|(i, _)| *i != top_at).map(|(_, s)| s).max_by(f64::total_cmp)?;
    (milli(top) >= milli(SUGGEST_THRESHOLD) && milli(top) - milli(second) >= milli(SUGGEST_MARGIN)).then_some(top_at)
}

fn letter(i: usize) -> char {
    (b'a' + i as u8) as char
}

/// Noul 題的 key：`q<題號>_<選項字母>`，題號從 1 起、字母從 a 起。
fn key(q: &AskedQuestion, option: usize) -> String {
    format!("q{}_{}", q.index + 1, letter(option))
}

fn clip(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn last_chars(text: &str, max: usize) -> &str {
    let n = text.chars().count();
    if n <= max {
        return text;
    }
    let from = text.char_indices().nth(n - max).map_or(0, |(i, _)| i);
    &text[from..]
}

/// 使用者 prompt 的尾段：先遮罩、再留最後 [`PROMPT_CHARS`] 字元。最新的指示在尾端；先遮再截，
/// 才不會把一個憑據從中間切開、讓遮罩認不出來。
fn prompt_tail(raw: &str) -> String {
    let masked = super::mask(last_chars(raw.trim(), PROMPT_READ_CHARS));
    last_chars(masked.trim(), PROMPT_CHARS).to_string()
}

/// 題目照 #1475 的描述：state 帶題目、各選項 label＋description 與使用者 prompt 尾段；
/// 每個選項一題 Noul，指令文字裡只有我們自己的 id，不夾使用者內容。
fn request_body(model: &str, prompt: &str, questions: &[AskedQuestion]) -> Value {
    let state_questions: Vec<Value> = questions
        .iter()
        .map(|q| {
            let options: Vec<Value> = q
                .options
                .iter()
                .enumerate()
                .map(|(i, o)| {
                    json!({
                        "id": letter(i).to_string(),
                        "label": super::mask(&clip(&o.label, LABEL_CHARS)),
                        "description": super::mask(&clip(&o.description, DESCRIPTION_CHARS)),
                    })
                })
                .collect();
            json!({"id": format!("q{}", q.index + 1), "question": super::mask(&clip(&q.question, QUESTION_CHARS)), "options": options})
        })
        .collect();
    let mut asked = serde_json::Map::new();
    for q in questions {
        for i in 0..q.options.len() {
            asked.insert(
                key(q, i),
                json!({
                    "type": "noul",
                    "instructions": {
                        "question": format!(
                            "`user_prompt` is what the user last asked this agent to do. The agent has stopped to ask the user the multiple-choice question with id `q{}` in `questions`. Would the user pick the option with id `{}` of that question?",
                            q.index + 1,
                            letter(i)
                        ),
                        "focus": "Decide only from what the user wrote in `user_prompt` and the wording of the question and its options. An option being listed first, or marked as recommended by the agent, is not evidence. If the user's words do not favour this option over the other options of the same question, answer false."
                    },
                    "criteria": {
                        "true": "The user's instructions clearly favour this option over the other options of the same question",
                        "false": "The instructions do not decide between the options, favour another option, or this option is a poor fit"
                    }
                }),
            );
        }
    }
    json!({"model": model, "state": {"user_prompt": prompt, "questions": state_questions}, "questions": Value::Object(asked)})
}

/// 每題每個選項的分數。缺一題、不是 0–1 的有限數都算整個答案壞掉：少一個選項的分數，排名就不可比。
fn parse_scores(answer: &Value, questions: &[AskedQuestion]) -> Result<Vec<Vec<f64>>> {
    questions
        .iter()
        .map(|q| {
            (0..q.options.len())
                .map(|i| {
                    answer["answers"][key(q, i)]["noul"]
                        .as_f64()
                        .filter(|p| p.is_finite() && (0.0..=1.0).contains(p))
                        .ok_or_else(|| anyhow!("no usable noul for {}", key(q, i)))
                })
                .collect()
        })
        .collect()
}

/// 帳本 `matched_line` 的終態與 API `jev_options.questions` 同形。
fn outcome(tool_use_id: &str, questions: &[AskedQuestion], scores: &[Vec<f64>]) -> Value {
    let qs: Vec<Value> = questions
        .iter()
        .zip(scores)
        .map(|(q, s)| json!({"index": q.index, "scores": s, "suggested": suggest(s)}))
        .collect();
    json!({"tool_use_id": tool_use_id, "questions": qs})
}

/// 這個 `tool_use` 在帳本上的狀態。
#[derive(Debug, PartialEq)]
pub enum Lookup {
    /// 沒問過。
    Never,
    /// 問過了（在飛、失敗、被收掉都算）：不重問，也沒有建議可帶。
    NoHint,
    /// 答案回來了：API 的 `jev_options`。
    Hint(Value),
}

pub async fn lookup(app: &impl crate::capabilities::Db, run_id: &str, tool_use_id: &str) -> Result<Lookup> {
    // 不在 SQL 裡 `json_extract`：同一個 run 的 `limit_hit` 列 `matched_line` 是純文字，SQLite 不保證 AND 的求值順序。
    let rows: Vec<(String, String, Option<String>, Option<String>)> =
        sqlx::query_as("SELECT id, matched_line, error, model FROM judge_shadow WHERE regex_verdict = ? AND run_id = ? ORDER BY at DESC, id DESC")
            .bind(VERDICT)
            .bind(run_id)
            .fetch_all(app.db())
            .await?;
    for (id, matched, error, model) in rows {
        let Ok(v) = serde_json::from_str::<Value>(&matched) else { continue };
        if v["tool_use_id"].as_str() != Some(tool_use_id) {
            continue;
        }
        if error.is_some() || !v["questions"].is_array() {
            return Ok(Lookup::NoHint);
        }
        return Ok(Lookup::Hint(json!({
            "ledger_id": id,
            "model": model,
            "threshold": SUGGEST_THRESHOLD,
            "margin": SUGGEST_MARGIN,
            "questions": v["questions"],
        })));
    }
    Ok(Lookup::Never)
}

/// 同一個 `tool_use` 同時只准一個背景 task 在問：兩次輪詢可能同時看到「帳本沒有」，
/// `reserve_slot` 的占位是在 HTTP 之前才寫，兩個 task 會各占一個名額。
static IN_FLIGHT: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

struct Flight(String);

impl Flight {
    fn take(key: String) -> Option<Self> {
        IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone()).then_some(Flight(key))
    }
}

impl Drop for Flight {
    fn drop(&mut self) {
        IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.0);
    }
}

/// 為什麼這一筆不問、不寫帳本——關著的東西不該留痕跡（同 [`super::Skip`]）。移交出去的專案歸接手的 daemon 管（#1421）。
async fn silent_gate(app: &impl crate::capabilities::Db, cfg: &crate::config::JudgeCfg, bot: &crate::db::Bot) -> bool {
    let label = crate::db::project(app.db(), &bot.project_id).await.ok().flatten().map(|p| p.label).unwrap_or_default();
    if matches!(super::gate(cfg, &bot.project_id, &label, 0), Some(super::Skip::Disabled | super::Skip::ProjectNotListed)) {
        return true;
    }
    !matches!(crate::handoff::project_handed_off_to(app.db(), &bot.project_id).await, Ok(None))
}

/// Fresh transcript check supplied by the daemon (remote reads bypass the polling cache).
pub trait PendingQuestion: Send + Sync {
    fn question_pending(&self, bot: &crate::db::Bot, run: &crate::db::Run, tool: &str)
        -> impl std::future::Future<Output = bool> + Send;
}
impl<T: PendingQuestion + ?Sized> PendingQuestion for Arc<T> {
    async fn question_pending(&self, bot: &crate::db::Bot, run: &crate::db::Run, tool: &str) -> bool {
        self.as_ref().question_pending(bot, run, tool).await
    }
}

/// `GET /bots/{id}/pending-question` 讀到一個還沒被回答的 `AskUserQuestion` 時呼叫。
///
/// 回 API 的 `jev_options`：答案已經在帳本上就帶出來，否則 `None`。**不等 Jev**：帳本沒有的話丟一個
/// 背景 task 去問就回來，答案落帳後發 [`EVENT`]。開關、專案名單、移交都在丟 task 之前先看，
/// 不符就連 task 都不起；名額與 key 在 task 裡才占（要問的時候才占，不白燒）。
pub async fn hint_for(
    app: &Arc<impl crate::capabilities::Cfg + crate::capabilities::Db + crate::capabilities::Emit + crate::judge::JudgeFuse + PendingQuestion + crate::capabilities::BgTasks + crate::capabilities::Shutdown + 'static>,
    bot: &crate::db::Bot,
    run_id: &str,
    tool_use_id: &str,
    input: &Value,
) -> Option<Value> {
    let cfg = app.cfg().get().await.judge;
    if !cfg.enabled || askable(input).is_empty() || tool_use_id.is_empty() || silent_gate(&**app, &cfg, bot).await {
        return None;
    }
    let context = current_prompt(&**app, &bot.id, run_id).await.ok().flatten()?;
    if !still_current(&**app, &cfg, &bot.id, run_id, tool_use_id, &context.0).await.ok()? { return None; }
    match lookup(&**app, run_id, tool_use_id).await {
        Ok(Lookup::Hint(v)) => {
            let matched: Option<String> = sqlx::query_scalar("SELECT matched_line FROM judge_shadow WHERE id=?")
                .bind(v["ledger_id"].as_str()?).fetch_optional(app.db()).await.ok()?;
            let stored: Value = serde_json::from_str(&matched?).ok()?;
            return (stored["turn_id"].as_str() == Some(context.0.as_str())).then_some(v);
        },
        Ok(Lookup::NoHint) | Err(_) => return None,
        Ok(Lookup::Never) => {}
    }
    if app.shutdown().is_cancelled() {
        return None;
    }
    let flight = Flight::take(format!("{run_id}:{tool_use_id}"))?;
    let (app, shutdown) = (app.clone(), app.shutdown().clone());
    let (bot_id, run_id, tool_use_id, input) = (bot.id.clone(), run_id.to_string(), tool_use_id.to_string(), input.clone());
    let tasks = app.background_tasks().clone();
    crate::background_loop::spawn_named(&tasks, "judge ask_option", async move {
        let _flight = flight;
        tokio::select! {
            _ = shutdown.cancelled() => {}
            r = ask_context(&app, &bot_id, &run_id, &tool_use_id, &input, Some(context)) => {
                if let Err(e) = r {
                    tracing::debug!(bot = %bot_id, error = %e, "judge ask_option skipped");
                }
            }
        }
    });
    None
}

/// 讀 prompt、問 Jev、記帳本、發事件。回 `Ok(true)`＝問了且答案可用。
/// 關著、專案不在名單、已移交、key 讀不到、沒有 prompt、問過了、名額沒了：靜默回 `Ok(false)`，不寫帳本（名額沒了也不寫）。
pub async fn ask(
    app: &(impl crate::capabilities::Cfg + crate::capabilities::Db + crate::capabilities::Emit + crate::judge::JudgeFuse + PendingQuestion),
    bot_id: &str,
    run_id: &str,
    tool_use_id: &str,
    input: &Value,
) -> Result<bool> {
    let context = current_prompt(app, bot_id, run_id).await?;
    ask_context(app, bot_id, run_id, tool_use_id, input, context).await
}

async fn ask_context(
    app: &(impl crate::capabilities::Cfg + crate::capabilities::Db + crate::capabilities::Emit + crate::judge::JudgeFuse + PendingQuestion),
    bot_id: &str, run_id: &str, tool_use_id: &str, input: &Value,
    context: Option<(String, String)>,
) -> Result<bool> {
    let cfg = app.cfg().get().await.judge;
    if !cfg.enabled {
        return Ok(false);
    }
    let Some(bot) = crate::db::bot(app.db(), bot_id).await? else { return Ok(false) };
    if bot.deleted_at.is_some() || silent_gate(app, &cfg, &bot).await {
        return Ok(false);
    }
    let questions = askable(input);
    if questions.is_empty() {
        return Ok(false);
    }
    // 帳本在 task 開頭再看一次：兩次輪詢之間，前一個 task 可能剛好寫完並放掉 [`Flight`]。
    if lookup(app, run_id, tool_use_id).await? != Lookup::Never {
        return Ok(false);
    }
    let Ok(key) = super::read_key(&cfg.key_file) else { return Ok(false) };
    let Some((turn_id, prompt)) = context else { return Ok(false) };
    if !still_current(app, &cfg, bot_id, run_id, tool_use_id, &turn_id).await? { return Ok(false); }
    let prompt = prompt_tail(&prompt);
    if prompt.is_empty() {
        return Ok(false);
    }
    let label = crate::db::project(app.db(), &bot.project_id).await?.map(|p| p.label).unwrap_or_default();
    let slot = match super::reserve_slot(
        app,
        &cfg,
        &bot.project_id,
        &label,
        bot_id,
        run_id,
        &bot.kind,
        &json!({"tool_use_id": tool_use_id, "turn_id": turn_id}).to_string(),
        false,
        VERDICT,
    )
    .await
    {
        Ok(id) => id,
        // 名額、開關在占位的鎖裡又判一次；不是錯誤。
        Err(e) if ["Fuse", "Disabled", "ProjectNotListed"].iter().any(|s| e.to_string() == *s) => return Ok(false),
        Err(e) => return Err(e),
    };
    if !still_current(app, &cfg, bot_id, run_id, tool_use_id, &turn_id).await? {
        sqlx::query("UPDATE judge_shadow SET error='stale' WHERE id=?").bind(&slot).execute(app.db()).await?;
        return Ok(false);
    }
    let body = request_body(&cfg.model, &prompt, &questions);
    let started = Instant::now();
    let answer = match super::post_systemone(&cfg, &key, &body).await {
        Ok(v) => parse_scores(&v, &questions).map(|s| (s, v["model"].as_str().map(str::to_string), v["usage"]["input_tokens"].as_i64())),
        Err(e) => Err(e),
    };
    let ms = started.elapsed().as_millis() as i64;
    if !still_current(app, &cfg, bot_id, run_id, tool_use_id, &turn_id).await? {
        sqlx::query("UPDATE judge_shadow SET error='stale', ms=? WHERE id=?").bind(ms).bind(&slot).execute(app.db()).await?;
        return Ok(false);
    }
    let (matched, model, tokens, error, scores) = match answer {
        Ok((scores, model, tokens)) => {
            let mut matched = outcome(tool_use_id, &questions, &scores);
            matched["turn_id"] = json!(turn_id);
            (matched.to_string(), model, tokens, None, Some(scores))
        },
        Err(e) => (json!({"tool_use_id": tool_use_id}).to_string(), None, None, Some(e.to_string()), None),
    };
    sqlx::query("UPDATE judge_shadow SET matched_line=?, model=?, ms=?, input_tokens=?, error=? WHERE id=?")
        .bind(&matched)
        .bind(model)
        .bind(ms)
        .bind(tokens)
        .bind(error)
        .bind(&slot)
        .execute(app.db())
        .await?;
    if scores.is_none() {
        return Ok(false);
    }
    if !still_current(app, &cfg, bot_id, run_id, tool_use_id, &turn_id).await? {
        sqlx::query("UPDATE judge_shadow SET matched_line=?, error='stale' WHERE id=?")
            .bind(json!({"tool_use_id": tool_use_id}).to_string()).bind(&slot).execute(app.db()).await?;
        return Ok(false);
    }
    app.emit("pending_question_hint", json!({"bot_id": bot_id, "run_id": run_id, "tool_use_id": tool_use_id})).await;
    Ok(true)
}

/// 這顆 bot 指定 run 的回合 id 與使用者 prompt：在飛的那一則，沒有就最近一則已送出的。外部回合（使用者在終端自己打的）
/// `prompt_text` 是空的，改讀那一回合的使用者訊息。
async fn current_prompt(app: &impl crate::capabilities::Db, bot_id: &str, run_id: &str) -> Result<Option<(String, String)>> {
    let text: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT t.id, COALESCE(NULLIF(TRIM(t.prompt_text), ''),
                         (SELECT m.content FROM messages m WHERE m.turn_id = t.id AND m.role = 'user' ORDER BY m.created_at ASC, m.id ASC LIMIT 1))
           FROM turns t JOIN conversations c ON c.id = t.conversation_id
          WHERE c.bot_id = ? AND t.run_id = ? AND t.status <> 'queued'
          ORDER BY (t.status = 'in_flight') DESC, t.created_at DESC, t.rowid DESC
          LIMIT 1",
    )
    .bind(bot_id)
    .bind(run_id)
    .fetch_optional(app.db())
    .await?;
    Ok(text.and_then(|(id, text)| text.filter(|t| !t.trim().is_empty()).map(|text| (id, text))))
}

async fn still_current(
    app: &(impl crate::capabilities::Db + PendingQuestion), cfg: &crate::config::JudgeCfg,
    bot_id: &str, run_id: &str, tool: &str, turn_id: &str,
) -> Result<bool> {
    let Some(bot) = crate::db::bot(app.db(), bot_id).await? else { return Ok(false) };
    let Some(run) = crate::db::active_run(app.db(), bot_id).await? else { return Ok(false) };
    if bot.deleted_at.is_some() || run.id != run_id || silent_gate(app, cfg, &bot).await { return Ok(false); }
    if current_prompt(app, bot_id, run_id).await?.as_ref().map(|c| c.0.as_str()) != Some(turn_id) { return Ok(false); }
    if !app.question_pending(&bot, &run, tool).await { return Ok(false); }
    // Transcript I/O can yield; check the DB boundary once more afterwards.
    let active = crate::db::active_run(app.db(), bot_id).await?;
    let Some(bot) = crate::db::bot(app.db(), bot_id).await? else { return Ok(false) };
    Ok(bot.deleted_at.is_none() && active.is_some_and(|r| r.id == run_id)
        && !silent_gate(app, cfg, &bot).await
        && current_prompt(app, bot_id, run_id).await?.as_ref().map(|c| c.0.as_str()) == Some(turn_id))
}

#[cfg(test)]
mod pure_tests {
    use super::*;

    fn ask_input(questions: Value) -> Value {
        json!({"questions": questions})
    }

    fn opts(n: usize) -> Value {
        Value::Array((0..n).map(|i| json!({"label": format!("選項{i}"), "description": format!("說明{i}")})).collect())
    }

    /// 門檻與差距各自要過；邊界（剛好 0.7、剛好差 0.15）算過，浮點誤差不能讓它掉下去。
    #[test]
    fn a_suggestion_needs_both_the_floor_and_the_margin() {
        assert_eq!(suggest(&[0.9, 0.2]), Some(0));
        assert_eq!(suggest(&[0.2, 0.9, 0.1]), Some(1));
        assert_eq!(suggest(&[0.7, 0.55]), Some(0), "剛好 0.7 與剛好差 0.15 都算過（0.7-0.55 在浮點上是 0.1499…）");
        assert_eq!(suggest(&[0.95, 0.8]), Some(0), "0.95-0.8 在浮點上也是 0.1499…");
        assert_eq!(suggest(&[0.69, 0.1]), None, "沒到門檻");
        assert_eq!(suggest(&[0.7, 0.56]), None, "差距不到 0.15");
        assert_eq!(suggest(&[0.85, 0.8]), None, "兩個都高分：使用者的指示沒有決定哪一個");
        assert_eq!(suggest(&[0.9, 0.9]), None, "同分不標");
        assert_eq!(suggest(&[0.4, 0.3, 0.2]), None, "全都低");
        assert_eq!(suggest(&[0.9]), None);
        assert_eq!(suggest(&[]), None);
    }

    /// 多選題、少於兩個選項、選項太多、沒有題目的都不問；題號保留原位置。
    #[test]
    fn only_single_select_questions_with_two_to_eight_options_are_asked() {
        let input = ask_input(json!([
            {"question": "多選", "multiSelect": true, "options": opts(3)},
            {"question": "單選", "multiSelect": false, "options": opts(3)},
            {"question": "只有一個", "options": opts(1)},
            {"question": "太多", "options": opts(9)},
            {"question": "  ", "options": opts(2)},
            {"question": "沒標 multiSelect 算單選", "options": opts(2)},
            {"question": "選項缺 label", "options": [{"description": "x"}, {"label": "y"}]},
        ]));
        let got = askable(&input);
        assert_eq!(got.iter().map(|q| (q.index, q.question.as_str(), q.options.len())).collect::<Vec<_>>(), [(1, "單選", 3)]);
        // MAX_QUESTIONS 之後的位置不看：第 5 題不問。
        let five = ask_input(json!([
            {"question": "多選", "multiSelect": true, "options": opts(2)}, {"question": "多選", "multiSelect": true, "options": opts(2)},
            {"question": "多選", "multiSelect": true, "options": opts(2)}, {"question": "多選", "multiSelect": true, "options": opts(2)},
            {"question": "第五題", "options": opts(2)},
        ]));
        assert!(askable(&five).is_empty());
        assert!(askable(&json!({})).is_empty());
    }

    /// 一個請求：每個選項一題 Noul；state 只有題目、選項與 prompt 尾段（已遮罩），指令文字不夾使用者內容。
    #[test]
    fn the_request_is_one_noul_per_option_with_masked_state() {
        let q = AskedQuestion {
            index: 1,
            question: "結果寄給 someone@example.com 嗎？".into(),
            options: vec![
                AskedOption { label: "內網".into(), description: "DB_PASSWORD=hunter2".into() },
                AskedOption { label: "公網".into(), description: "".into() },
            ],
        };
        let body = request_body("jev-1.13.0", "請走內網", std::slice::from_ref(&q));
        assert_eq!(body["model"], "jev-1.13.0");
        let mut keys: Vec<_> = body["questions"].as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["q2_a", "q2_b"], "題號從 1 起、保留原位置；字母從 a 起");
        assert_eq!(body["questions"]["q2_a"]["type"], "noul");
        let state = body["state"].as_object().unwrap();
        let mut state_keys: Vec<_> = state.keys().cloned().collect();
        state_keys.sort();
        assert_eq!(state_keys, ["questions", "user_prompt"]);
        assert_eq!(state["user_prompt"], "請走內網");
        assert_eq!(state["questions"][0]["id"], "q2");
        assert_eq!(state["questions"][0]["options"][0]["id"], "a");
        assert_eq!(state["questions"][0]["options"][0]["label"], "內網");
        let all = body.to_string();
        assert!(!all.contains("someone@example.com") && !all.contains("hunter2"), "題目與選項走既有的遮罩：{all}");
        let instruction = body["questions"]["q2_b"]["instructions"]["question"].as_str().unwrap();
        assert!(instruction.contains("`q2`") && instruction.contains("`b`") && !instruction.contains("公網"));
    }

    /// 尾段：先遮罩再留最後 2000 字元（最新的指示在尾端）。
    #[test]
    fn the_prompt_keeps_the_masked_tail() {
        let long = format!("{}\nAuthorization: Bearer abc.def.ghi\n最後的指示：選內網", "舊".repeat(3000));
        let got = prompt_tail(&long);
        assert_eq!(got.chars().count(), PROMPT_CHARS);
        assert!(got.ends_with("最後的指示：選內網") && !got.contains("abc.def.ghi"), "{got}");
        assert_eq!(prompt_tail("  短的  "), "短的");
        assert_eq!(prompt_tail("   "), "");
    }

    /// 缺一題、超出 0–1、不是數字：整個答案壞掉，不拿剩下的湊。
    #[test]
    fn a_missing_or_out_of_range_answer_spoils_the_whole_response() {
        let q = AskedQuestion { index: 0, question: "q".into(), options: vec![AskedOption { label: "a".into(), description: String::new() }; 2] };
        let ok = json!({"answers": {"q1_a": {"noul": 0.9}, "q1_b": {"noul": 0.1}}});
        assert_eq!(parse_scores(&ok, std::slice::from_ref(&q)).unwrap(), [[0.9, 0.1]]);
        for bad in [
            json!({"answers": {"q1_a": {"noul": 0.9}}}),
            json!({"answers": {"q1_a": {"noul": 1.2}, "q1_b": {"noul": 0.1}}}),
            json!({"answers": {"q1_a": {"noul": -0.1}, "q1_b": {"noul": 0.1}}}),
            json!({"answers": {"q1_a": {"noul": "high"}, "q1_b": {"noul": 0.1}}}),
            json!({}),
        ] {
            assert!(parse_scores(&bad, std::slice::from_ref(&q)).is_err(), "{bad}");
        }
    }
}

#[cfg(all(test, feature = "daemon-test-harness"))]
mod tests {
    use super::*;
    use crate::db;
    use crate::state::App;
    use crate::testing as tt;
    use axum::extract::{Path, State};
    use axum::http::StatusCode;
    use std::collections::HashMap;

    #[derive(Clone)]
    enum Mode {
        /// 依 key 回分數，沒列的 0.05。
        Scores(HashMap<String, f64>),
        Status(u16),
        Gate(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>),
    }

    async fn fake_jev(mode: Mode) -> (String, Arc<Mutex<Vec<Value>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let route = axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            let (log, mode) = (log.clone(), mode.clone());
            async move {
                log.lock().unwrap().push(body.clone());
                match mode {
                    Mode::Gate(entered, release) => {
                        entered.notify_one();
                        release.notified().await;
                        (StatusCode::OK, axum::Json(json!({"answers": {"q1_a": {"noul": 0.95}, "q1_b": {"noul": 0.05}}})))
                    }
                    Mode::Status(code) => (StatusCode::from_u16(code).unwrap(), axum::Json(json!({"error": "no"}))),
                    Mode::Scores(by_key) => {
                        let mut answers = serde_json::Map::new();
                        for k in body["questions"].as_object().unwrap().keys() {
                            answers.insert(k.clone(), json!({"type": "noul", "noul": by_key.get(k).copied().unwrap_or(0.05)}));
                        }
                        (StatusCode::OK, axum::Json(json!({"model": "jev-1.13.0", "answers": answers, "usage": {"input_tokens": 321}})))
                    }
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, axum::Router::new().route("/v1/systemone", route)).await.unwrap() });
        (url, seen)
    }

    struct Rig {
        env: tt::Env,
        dir: std::path::PathBuf,
        seen: Arc<Mutex<Vec<Value>>>,
        bot: db::Bot,
        run_id: String,
        transcript: std::path::PathBuf,
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    fn scores(pairs: &[(&str, f64)]) -> Mode {
        Mode::Scores(pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect())
    }

    /// 開著、專案在名單、有 key、一顆 claude bot 有 run 與 transcript、一則在飛的使用者 prompt。
    async fn stand(mode: Mode, enabled: bool, listed: bool) -> Rig {
        let env = tt::env().await;
        let app = env.app.clone();
        let (url, seen) = fake_jev(mode).await;
        let dir = tt::track(std::env::temp_dir().join(format!("am-judge-ask-option-{}", db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("key");
        std::fs::write(&key, "k-test\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let pid = if listed { env.project_id.clone() } else { "not-this-project".into() };
        let key_s = key.to_string_lossy().into_owned();
        app.cfg
            .update(move |c| {
                c.judge.enabled = enabled;
                c.judge.projects = vec![pid];
                c.judge.key_file = key_s;
                c.judge.endpoint = url;
                c.judge.timeout_ms = 2000;
                Ok(())
            })
            .await
            .unwrap();
        let bot = tt::claude_bot(&app, &env.project_id, "asker").await;
        let cfg_dir = dir.join("claude");
        sqlx::query("UPDATE bots SET env_json=? WHERE id=?")
            .bind(json!({"CLAUDE_CONFIG_DIR": cfg_dir.to_string_lossy()}).to_string())
            .bind(&bot.id)
            .execute(&app.db)
            .await
            .unwrap();
        let run_id = tt::fake_run(&app, &bot.id).await;
        let transcript = cfg_dir.join("projects/-work/s.jsonl");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        sqlx::query("UPDATE runs SET transcript_path=? WHERE id=?").bind(transcript.to_string_lossy()).bind(&run_id).execute(&app.db).await.unwrap();
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,?,'web','in_flight','ok',?,?)")
            .bind(db::ulid())
            .bind(&conv)
            .bind(&run_id)
            .bind("部署 prod console，連線一律走內網，不要開公網。 GITHUB_TOKEN=ghp_abcdefghijklmnopqrstuvwxyz0123456789")
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let bot = db::bot(&app.db, &bot.id).await.unwrap().unwrap();
        Rig { env, dir, seen, bot, run_id, transcript }
    }

    fn two_options() -> Value {
        json!([{"question": "連線方式？", "header": "連線", "multiSelect": false,
                "options": [{"label": "內網 http", "description": "流量留在 VPC"}, {"label": "公網 https", "description": "拿掉白名單"}]}])
    }

    fn write_ask(rig: &Rig, id: &str, questions: Value) {
        let line = json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": id, "name": "AskUserQuestion", "input": {"questions": questions}}]}});
        std::fs::write(&rig.transcript, format!("{line}\n")).unwrap();
    }

    async fn get(rig: &Rig) -> Value {
        let axum::Json(body) = crate::runners::pending_question::get_pending_question(State(rig.env.app.clone()), Path(rig.bot.id.clone())).await.unwrap();
        body
    }

    async fn ledger(app: &Arc<App>) -> Vec<(String, Option<String>, Option<f64>)> {
        sqlx::query_as("SELECT matched_line, error, jev_is_live_ui FROM judge_shadow WHERE regex_verdict='ask_option' ORDER BY at, id")
            .fetch_all(&app.db)
            .await
            .unwrap()
    }

    /// 等答案落帳那則事件（不靠 sleep）。
    async fn hint_event(rx: &mut tokio::sync::broadcast::Receiver<crate::state::WsEvent>) -> Value {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let ev = rx.recv().await.unwrap();
                if ev.kind == EVENT {
                    return ev.data;
                }
            }
        })
        .await
        .expect("答案落帳後要發事件")
    }

    #[tokio::test]
    async fn a_stored_hint_is_not_served_to_a_later_turn() {
        let rig = stand(scores(&[("q1_a", 0.95)]), true, true).await;
        write_ask(&rig, "toolu_1", two_options());
        assert!(ask(&rig.env.app, &rig.bot.id, &rig.run_id, "toolu_1", &json!({"questions": two_options()})).await.unwrap());
        let conv = db::conversation_id(&rig.env.app.db, &rig.bot.id).await.unwrap();
        sqlx::query("UPDATE turns SET status='completed'").execute(&rig.env.app.db).await.unwrap();
        sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,prompt_text,created_at) VALUES (?,?,?,'web','in_flight','ok','LATER PROMPT','9999')")
            .bind(db::ulid()).bind(conv).bind(&rig.run_id).execute(&rig.env.app.db).await.unwrap();
        assert_eq!(get(&rig).await["jev_options"], Value::Null);
        assert_eq!(rig.seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_background_question_cannot_follow_a_later_turn_before_send() {
        let rig = stand(scores(&[("q1_a", 0.95)]), true, true).await;
        write_ask(&rig, "toolu_1", two_options());
        let gate = rig.env.app.judge_fuse.lock().await;
        assert_eq!(get(&rig).await["jev_options"], Value::Null);
        rig.env.app.background_tasks.close();
        sqlx::query("UPDATE turns SET status='completed'").execute(&rig.env.app.db).await.unwrap();
        let conv = db::conversation_id(&rig.env.app.db, &rig.bot.id).await.unwrap();
        sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,prompt_text,created_at) VALUES (?,?,?,'web','in_flight','ok','LATER PROMPT','9999')")
            .bind(db::ulid()).bind(conv).bind(&rig.run_id).execute(&rig.env.app.db).await.unwrap();
        drop(gate);
        rig.env.app.background_tasks.wait().await;
        assert!(rig.seen.lock().unwrap().is_empty(), "later turn must not be sent for the old question");
        assert!(matches!(lookup(&rig.env.app, &rig.run_id, "toolu_1").await.unwrap(), Lookup::Never | Lookup::NoHint));
        for row in ledger(&rig.env.app).await { assert_eq!(row.1.as_deref(), Some("stale")); }
    }

    #[tokio::test]
    async fn an_old_run_never_borrows_the_new_runs_prompt() {
        let rig = stand(scores(&[("q1_a", 0.95)]), true, true).await;
        write_ask(&rig, "toolu_1", two_options());
        sqlx::query("UPDATE runs SET state='exited', ended_at='ended' WHERE id=?").bind(&rig.run_id).execute(&rig.env.app.db).await.unwrap();
        let next = tt::fake_run(&rig.env.app, &rig.bot.id).await;
        let conv = db::conversation_id(&rig.env.app.db, &rig.bot.id).await.unwrap();
        sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,prompt_text,created_at) VALUES (?,?,?,'web','in_flight','ok','R2 PROMPT','9999')")
            .bind(db::ulid()).bind(conv).bind(next).execute(&rig.env.app.db).await.unwrap();
        assert!(!ask(&rig.env.app, &rig.bot.id, &rig.run_id, "toolu_1", &json!({"questions": two_options()})).await.unwrap());
        assert!(rig.seen.lock().unwrap().is_empty());
        assert!(ledger(&rig.env.app).await.is_empty());
    }

    #[tokio::test]
    async fn stale_questions_never_publish_after_the_http_gate() {
        for boundary in ["answered", "turn", "run", "handoff"] {
            let entered = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Notify::new());
            let rig = stand(Mode::Gate(entered.clone(), release.clone()), true, true).await;
            write_ask(&rig, "toolu_1", two_options());
            let mut rx = rig.env.app.subscribe();
            let app = rig.env.app.clone();
            let bot = rig.bot.id.clone();
            let run = rig.run_id.clone();
            let task = tokio::spawn(Box::pin(async move {
                ask(&app, &bot, &run, "toolu_1", &json!({"questions": two_options()})).await.unwrap()
            }));
            entered.notified().await;
            match boundary {
                "answered" => {
                    let mut log = std::fs::read_to_string(&rig.transcript).unwrap();
                    log.push_str(&json!({"message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1"}]}}).to_string());
                    std::fs::write(&rig.transcript, log).unwrap();
                }
                "turn" => {
                    sqlx::query("UPDATE turns SET status='completed'").execute(&rig.env.app.db).await.unwrap();
                    let conv = db::conversation_id(&rig.env.app.db, &rig.bot.id).await.unwrap();
                    sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,prompt_text,created_at) VALUES (?,?,?,'web','in_flight','ok','NEW PROMPT','9999')")
                        .bind(db::ulid()).bind(conv).bind(&rig.run_id).execute(&rig.env.app.db).await.unwrap();
                }
                "run" => {
                    sqlx::query("UPDATE runs SET state='exited', ended_at='ended' WHERE id=?").bind(&rig.run_id).execute(&rig.env.app.db).await.unwrap();
                    tt::fake_run(&rig.env.app, &rig.bot.id).await;
                }
                "handoff" => { sqlx::query("UPDATE projects SET handed_off_to='remote' WHERE id=?").bind(&rig.env.project_id).execute(&rig.env.app.db).await.unwrap(); }
                _ => unreachable!(),
            }
            release.notify_one();
            assert!(!task.await.unwrap(), "{boundary}: stale answer must be rejected");
            assert_eq!(lookup(&rig.env.app, &rig.run_id, "toolu_1").await.unwrap(), Lookup::NoHint, "{boundary}");
            assert_eq!(ledger(&rig.env.app).await[0].1.as_deref(), Some("stale"), "{boundary}");
            assert!(rig.seen.lock().unwrap()[0]["state"]["user_prompt"].as_str().unwrap().contains("連線一律走內網"));
            while let Ok(event) = rx.try_recv() { assert_ne!(event.kind, EVENT, "{boundary}"); }
        }
    }

    /// 有明顯建議：第一次讀不等 Jev（`jev_options` 為 null、題目照給）、答案回來發事件、之後的讀帶出建議選項與分數；
    /// 同一個 `tool_use` 不再問；請求是一個、每個選項一題；prompt 與題目已遮罩、不帶 bot 識別；不按任何鍵。
    #[tokio::test]
    async fn a_clear_favourite_is_served_by_the_api_after_one_background_request() {
        let rig = stand(scores(&[("q1_a", 0.91), ("q1_b", 0.08)]), true, true).await;
        let mut rx = rig.env.app.subscribe();
        write_ask(&rig, "toolu_1", two_options());

        let first = get(&rig).await;
        assert_eq!(first["questions"][0]["question"], "連線方式？", "題目照常顯示，不等 Jev");
        assert_eq!(first["jev_options"], Value::Null, "第一次讀的時候答案還沒回來");
        let ev = hint_event(&mut rx).await;
        assert_eq!(ev, json!({"bot_id": rig.bot.id, "run_id": rig.run_id, "tool_use_id": "toolu_1"}));

        let second = get(&rig).await;
        assert_eq!(second["questions"], first["questions"]);
        let hint = &second["jev_options"];
        assert_eq!(hint["threshold"], 0.7);
        assert_eq!(hint["margin"], 0.15);
        assert_eq!(hint["model"], "jev-1.13.0");
        assert_eq!(hint["questions"], json!([{"index": 0, "scores": [0.91, 0.08], "suggested": 0}]));
        let _ = get(&rig).await;

        let sent = rig.seen.lock().unwrap().clone();
        assert_eq!(sent.len(), 1, "同一個 tool_use 只問一次");
        let mut keys: Vec<_> = sent[0]["questions"].as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["q1_a", "q1_b"], "一個請求、每個選項一題");
        let text = sent[0].to_string();
        assert!(text.contains("連線一律走內網") && text.contains("流量留在 VPC"), "prompt 與選項說明有送出：{text}");
        assert!(!text.contains("ghp_abcdefghijklmnopqrstuvwxyz0123456789"), "prompt 走既有的遮罩");
        assert!(!text.contains(&rig.bot.id) && !text.contains(&rig.env.project_id), "不送 bot／專案識別");

        let rows = ledger(&rig.env.app).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, None, "沒有錯誤");
        assert_eq!(rows[0].2, None, "不寫進 jev_is_live_ui");
        let stored: Value = serde_json::from_str(&rows[0].0).unwrap();
        assert_eq!(stored["tool_use_id"], "toolu_1");
        assert_eq!(stored["questions"][0]["suggested"], 0);
        assert!(!rows[0].0.contains("內網") && !rows[0].0.contains("連線方式"), "帳本不存題目與選項原文");

        // 只提示：不按鍵、不送字、不新增任何回合。
        assert!(rig.env.herdr.calls_to("pane.send_keys").is_empty() && rig.env.herdr.calls_to("pane.send_text").is_empty());
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns").fetch_one(&rig.env.app.db).await.unwrap();
        assert_eq!(turns, 1, "沒有替使用者送出任何東西");
    }

    /// 分數接近或低於門檻：分數照存、不標建議。
    #[tokio::test]
    async fn close_or_low_scores_carry_no_suggestion() {
        for (a, b) in [(0.85, 0.80), (0.6, 0.1), (0.7, 0.56)] {
            let rig = stand(scores(&[("q1_a", a), ("q1_b", b)]), true, true).await;
            write_ask(&rig, "toolu_1", two_options());
            assert!(ask(&rig.env.app, &rig.bot.id, &rig.run_id, "toolu_1", &json!({"questions": two_options()})).await.unwrap());
            let body = get(&rig).await;
            assert_eq!(body["jev_options"]["questions"], json!([{"index": 0, "scores": [a, b], "suggested": Value::Null}]), "{a}/{b}");
            assert_eq!(body["questions"][0]["options"].as_array().unwrap().len(), 2);
        }
    }

    /// Jev 失敗（5xx、答案缺題）：問題照常顯示、`jev_options` 是 null、帳本記 error、不重試。
    #[tokio::test]
    async fn a_failing_jev_leaves_the_question_untouched_and_is_not_retried() {
        let rig = stand(Mode::Status(503), true, true).await;
        write_ask(&rig, "toolu_1", two_options());
        let input = json!({"questions": two_options()});
        assert!(!ask(&rig.env.app, &rig.bot.id, &rig.run_id, "toolu_1", &input).await.unwrap());
        let body = get(&rig).await;
        assert_eq!(body["questions"][0]["question"], "連線方式？");
        assert_eq!(body["jev_options"], Value::Null);
        let rows = ledger(&rig.env.app).await;
        assert_eq!(rows.len(), 1);
        assert!(rows[0].1.as_deref().is_some_and(|e| e.contains("503")), "{:?}", rows[0]);
        // 再讀、再 ask 都不重問。
        let _ = get(&rig).await;
        assert!(!ask(&rig.env.app, &rig.bot.id, &rig.run_id, "toolu_1", &input).await.unwrap());
        assert_eq!(rig.seen.lock().unwrap().len(), 1);
        assert_eq!(ledger(&rig.env.app).await.len(), 1);
    }

    /// 關著、專案不在名單、專案已移交：零請求、零帳本，題目照常顯示。
    #[tokio::test]
    async fn disabled_unlisted_and_handed_off_projects_send_nothing() {
        let input = json!({"questions": two_options()});
        for case in ["disabled", "unlisted", "handed_off"] {
            let rig = stand(scores(&[("q1_a", 0.95)]), case != "disabled", case != "unlisted").await;
            if case == "handed_off" {
                sqlx::query("UPDATE projects SET handed_off_to='remote' WHERE id=?").bind(&rig.env.project_id).execute(&rig.env.app.db).await.unwrap();
            }
            write_ask(&rig, "toolu_1", two_options());
            let body = get(&rig).await;
            assert_eq!(body["questions"][0]["question"], "連線方式？", "{case}");
            assert_eq!(body["jev_options"], Value::Null, "{case}");
            assert!(!ask(&rig.env.app, &rig.bot.id, &rig.run_id, "toolu_1", &input).await.unwrap(), "{case}");
            assert!(rig.seen.lock().unwrap().is_empty(), "{case}: 不發請求");
            assert!(ledger(&rig.env.app).await.is_empty(), "{case}: 不寫帳本");
        }
    }

    /// 每小時名額（`reserve_slot` 的保險絲）用完：不問、不寫帳本、題目照常顯示。
    #[tokio::test]
    async fn an_exhausted_hourly_fuse_means_no_request() {
        let rig = stand(scores(&[("q1_a", 0.95)]), true, true).await;
        rig.env.app.cfg.update(|c| {
            c.judge.max_per_hour = 1;
            Ok(())
        }).await.unwrap();
        // 別的場景已經占滿這一小時的名額。
        sqlx::query("INSERT INTO judge_shadow (id, at, bot_id, run_id, kind, matched_line, composer_idle, regex_verdict, error) VALUES (?,?,?,?,?,?,0,'limit_hit','boom')")
            .bind(db::ulid())
            .bind(db::now())
            .bind(&rig.bot.id)
            .bind("other-run")
            .bind("claude")
            .bind("limit")
            .execute(&rig.env.app.db)
            .await
            .unwrap();
        write_ask(&rig, "toolu_1", two_options());
        let input = json!({"questions": two_options()});
        assert!(!ask(&rig.env.app, &rig.bot.id, &rig.run_id, "toolu_1", &input).await.unwrap());
        assert!(rig.seen.lock().unwrap().is_empty());
        assert!(ledger(&rig.env.app).await.is_empty());
    }

    /// 多選題不問；單選題與多選題同一次提問時，只問單選那題（題號保留原位置）。
    #[tokio::test]
    async fn multi_select_questions_are_not_asked_and_do_not_shift_the_numbering() {
        let rig = stand(scores(&[("q2_b", 0.9)]), true, true).await;
        let only_multi = json!([{"question": "要哪些？", "multiSelect": true, "options": [{"label": "A"}, {"label": "B"}]}]);
        write_ask(&rig, "toolu_m", only_multi.clone());
        assert_eq!(get(&rig).await["jev_options"], Value::Null);
        assert!(rig.seen.lock().unwrap().is_empty(), "只有多選題：不發請求");

        let mixed = json!([
            {"question": "要哪些？", "multiSelect": true, "options": [{"label": "A"}, {"label": "B"}]},
            {"question": "連線方式？", "multiSelect": false, "options": [{"label": "內網"}, {"label": "公網"}]},
        ]);
        write_ask(&rig, "toolu_x", mixed.clone());
        assert!(ask(&rig.env.app, &rig.bot.id, &rig.run_id, "toolu_x", &json!({"questions": mixed})).await.unwrap());
        let body = get(&rig).await;
        assert_eq!(body["jev_options"]["questions"], json!([{"index": 1, "scores": [0.05, 0.9], "suggested": 1}]));
        let sent = rig.seen.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        let mut keys: Vec<_> = sent[0]["questions"].as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["q2_a", "q2_b"]);
    }

    /// 同一題換一次提問（另一個 tool_use）要重問；上一次的建議不能掛到新的這一次。
    #[tokio::test]
    async fn a_new_tool_use_is_asked_again() {
        let rig = stand(scores(&[("q1_a", 0.9)]), true, true).await;
        write_ask(&rig, "toolu_1", two_options());
        let input = json!({"questions": two_options()});
        assert!(ask(&rig.env.app, &rig.bot.id, &rig.run_id, "toolu_1", &input).await.unwrap());
        write_ask(&rig, "toolu_2", two_options());
        assert_eq!(lookup(&rig.env.app, &rig.run_id, "toolu_2").await.unwrap(), Lookup::Never, "新的提問還沒有答案，上一次的不能掛過來");
        assert!(ask(&rig.env.app, &rig.bot.id, &rig.run_id, "toolu_2", &input).await.unwrap());
        assert_eq!(rig.seen.lock().unwrap().len(), 2);
        assert_eq!(ledger(&rig.env.app).await.len(), 2);
    }

    /// 沒有使用者 prompt 可依據：不問（Jev 沒有東西可判斷，白送一次外送）。
    #[tokio::test]
    async fn no_prompt_to_go_by_means_no_request() {
        let rig = stand(scores(&[("q1_a", 0.9)]), true, true).await;
        sqlx::query("DELETE FROM turns").execute(&rig.env.app.db).await.unwrap();
        write_ask(&rig, "toolu_1", two_options());
        let input = json!({"questions": two_options()});
        assert!(!ask(&rig.env.app, &rig.bot.id, &rig.run_id, "toolu_1", &input).await.unwrap());
        assert!(rig.seen.lock().unwrap().is_empty());
        assert!(ledger(&rig.env.app).await.is_empty());
    }
}
