//! 忙到一半被重啟的 claude，接回來之後補一句續行提示（claude 2.1.281）。
//!
//! 2.1.281 的 changelog：「Fixed resuming a session that ended during a tool call: Claude now sees the call and is told its
//! outcome is unknown, and a manual resume no longer adds a hidden "Continue" message」。以前 `--resume` 一段停在工具中間的
//! 對話，CLI 自己補一句隱藏的 Continue、agent 接著做；現在不補了——接回來的 bot 停在輸入框，等一句永遠不會來的話。
//!
//! 所以由 daemon 補，但只在**確定被砍在工具中途**時補一次（#424 使用者裁示丙，疊在 #430 的條件上）：
//! - **停在等人的畫面不催**：`blocked`＝確認框、問卷、額度選單。那一步只有人能決定（#423：不准自動按），叫模型
//!   「接著把原本的工作做完」等於替人做了那個決定。
//! - **不是外力收掉的不催**：上一回合是被 API 錯誤收掉的（撞額度等，`runs.turn_error`）、或使用者剛從網頁中斷過
//!   （`interrupt_grace` 記的接管時刻落在這個 run 裡）——那是有人或有原因叫它停的，不替它續做。
//! - **尾巴要是被砍的工具**（[`ends_mid_tool`]）：要接回的 transcript 結尾是沒有結果的 `tool_use`（herdr 砍 pane），
//!   或是 `tool_result`（daemon／SIGTERM，實測 exit 137）且其後沒有模型回覆。正常的模型回覆、使用者打的字（含
//!   `[Request interrupted by user…]`）、空檔、讀不到的，都不補。檔案照這顆 bot 實際帳號的 `CLAUDE_CONFIG_DIR` 找
//!   （[`account_transcript`]）：換過身分的話，CLI 接著寫的是複製進新帳號那一份，舊帳號那份停在接回之前。
//! - **啟動當下不佔 queued 槽**：只記在記憶體裡（[`arm`]）。`resume_outcome = verified` 且畫面 `idle` 之後再等
//!   [`IDLE_WAIT`]；期間仍是同一個 run、仍 idle、沒有回合在飛、佇列一直是空的、這個 run 上沒有送過別的回合，
//!   時間到才排一則（[`check_at`]）。這段期間變成 `working`／`blocked`、有人排了派工、接回不是 `verified`、使用者中斷，
//!   這一輪就取消。AGM 或使用者的派工因此可以先佔，不會跟續行提示做成兩回合。
//!
//! 「重啟前在忙」有兩種讀法：
//! - **重啟**（`restart_bot_with`：`?resume=native` restart、換身分）：停之前在鎖裡讀（[`busy_before_restart`]）。
//!   一鍵重啟要求閒置（`require_idle`），不會遇到。
//! - **啟動**（herdr 整個重啟後 `start?resume=native`）：舊 run 已經被 pane 消失收成 `exited`，看它最後記的
//!   `agent_status`（[`prior_run_ended_busy`]）。pane 是被外力收掉的，最後一次狀態就是當時的狀態。使用者自己停的
//!   （`stopped`）不算：停下來是他要的，接回來不替他續做。「上一個 run」用 `rowid` 挑：`started_at` 字串精度不一、
//!   ulid 同毫秒不單調，都排不出真正最後那個（#101、#100）。
//!
//! 不補的：codex／grok（沒有這個改變）；接不回、退回開新對話的（沒有「原本的工作」可續）；遠端主機的 bot
//! （transcript 在那台機器上，這裡讀不到尾巴，跟 `stuck_turns` 一樣只讀本機）。同一個 run 只補一次
//! （`client_request_id = resume-nudge:<run_id>`）。等待記在這個行程的記憶體裡，daemon 在這段時間重啟就不補。

use super::*;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// 送進 bot 的續行提示。
pub(crate) const NUDGE_TEXT: &str = "[來自 AG Man daemon] 重啟前你正在進行的工作被中斷了（重啟當下可能有工具還在跑，結果不明）。\
請先確認上一個工具的實際結果，再接著把原本的工作做完。";

/// 續行提示的 `client_request_id` 前綴，後面接排它的那個 run。
pub(crate) const CRID_PREFIX: &str = "resume-nudge:";

/// 接回驗證過、畫面閒置之後，還要再空著這麼久才送（#424 裁示）。
pub(crate) const IDLE_WAIT: Duration = Duration::from_secs(10);

/// 純規則：這次啟動要不要進候選。`busy` 是重啟前讀到的忙碌理由（`None`＝閒著）。
/// `blocked`（在等人按確認框／答問卷）不算：那一步只有人能決定（#423、#430）。
pub(crate) fn wants_nudge(kind: &str, resume_native: bool, busy: Option<&str>) -> bool {
    kind == "claude" && resume_native && matches!(busy, Some("working" | "turn_in_flight"))
}

/// 純規則：claude transcript 的尾巴是不是被砍在工具中途。從後往前找最後一則對話：模型的 `tool_use`（後面沒有結果）、
/// 或 `tool_result`（後面沒有模型回覆）＝是；模型的文字回覆、使用者打的字（含中斷標記、`<pasted_content>`）＝不是。
/// 系統列、`isMeta`、只有 thinking 的模型列略過；讀尾巴切到一半的第一行解析不了，也略過。空檔＝不是。
/// 2.1.281 接回時替沒有結果的呼叫補的「[Tool call interrupted: … outcome is unknown …]」本身就是 `tool_result`，照樣算。
pub(crate) fn ends_mid_tool(log: &str) -> bool {
    for line in log.lines().rev() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let content = v.pointer("/message/content");
        let has = |kind: &str| {
            content.and_then(Value::as_array).is_some_and(|parts| parts.iter().any(|p| p.get("type").and_then(Value::as_str) == Some(kind)))
        };
        match v.get("type").and_then(Value::as_str) {
            Some("assistant") if has("tool_use") => return true,
            Some("assistant") if has("text") || content.is_some_and(Value::is_string) => return false,
            Some("user") if has("tool_result") => return true,
            Some("user") if v.get("isMeta").and_then(Value::as_bool) != Some(true) => return false,
            _ => {}
        }
    }
    false
}

/// 上一個 run 是**有人或有原因**叫它停的：回原因（只進 log），不補。`started_at` 是那個 run 的開始時間。
fn stopped_on_purpose(bot_id: &str, started_at: &str, turn_error: Option<&str>) -> Option<&'static str> {
    if turn_error.is_some_and(|e| !e.trim().is_empty()) {
        return Some("the last turn was cut short by an API error (quota or otherwise)");
    }
    let started = chrono::DateTime::parse_from_rfc3339(started_at).ok()?.with_timezone(&chrono::Utc);
    super::interrupt_grace::hold_of(bot_id).filter(|at| *at >= started).map(|_| "the user interrupted this run")
}

/// 重啟的那一半：停之前讀，呼叫端持 bot 鎖。讀不到就不補（寧可少補一句，也不在不知道的時候對 bot 說它被中斷了）。
pub(crate) async fn busy_before_restart(app: &Arc<App>, bot: &db::Bot, opts: &StartOpts) -> Option<&'static str> {
    if !wants_nudge(&bot.kind, opts.resume_native, Some("working")) {
        return None;
    }
    let run = match db::active_run(&app.db, &bot.id).await {
        Ok(Some(run)) => run,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!(bot = %bot.name, error = %e, "could not read the run before the restart; no resume nudge");
            return None;
        }
    };
    let busy = match run.agent_status.as_str() {
        "working" => Some("working"),
        // 在等人：不催，也不再看有沒有回合沒收（那一回合就是卡在等人）。
        "blocked" => None,
        _ => match db::in_flight_turn(&app.db, &run.id).await {
            Ok(t) => t.map(|_| "turn_in_flight"),
            Err(e) => {
                tracing::warn!(bot = %bot.name, error = %e, "could not read the in-flight turn before the restart; no resume nudge");
                None
            }
        },
    };
    let busy = busy.filter(|b| wants_nudge(&bot.kind, opts.resume_native, Some(b)))?;
    if let Some(why) = stopped_on_purpose(&bot.id, &run.started_at, run.turn_error.as_deref()) {
        tracing::info!(bot = %bot.name, run = %run.id, busy, why, "busy before the restart, but it was stopped on purpose; no resume nudge");
        return None;
    }
    Some(busy)
}

/// 啟動的那一半：在新 run 寫進去之前讀這顆 bot 的上一個 run。只認被外力收掉的（`exited`），最後記的是 working、
/// 而且不是有人或有原因叫它停的（[`stopped_on_purpose`]）。
pub(crate) async fn prior_run_ended_busy(app: &Arc<App>, bot: &db::Bot, opts: &StartOpts) -> Option<&'static str> {
    if !wants_nudge(&bot.kind, opts.resume_native, Some("working")) {
        return None;
    }
    // 最後寫進去的那一列＝最後一個 run：`rowid` 單調，`started_at`（精度不一）與 ulid（同毫秒不單調）都不是（#430）。
    let last: Option<(String, String, String, Option<String>)> =
        match sqlx::query_as("SELECT state, agent_status, started_at, turn_error FROM runs WHERE bot_id = ? ORDER BY rowid DESC LIMIT 1")
            .bind(&bot.id)
            .fetch_optional(&app.db)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(bot = %bot.name, error = %e, "could not read the previous run; no resume nudge");
                return None;
            }
        };
    let (state, status, started_at, turn_error) = last?;
    if state != "exited" || status != "working" {
        return None;
    }
    if let Some(why) = stopped_on_purpose(&bot.id, &started_at, turn_error.as_deref()) {
        tracing::info!(bot = %bot.name, why, "the previous run died mid-work, but it was stopped on purpose; no resume nudge");
        return None;
    }
    Some("working")
}

/// 記著的候選：排它的那個 run，與第一次看到「接回驗過、閒置、佇列空」的時刻（從這一刻起等 [`IDLE_WAIT`]）。
struct Armed {
    run_id: String,
    idle_since: Option<Instant>,
}

fn armed() -> &'static Mutex<HashMap<String, Armed>> {
    static ARMED: OnceLock<Mutex<HashMap<String, Armed>>> = OnceLock::new();
    ARMED.get_or_init(Default::default)
}

fn armed_run(bot_id: &str) -> Option<String> {
    armed().lock().unwrap_or_else(|e| e.into_inner()).get(bot_id).map(|a| a.run_id.clone())
}

fn disarm(bot_id: &str, run_id: &str) {
    let mut g = armed().lock().unwrap_or_else(|e| e.into_inner());
    if g.get(bot_id).is_some_and(|a| a.run_id == run_id) {
        g.remove(bot_id);
    }
}

/// 新 run 起來之後記成候選，**不排佇列**。只在真的帶了 `--resume` 時記（接不回、開了新對話就沒有原本的工作）。
/// 呼叫端持 bot 鎖，而且已經用 [`busy_before_restart`]／[`prior_run_ended_busy`] 濾過。
pub(crate) async fn arm(app: &Arc<App>, bot: &db::Bot, run_id: &str, busy: &str) {
    let resumed: Option<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT resume_session_id, resume_outcome FROM runs WHERE id = ?").bind(run_id).fetch_optional(&app.db).await.ok().flatten();
    let resumed = match resumed {
        Some((Some(_), _)) => true,
        Some((None, Some(o))) => o == "verified",
        _ => false,
    };
    if !resumed {
        tracing::info!(bot = %bot.name, run = run_id, busy, "busy before the restart, but this start did not resume the old session; no resume nudge");
        return;
    }
    armed().lock().unwrap_or_else(|e| e.into_inner()).insert(bot.id.clone(), Armed { run_id: run_id.to_string(), idle_since: None });
    tracing::info!(bot = %bot.name, run = run_id, busy, "resumed a claude that was busy before the restart; a nudge follows if it resumes verified, sits idle and the transcript ends on a killed tool");
    // 接回可能已經驗過、畫面也已經閒著，之後不一定還有狀態事件：自己先看一次（等這把鎖放掉）。
    schedule(app, &bot.id, Duration::ZERO);
}

/// 狀態事件（`idle`／`working`／`blocked`）或接回結論進來時叫：這顆 bot 有候選才再看一次。
pub(crate) fn poke(app: &Arc<App>, bot_id: &str) {
    if armed_run(bot_id).is_some() {
        schedule(app, bot_id, Duration::ZERO);
    }
}

/// 測試裡不排：測試直接叫 [`check_at`]、自己帶時間（同 `schedule_flush_queued`）。
fn schedule(app: &Arc<App>, bot_id: &str, delay: Duration) {
    if cfg!(test) {
        return;
    }
    let app = app.clone();
    let bot_id = bot_id.to_string();
    tokio::spawn(async move {
        if delay.is_zero() {
            tokio::task::yield_now().await;
        } else {
            tokio::time::sleep(delay).await;
        }
        let lock = app.bot_lock(&bot_id).await;
        let _g = lock.lock().await;
        check_at(&app, &bot_id, Instant::now()).await;
    });
}

enum Verdict {
    /// 還不知道（還在啟動、接回還沒有結論、讀不到 DB）：下一個事件再看。
    Wait,
    /// 這一輪取消，不再補。
    Cancel(&'static str),
    /// 接回驗過、閒著、佇列空、這個 run 上沒送過別的回合。
    Idle,
}

async fn verdict(app: &Arc<App>, bot_id: &str, run_id: &str) -> Verdict {
    let run = match db::run(&app.db, run_id).await {
        Ok(Some(run)) => run,
        Ok(None) => return Verdict::Cancel("the run is gone"),
        Err(_) => return Verdict::Wait,
    };
    match run.state.as_str() {
        "running" => {}
        "starting" => return Verdict::Wait,
        _ => return Verdict::Cancel("the run is no longer running"),
    }
    match run.resume_outcome.as_deref() {
        Some("verified") => {}
        Some(_) => return Verdict::Cancel("the resume was not verified"),
        None if run.resume_session_id.is_some() => return Verdict::Wait,
        None => return Verdict::Cancel("there is no hook to verify the resume"),
    }
    if let Some(why) = stopped_on_purpose(bot_id, &run.started_at, run.turn_error.as_deref()) {
        return Verdict::Cancel(why);
    }
    match run.agent_status.as_str() {
        "idle" => {}
        "working" | "blocked" => return Verdict::Cancel("the bot did not sit idle after the resume"),
        _ => return Verdict::Wait,
    }
    let Ok(conv) = db::conversation_id(&app.db, bot_id).await else { return Verdict::Wait };
    let counts: Result<(i64, i64), _> = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM turns WHERE conversation_id = ? AND status = 'queued'),
                (SELECT COUNT(*) FROM turns WHERE run_id = ?)",
    )
    .bind(&conv)
    .bind(run_id)
    .fetch_one(&app.db)
    .await;
    match counts {
        Ok((0, 0)) => Verdict::Idle,
        Ok((0, _)) => Verdict::Cancel("another prompt was sent on this run"),
        Ok(_) => Verdict::Cancel("another prompt is queued"),
        Err(_) => Verdict::Wait,
    }
}

/// 候選再看一次（呼叫端持 bot 鎖）。第一次看到 [`Verdict::Idle`] 起算 [`IDLE_WAIT`]，一路都沒被取消、時間到了、
/// transcript 尾巴是被砍的工具，才排續行提示。回 `true`＝排進去了。
pub(crate) async fn check_at(app: &Arc<App>, bot_id: &str, now: Instant) -> bool {
    let Some(run_id) = armed_run(bot_id) else { return false };
    match verdict(app, bot_id, &run_id).await {
        Verdict::Wait => return false,
        Verdict::Cancel(why) => {
            disarm(bot_id, &run_id);
            tracing::info!(bot = %bot_id, run = %run_id, why, "resume nudge cancelled");
            return false;
        }
        Verdict::Idle => {}
    }
    let since = {
        let mut g = armed().lock().unwrap_or_else(|e| e.into_inner());
        let Some(a) = g.get_mut(bot_id).filter(|a| a.run_id == run_id) else { return false };
        a.idle_since.replace(a.idle_since.unwrap_or(now))
    };
    let Some(since) = since else {
        schedule(app, bot_id, IDLE_WAIT);
        return false;
    };
    if now.saturating_duration_since(since) < IDLE_WAIT {
        return false;
    }
    disarm(bot_id, &run_id);
    let (Ok(Some(bot)), Ok(Some(run))) = (db::bot(&app.db, bot_id).await, db::run(&app.db, &run_id).await) else { return false };
    match transcript_ends_mid_tool(app, &bot, &run).await {
        Ok(true) => queue(app, &bot, &run_id).await,
        Ok(false) => {
            tracing::info!(bot = %bot.name, run = %run_id, "busy before the restart, but the transcript does not end on a killed tool; no resume nudge");
            false
        }
        Err(why) => {
            tracing::info!(bot = %bot.name, run = %run_id, why, "could not read the resumed transcript; no resume nudge");
            false
        }
    }
}

/// 這段對話在這個帳號底下的 transcript：`<CLAUDE_CONFIG_DIR>/projects/<cwd 目錄名>/<session>.jsonl`（`--resume` 自己也是
/// 這樣找）。cwd 目錄名取記過的那個路徑：換身分時那份記在舊帳號底下，但 `stage_cross_identity_transcript` 照同一個名字複製過來。
fn account_transcript(config_dir: &str, recorded: &str, session: &str) -> Option<std::path::PathBuf> {
    let cwd_key = std::path::Path::new(recorded).parent()?.file_name()?;
    Some(std::path::Path::new(config_dir).join("projects").join(cwd_key).join(format!("{session}.jsonl")))
}

/// 接回的那段 transcript 尾巴是不是被砍的工具（[`ends_mid_tool`]）。只讀本機（同 `stuck_turns`）；`Err`＝讀不到。
async fn transcript_ends_mid_tool(app: &Arc<App>, bot: &db::Bot, run: &db::Run) -> Result<bool, String> {
    let host = db::bot_host(&app.db, &bot.id).await.map_err(|e| e.to_string())?;
    if host != LOCAL_HOST {
        return Err(format!("the transcript is on {host}, not readable from here"));
    }
    let session = run.native_session_id.clone().filter(|s| !s.trim().is_empty()).ok_or("the resumed run has no session id")?;
    let recorded: Option<String> = sqlx::query_scalar(
        "SELECT transcript_path FROM runs WHERE bot_id = ? AND native_session_id = ? AND transcript_path IS NOT NULL AND transcript_path != ''
          ORDER BY rowid DESC LIMIT 1",
    )
    .bind(&bot.id)
    .bind(&session)
    .fetch_optional(&app.db)
    .await
    .map_err(|e| e.to_string())?;
    let recorded = recorded.ok_or("no transcript path was ever recorded for this session")?;
    let config = super::start::identity_config_dir(app, &host, bot.identity.as_deref()).await;
    let path = account_transcript(&config, &recorded, &session).ok_or("the recorded transcript path has no cwd directory")?;
    let shown = path.display().to_string();
    let log = tokio::task::spawn_blocking(move || super::transcript_origin::read_tail(&path)).await.ok().flatten();
    Ok(ends_mid_tool(&log.ok_or(format!("{shown} is not readable"))?))
}

/// 排續行提示進佇列（[`check_at`] 確定要送時）。回 `true`＝排進去了；排不進去只記 log。
async fn queue(app: &Arc<App>, bot: &db::Bot, run_id: &str) -> bool {
    let conv = match db::conversation_id(&app.db, &bot.id).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(bot = %bot.name, run = run_id, error = %e, "no conversation for the resume nudge");
            return false;
        }
    };
    let relay = super::prompt::RelaySrc::trusted(Some(crate::agent_relay::DAEMON_SENDER));
    let crid = format!("{CRID_PREFIX}{run_id}");
    match super::prompt::queue_for_next_turn(app, &conv, &bot.id, NUDGE_TEXT, NUDGE_TEXT, &crid, None, relay).await {
        Ok(out) => {
            tracing::info!(bot = %bot.name, run = run_id, turn = %out.turn_id,
                           "resumed a claude that was killed mid-tool; queued a nudge to continue (2.1.281 no longer adds a hidden Continue)");
            super::queue::schedule_flush_queued(app, &bot.id);
            true
        }
        Err(e) => {
            tracing::info!(bot = %bot.name, run = run_id, error = ?e, "resume nudge not queued (another prompt is already queued, or the write failed)");
            false
        }
    }
}

/// flush 放行前的最後一關（`resume_gate` 開了之後）：這一筆是續行提示的話，只有排它的那個 run、而且
/// `resume_outcome = verified` 才送；其他一律撤掉並在聊天室說明。回 `true`＝這一筆不送了（撤掉了，或已經不在佇列）。
/// 不是續行提示的一律 `false`。呼叫端持 bot 鎖。
pub(crate) async fn withdraw_unless_resumed(app: &Arc<App>, turn: &db::Turn, run: &db::Run) -> anyhow::Result<bool> {
    let Some(for_run) = turn.client_request_id.as_deref().and_then(|c| c.strip_prefix(CRID_PREFIX)) else { return Ok(false) };
    let why = if for_run != run.id {
        "續行提示沒有送出：排它的那一次啟動已經不在了（之後又重啟過），接著做什麼交給使用者或 AGM。"
    } else {
        match run.resume_outcome.as_deref() {
            Some("verified") => return Ok(false),
            Some("mismatch") => "續行提示沒有送出：這次接回的不是原本那段對話（CLI 開了新對話），新對話裡沒有「原本的工作」可以接著做。",
            Some("unverified") => "續行提示沒有送出：確認不了接回的是不是原本那段對話，不在不確定的對話裡叫它接著做。",
            _ => "續行提示沒有送出：這顆 bot 沒有 hook 可以確認接回的是原本那段對話，不在不確定的對話裡叫它接著做。",
        }
    };
    super::queue::revoke_queued_turn(app, &turn.id, why).await?;
    tracing::info!(bot = %run.bot_id, run = %run.id, turn = %turn.id, outcome = ?run.resume_outcome, "withdrew the resume nudge: not a verified resume of the old session");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{claude_bot, env, Env};
    use serde_json::json;

    /// herdr 砍 pane：模型叫了工具，結果還沒寫進去。
    fn tool_use() -> String {
        json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "cargo test"}}]}}).to_string()
    }
    /// daemon／SIGTERM：工具被收掉（exit 137），模型還沒回。
    fn tool_result() -> String {
        json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "Exit code 137", "is_error": true}]}}).to_string()
    }
    fn reply(text: &str) -> String {
        json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}}).to_string()
    }
    fn user(text: &str) -> String {
        json!({"type": "user", "message": {"role": "user", "content": text}}).to_string()
    }
    fn log(lines: &[String]) -> String {
        lines.iter().map(|l| format!("{l}\n")).collect()
    }
    /// 被砍在工具中途的 transcript（預設尾巴）。
    fn killed() -> String {
        log(&[user("跑測試"), tool_use()])
    }

    /// 裝 cwd 目錄名；真的 transcript 在 `<CLAUDE_CONFIG_DIR>/projects/<這個>/<session>.jsonl`。
    const CWD_KEY: &str = "-Users-m4p-project-x";

    fn write_transcript(config_dir: &std::path::Path, bot_id: &str, body: &str) -> std::path::PathBuf {
        let dir = config_dir.join("projects").join(CWD_KEY);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join(format!("sid-{bot_id}.jsonl"));
        std::fs::write(&f, body).unwrap();
        f
    }

    /// 這顆 bot 用的帳號（`CLAUDE_CONFIG_DIR`）；測試不碰真的 `~/.claude`。
    async fn use_account(e: &Env, bot_id: &str, name: &str) -> std::path::PathBuf {
        let dir = e.dir.join(name);
        let (n, d) = (name.to_string(), dir.to_str().unwrap().to_string());
        e.app
            .cfg
            .update(move |c| {
                if !c.identities.iter().any(|i| i.name == n) {
                    c.identities.push(crate::config::IdentityCfg {
                        name: n.clone(),
                        kind: "claude".into(),
                        host: None,
                        env: [("CLAUDE_CONFIG_DIR".to_string(), d.clone())].into(),
                        args: vec![],
                    });
                }
                Ok(())
            })
            .await
            .unwrap();
        sqlx::query("UPDATE bots SET identity=? WHERE id=?").bind(name).bind(bot_id).execute(&e.app.db).await.unwrap();
        dir
    }

    async fn queued(e: &Env, bot_id: &str) -> Vec<(String, Option<String>)> {
        let conv = db::conversation_id(&e.app.db, bot_id).await.unwrap();
        sqlx::query_as(
            "SELECT m.content, m.relay_from FROM turns t JOIN messages m ON m.turn_id = t.id
             WHERE t.conversation_id = ? AND t.status = 'queued'",
        )
        .bind(&conv)
        .fetch_all(&e.app.db)
        .await
        .unwrap()
    }

    /// 上一段對話有 transcript，`--resume` 接得回；尾巴是被砍的工具。
    async fn resumable(e: &Env, bot_id: &str, state: &str, status: &str) {
        resumable_at(e, bot_id, state, status, "2026-09-24T00:00:00Z", None).await;
    }

    async fn resumable_at(e: &Env, bot_id: &str, state: &str, status: &str, started_at: &str, turn_error: Option<&str>) {
        resumable_with(e, bot_id, state, status, started_at, turn_error, &killed()).await;
    }

    async fn resumable_with(e: &Env, bot_id: &str, state: &str, status: &str, started_at: &str, turn_error: Option<&str>, body: &str) {
        let account = use_account(e, bot_id, "cc").await;
        let transcript = write_transcript(&account, bot_id, body);
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at, turn_error)
             VALUES (?,?,?,?,?,?,?,'2026-09-24T00:01:00Z',?)",
        )
        .bind(db::ulid())
        .bind(bot_id)
        .bind(state)
        .bind(status)
        .bind(format!("sid-{bot_id}"))
        .bind(transcript.to_str().unwrap())
        .bind(started_at)
        .bind(turn_error)
        .execute(&e.app.db)
        .await
        .unwrap();
    }

    fn strict() -> StartOpts {
        StartOpts { resume_native: true, resume_required: true, ..Default::default() }
    }

    /// SessionStart 回報了：接回的結論寫上去、畫面回到 `idle`。
    async fn settle(e: &Env, bot_id: &str, run: &str, outcome: Option<&str>) {
        sqlx::query(
            "UPDATE runs SET state='running', agent_status='idle', resume_session_id=NULL, resume_outcome=?, native_session_id=? WHERE id=?",
        )
        .bind(outcome)
        .bind(format!("sid-{bot_id}"))
        .bind(run)
        .execute(&e.app.db)
        .await
        .unwrap();
    }

    /// 看一次開窗、再看一次滿 10 秒：回第二次有沒有排進去。
    async fn wait_out(e: &Env, bot_id: &str) -> bool {
        let t0 = Instant::now();
        let first = check_at(&e.app, bot_id, t0).await;
        assert!(!first, "第一次看到閒置只開始計時，不送");
        check_at(&e.app, bot_id, t0 + IDLE_WAIT).await
    }

    #[test]
    fn only_a_busy_claude_resume_is_nudged() {
        assert!(wants_nudge("claude", true, Some("working")));
        assert!(!wants_nudge("claude", true, Some("blocked")), "停在確認框／問卷等人的不催（#430）");
        assert!(wants_nudge("claude", true, Some("turn_in_flight")));
        assert!(!wants_nudge("claude", true, None), "閒著重啟的不補");
        assert!(!wants_nudge("claude", false, Some("working")), "開新對話的不補");
        assert!(!wants_nudge("codex", true, Some("working")), "codex 照舊");
        assert!(!wants_nudge("grok", true, Some("working")), "grok 照舊");
    }

    /// 尾巴只有兩種算被砍在工具中途：沒有結果的 `tool_use`、`tool_result` 之後沒有模型回覆（#424 裁示丙）。
    #[test]
    fn only_a_killed_tool_at_the_tail_counts() {
        let system = json!({"type": "system", "subtype": "stop_hook_summary"}).to_string();
        let meta = json!({"type": "user", "isMeta": true, "message": {"role": "user", "content": "<local-command-caveat>"}}).to_string();
        let thinking = json!({"type": "assistant", "message": {"content": [{"type": "thinking", "thinking": "…"}]}}).to_string();
        let interrupted = json!({"type": "user", "message": {"content": [{"type": "text", "text": "[Request interrupted by user for tool use]"}]}}).to_string();
        // 2.1.281 接回時替沒有結果的呼叫補的那一筆（字串取自 2.1.281 執行檔）。
        let unknown = json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "toolu_1",
            "content": "[Tool call interrupted: the session ended before this call's result was recorded, so its outcome is unknown. Check whether it took effect before relying on it or running it again.]"}]}})
        .to_string();
        for (case, lines, want) in [
            ("tool_use 沒有結果", vec![user("跑"), reply("我先跑測試"), tool_use()], true),
            ("tool_result 之後沒回覆", vec![user("跑"), tool_use(), tool_result()], true),
            ("接回補的 outcome unknown", vec![user("跑"), tool_use(), unknown], true),
            ("後面只有系統列與 meta", vec![user("跑"), tool_use(), system.clone(), meta.clone()], true),
            ("tool_result 後只想了一下", vec![user("跑"), tool_use(), tool_result(), thinking.clone()], true),
            ("讀尾巴切到一半的第一行", vec!["ol_use\"}]}}".into(), tool_use()], true),
            ("正常回覆", vec![user("跑"), tool_use(), tool_result(), reply("測試都過了")], false),
            ("回覆後面接系統列", vec![user("跑"), reply("好了"), system], false),
            ("使用者中斷", vec![user("跑"), tool_use(), tool_result(), interrupted], false),
            ("使用者打了字還沒回", vec![reply("好了"), user("下一件")], false),
            ("空檔", vec![], false),
            ("只有 meta", vec![meta], false),
        ] {
            assert_eq!(ends_mid_tool(&log(&lines)), want, "{case}");
        }
    }

    /// 重啟：做事中被 `?resume=native` 重啟的 claude 記成候選、**不佔佇列**；閒置中、停在等人畫面上（`blocked`）重啟的不記。
    #[tokio::test]
    async fn a_claude_restarted_while_busy_is_armed_without_taking_the_queue() {
        let e = env().await;
        let strict = strict();
        for (status, want) in [("working", true), ("blocked", false), ("idle", false)] {
            let bot = claude_bot(&e.app, &e.project_id, &format!("r-{status}")).await;
            resumable(&e, &bot.id, "stopped", "idle").await;
            let run = crate::lifecycle::start_bot_with(&e.app, &bot.id, strict.clone()).await.unwrap();
            sqlx::query("UPDATE runs SET agent_status=? WHERE id=?").bind(status).bind(&run).execute(&e.app.db).await.unwrap();
            let run = crate::lifecycle::restart_bot_with(&e.app, &bot.id, strict.clone()).await.unwrap();
            assert!(queued(&e, &bot.id).await.is_empty(), "{status}：啟動當下不佔 queued 槽");
            assert_eq!(armed_run(&bot.id), want.then_some(run), "{status}");
            crate::lifecycle::stop_bot(&e.app, &bot.id).await.unwrap();
        }
    }

    /// 閒著但還有一回合沒收（hook 還沒到）也算忙。
    #[tokio::test]
    async fn an_open_turn_counts_as_busy() {
        let e = env().await;
        let strict = strict();
        let bot = claude_bot(&e.app, &e.project_id, "open-turn").await;
        resumable(&e, &bot.id, "stopped", "idle").await;
        let run = crate::lifecycle::start_bot_with(&e.app, &bot.id, strict.clone()).await.unwrap();
        sqlx::query("UPDATE runs SET agent_status='idle' WHERE id=?").bind(&run).execute(&e.app.db).await.unwrap();
        let conv = db::conversation_id(&e.app.db, &bot.id).await.unwrap();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
            .bind(db::ulid())
            .bind(&conv)
            .bind(&run)
            .bind(db::now())
            .execute(&e.app.db)
            .await
            .unwrap();
        let run = crate::lifecycle::restart_bot_with(&e.app, &bot.id, strict).await.unwrap();
        assert_eq!(armed_run(&bot.id), Some(run));
    }

    /// herdr 整個重啟：舊 run 被 pane 消失收成 `exited`、最後記著 working，`start?resume=native` 接回之後記成候選；
    /// 最後是 idle 的、停在等人畫面上的（`blocked`）、使用者自己停的（`stopped`）不記。
    #[tokio::test]
    async fn a_start_after_the_pane_died_mid_work_is_armed() {
        let e = env().await;
        let strict = strict();
        for (state, status, want) in [("exited", "working", true), ("exited", "blocked", false), ("exited", "idle", false), ("stopped", "working", false)] {
            let bot = claude_bot(&e.app, &e.project_id, &format!("s-{state}-{status}")).await;
            resumable(&e, &bot.id, state, status).await;
            let run = crate::lifecycle::start_bot_with(&e.app, &bot.id, strict.clone()).await.unwrap();
            assert_eq!(armed_run(&bot.id), want.then_some(run), "{state}/{status}");
            assert!(queued(&e, &bot.id).await.is_empty(), "{state}/{status}：啟動當下不佔 queued 槽");
            crate::lifecycle::stop_bot(&e.app, &bot.id).await.unwrap();
        }
    }

    /// 上一回合是被 API 錯誤收掉的（撞額度等，`runs.turn_error`）：不是外力砍在半路，不補——重啟與啟動兩條都一樣。
    #[tokio::test]
    async fn a_run_cut_short_by_an_api_error_is_not_nudged() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "quota-start").await;
        resumable_at(&e, &bot.id, "exited", "working", "2026-09-24T00:00:00Z", Some("API Error: 429 rate limit")).await;
        crate::lifecycle::start_bot_with(&e.app, &bot.id, strict()).await.unwrap();
        assert_eq!(armed_run(&bot.id), None, "啟動：上一個 run 撞額度收掉的不補");
        crate::lifecycle::stop_bot(&e.app, &bot.id).await.unwrap();

        let bot = claude_bot(&e.app, &e.project_id, "quota-restart").await;
        resumable(&e, &bot.id, "stopped", "idle").await;
        let run = crate::lifecycle::start_bot_with(&e.app, &bot.id, strict()).await.unwrap();
        sqlx::query("UPDATE runs SET agent_status='working', turn_error='API Error: 429 rate limit' WHERE id=?")
            .bind(&run)
            .execute(&e.app.db)
            .await
            .unwrap();
        crate::lifecycle::restart_bot_with(&e.app, &bot.id, strict()).await.unwrap();
        assert_eq!(armed_run(&bot.id), None, "重啟：這一回合撞額度收掉的不補");
    }

    /// 使用者剛從網頁中斷過這個 run：是人叫它停的，重啟接回之後不替他續做。
    #[tokio::test]
    async fn a_run_the_user_interrupted_is_not_nudged() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "interrupted").await;
        resumable(&e, &bot.id, "stopped", "idle").await;
        let run = crate::lifecycle::start_bot_with(&e.app, &bot.id, strict()).await.unwrap();
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run).execute(&e.app.db).await.unwrap();
        crate::lifecycle::interrupt_grace::note_user_interrupt(&bot.id);
        crate::lifecycle::restart_bot_with(&e.app, &bot.id, strict()).await.unwrap();
        assert_eq!(armed_run(&bot.id), None);
    }

    /// 「上一個 run」是最後寫進去的那一列，不是 `started_at` 字串排最大的那列：秒精度的 `…00Z` 字串上排在
    /// 毫秒精度的 `…00.500Z` 後面（`Z` > `.`，#101），用字串排會拿舊的那個 run 的 `exited`／working 來補。
    #[tokio::test]
    async fn the_previous_run_is_the_last_one_written_not_the_largest_timestamp_string() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "mixed-precision").await;
        resumable_at(&e, &bot.id, "exited", "working", "2026-09-24T00:00:00Z", None).await;
        resumable_at(&e, &bot.id, "stopped", "idle", "2026-09-24T00:00:00.500Z", None).await;
        crate::lifecycle::start_bot_with(&e.app, &bot.id, strict()).await.unwrap();
        assert_eq!(armed_run(&bot.id), None, "真正最後那個 run 是使用者停的，不補");
    }

    /// #424 裁示丙的整張表：transcript 尾巴（沒結果的 tool_use／tool_result 無回覆／正常回覆／空檔）×
    /// 接回 verified 與否 × 10 秒內佇列有沒有東西。只有「被砍的工具＋verified＋佇列一直空」送一次。
    #[tokio::test]
    async fn the_nudge_needs_a_killed_tool_a_verified_resume_and_an_empty_queue() {
        let e = env().await;
        let tails = [
            ("tool_use", killed(), true),
            ("tool_result", log(&[user("跑"), tool_use(), tool_result()]), true),
            ("reply", log(&[user("跑"), tool_use(), tool_result(), reply("做完了")]), false),
            ("empty", String::new(), false),
        ];
        for (tail, body, killed_tool) in &tails {
            for verified in [true, false] {
                for queue_in_window in [false, true] {
                    let case = format!("{tail}/verified={verified}/queue={queue_in_window}");
                    let bot = claude_bot(&e.app, &e.project_id, &format!("m-{tail}-{verified}-{queue_in_window}")).await;
                    resumable_with(&e, &bot.id, "exited", "working", "2026-09-24T00:00:00Z", None, body).await;
                    let run = crate::lifecycle::start_bot_with(&e.app, &bot.id, strict()).await.unwrap();
                    assert_eq!(armed_run(&bot.id).as_deref(), Some(run.as_str()), "{case}：先記成候選");
                    settle(&e, &bot.id, &run, Some(if verified { "verified" } else { "mismatch" })).await;
                    let t0 = Instant::now();
                    assert!(!check_at(&e.app, &bot.id, t0).await, "{case}：不會馬上送");
                    if queue_in_window {
                        let conv = db::conversation_id(&e.app.db, &bot.id).await.unwrap();
                        let relay = super::super::prompt::RelaySrc::trusted(Some("agm"));
                        super::super::prompt::queue_for_next_turn(&e.app, &conv, &bot.id, "派工", "派工", "agm-1", None, relay).await.unwrap();
                    }
                    let sent = check_at(&e.app, &bot.id, t0 + IDLE_WAIT).await;
                    let want = *killed_tool && verified && !queue_in_window;
                    assert_eq!(sent, want, "{case}");
                    let q = queued(&e, &bot.id).await;
                    let mut expect = Vec::new();
                    if queue_in_window {
                        expect.push(("派工".to_string(), Some("agm".to_string())));
                    }
                    if want {
                        expect.push((NUDGE_TEXT.to_string(), Some("daemon".to_string())));
                    }
                    assert_eq!(q, expect, "{case}");
                    assert_eq!(armed_run(&bot.id), None, "{case}：不論送不送，這個 run 都不再看");
                    assert!(!check_at(&e.app, &bot.id, t0 + IDLE_WAIT * 3).await, "{case}：同一個 run 只補一次");
                    crate::lifecycle::stop_bot(&e.app, &bot.id).await.unwrap();
                }
            }
        }
    }

    /// 10 秒從「接回驗過且 idle」起算：還沒驗過的時候怎麼等都不算，驗過後不滿 10 秒不送。
    #[tokio::test]
    async fn the_ten_seconds_start_once_the_resume_is_verified_and_idle() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "window").await;
        resumable(&e, &bot.id, "exited", "working").await;
        let run = crate::lifecycle::start_bot_with(&e.app, &bot.id, strict()).await.unwrap();
        let t0 = Instant::now();
        sqlx::query("UPDATE runs SET state='running', agent_status='idle' WHERE id=?").bind(&run).execute(&e.app.db).await.unwrap();
        assert!(!check_at(&e.app, &bot.id, t0).await, "接回還沒結論：等");
        assert!(!check_at(&e.app, &bot.id, t0 + IDLE_WAIT * 2).await, "還沒結論就不開始算");
        settle(&e, &bot.id, &run, Some("verified")).await;
        let t1 = t0 + IDLE_WAIT * 3;
        assert!(!check_at(&e.app, &bot.id, t1).await, "驗過、閒著：從這裡開始算");
        assert!(!check_at(&e.app, &bot.id, t1 + IDLE_WAIT - Duration::from_secs(1)).await, "9 秒：還不送");
        assert!(queued(&e, &bot.id).await.is_empty());
        assert!(check_at(&e.app, &bot.id, t1 + IDLE_WAIT).await, "滿 10 秒：送");
        assert_eq!(queued(&e, &bot.id).await, vec![(NUDGE_TEXT.to_string(), Some("daemon".to_string()))]);
    }

    /// 驗過之後在等的 10 秒裡變成 `working`／`blocked`、或這個 run 上已經送過別的回合：這一輪取消，回到 idle 也不補。
    #[tokio::test]
    async fn activity_during_the_window_cancels_the_nudge() {
        let e = env().await;
        for case in ["working", "blocked", "turn"] {
            let bot = claude_bot(&e.app, &e.project_id, &format!("w-{case}")).await;
            resumable(&e, &bot.id, "exited", "working").await;
            let run = crate::lifecycle::start_bot_with(&e.app, &bot.id, strict()).await.unwrap();
            settle(&e, &bot.id, &run, Some("verified")).await;
            let t0 = Instant::now();
            assert!(!check_at(&e.app, &bot.id, t0).await);
            if case == "turn" {
                let conv = db::conversation_id(&e.app.db, &bot.id).await.unwrap();
                sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','completed','ok',?)")
                    .bind(db::ulid())
                    .bind(&conv)
                    .bind(&run)
                    .bind(db::now())
                    .execute(&e.app.db)
                    .await
                    .unwrap();
            } else {
                sqlx::query("UPDATE runs SET agent_status=? WHERE id=?").bind(case).bind(&run).execute(&e.app.db).await.unwrap();
                assert!(!check_at(&e.app, &bot.id, t0 + Duration::from_secs(3)).await);
                sqlx::query("UPDATE runs SET agent_status='idle' WHERE id=?").bind(&run).execute(&e.app.db).await.unwrap();
            }
            assert!(!check_at(&e.app, &bot.id, t0 + IDLE_WAIT).await, "{case}");
            assert!(queued(&e, &bot.id).await.is_empty(), "{case}");
            crate::lifecycle::stop_bot(&e.app, &bot.id).await.unwrap();
        }
    }

    /// transcript 照這顆 bot 實際帳號的 `CLAUDE_CONFIG_DIR` 找：換身分後 CLI 接著寫的是複製進新帳號那份，
    /// 舊帳號那份（run 上記的路徑）停在接回之前，不看它。
    #[tokio::test]
    async fn the_transcript_is_read_from_the_accounts_config_dir() {
        let e = env().await;
        for (case, old_body, account_body, want) in [
            ("帳號那份是被砍的工具", log(&[user("跑"), reply("好了")]), killed(), true),
            ("帳號那份已經回覆了", killed(), log(&[user("跑"), tool_use(), tool_result(), reply("好了")]), false),
        ] {
            let bot = claude_bot(&e.app, &e.project_id, &format!("acct-{want}")).await;
            let old = write_transcript(&e.dir.join(format!("old-{want}")), &bot.id, &old_body);
            sqlx::query(
                "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at)
                 VALUES (?,?,'exited','working',?,?,'2026-09-24T00:00:00Z','2026-09-24T00:01:00Z')",
            )
            .bind(db::ulid())
            .bind(&bot.id)
            .bind(format!("sid-{}", bot.id))
            .bind(old.to_str().unwrap())
            .execute(&e.app.db)
            .await
            .unwrap();
            let account = use_account(&e, &bot.id, &format!("cc-new-{want}")).await;
            let run = crate::lifecycle::start_bot_with(&e.app, &bot.id, strict()).await.unwrap();
            // 換身分：啟動時把舊帳號那份複製進新帳號；之後 CLI 接著寫的是新帳號那份。
            write_transcript(&account, &bot.id, &account_body);
            settle(&e, &bot.id, &run, Some("verified")).await;
            assert_eq!(wait_out(&e, &bot.id).await, want, "{case}");
            crate::lifecycle::stop_bot(&e.app, &bot.id).await.unwrap();
        }
    }

    /// flush 放行前的最後一關：排進去之後 run 換掉、或接回結論翻成不是 `verified`，撤掉並在聊天室說明（#430）。
    #[tokio::test]
    async fn a_queued_nudge_is_withdrawn_unless_the_resume_is_still_verified() {
        let e = env().await;
        for (case, outcome, withdrawn) in [("verified", "verified", false), ("mismatch", "mismatch", true), ("unverified", "unverified", true)] {
            let bot = claude_bot(&e.app, &e.project_id, &format!("v-{case}")).await;
            resumable(&e, &bot.id, "exited", "working").await;
            let run = crate::lifecycle::start_bot_with(&e.app, &bot.id, strict()).await.unwrap();
            settle(&e, &bot.id, &run, Some("verified")).await;
            assert!(wait_out(&e, &bot.id).await, "{case}：先排進去");
            sqlx::query("UPDATE runs SET resume_outcome=? WHERE id=?").bind(outcome).bind(&run).execute(&e.app.db).await.unwrap();
            let crid = format!("{CRID_PREFIX}{run}");
            forget_queue_retry_timer(&bot.id);
            flush_queued_locked(&e.app, &bot.id).await.unwrap();
            forget_queue_retry_timer(&bot.id);
            let conv = db::conversation_id(&e.app.db, &bot.id).await.unwrap();
            let t: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE conversation_id=? AND client_request_id=?")
                .bind(&conv)
                .bind(&crid)
                .fetch_one(&e.app.db)
                .await
                .unwrap();
            let notes: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'")
                .bind(&t.id)
                .fetch_all(&e.app.db)
                .await
                .unwrap();
            let said = notes.iter().any(|n| n.contains("續行提示沒有送出"));
            if withdrawn {
                assert_eq!((t.status.as_str(), t.flush_retries, said), ("failed", 0, true), "{case}：撤掉、一個字都沒打、有說明：{notes:?}");
            } else {
                assert!(!said && (t.status != "queued" || t.flush_retries > 0), "{case}：過了這一關往下送：{t:?} {notes:?}");
            }
            crate::lifecycle::stop_bot(&e.app, &bot.id).await.unwrap();
        }
    }

    /// 接不回、退回開新對話：沒有原本的工作可續，不補。
    #[tokio::test]
    async fn a_fallback_to_a_new_conversation_is_not_nudged() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "fresh").await;
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, started_at, ended_at)
             VALUES (?,?,'exited','working','2026-09-24T00:00:00Z','2026-09-24T00:01:00Z')",
        )
        .bind(db::ulid())
        .bind(&bot.id)
        .execute(&e.app.db)
        .await
        .unwrap();
        crate::lifecycle::start_bot_with(&e.app, &bot.id, StartOpts { resume_native: true, ..Default::default() }).await.unwrap();
        assert_eq!(armed_run(&bot.id), None);
        assert!(queued(&e, &bot.id).await.is_empty());
    }

    /// 啟動前佇列裡已經有一則（AGM 的派工）：不擠掉它、也不疊第二則，這一輪直接取消。
    #[tokio::test]
    async fn an_already_queued_prompt_is_left_alone() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "has-queue").await;
        resumable(&e, &bot.id, "exited", "working").await;
        let conv = db::conversation_id(&e.app.db, &bot.id).await.unwrap();
        let relay = super::super::prompt::RelaySrc::trusted(Some("agm"));
        super::super::prompt::queue_for_next_turn(&e.app, &conv, &bot.id, "派工", "派工", "agm-1", None, relay).await.unwrap();
        let run = crate::lifecycle::start_bot_with(&e.app, &bot.id, strict()).await.unwrap();
        settle(&e, &bot.id, &run, Some("verified")).await;
        assert!(!check_at(&e.app, &bot.id, Instant::now()).await);
        assert_eq!(armed_run(&bot.id), None);
        assert_eq!(queued(&e, &bot.id).await, vec![("派工".to_string(), Some("agm".to_string()))]);
    }
}
