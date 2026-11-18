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
//!   （`interrupt_grace` 記的接管時刻落在這個 run 裡）——那是有人或有原因叫它停的，不替它續做。暫時性斷線（`Connection lost`、529、5xx）
//!   例外，由 `api_error_nudge` 在同一個 run 裡處理。
//! - **尾巴要是被砍的工具**（[`ends_mid_tool`]）：要接回的 transcript 結尾是沒有結果的 `tool_use`（herdr 砍 pane），
//!   或是 `tool_result`（daemon／SIGTERM，實測 exit 137）且其後沒有模型回覆。正常的模型回覆、使用者打的字（含
//!   `[Request interrupted by user…]`）、空檔、讀不到的，都不補。檔案照這顆 bot 實際帳號的 `CLAUDE_CONFIG_DIR` 找
//!   （[`account_transcript`]）：換過身份的話，CLI 接著寫的是複製進新帳號那一份，舊帳號那份停在接回之前。
//! - **啟動當下不佔 queued 槽**：只記在記憶體裡（[`arm`]）。`resume_outcome = verified` 且畫面 `idle` 之後再等
//!   [`IDLE_WAIT`]；期間仍是同一個 run、仍 idle、沒有回合在飛、佇列一直是空的、這個 run 上沒有送過別的回合，
//!   時間到才排一則（[`check_at`]）。這段期間變成 `working`／`blocked`、有人排了派工、接回不是 `verified`、使用者中斷，
//!   這一輪就取消。AGM 或使用者的派工因此可以先佔，不會跟續行提示做成兩回合。
//!
//! 「重啟前在忙」有兩種讀法：
//! - **重啟**（`restart_bot_with`：`?resume=native` restart、換身份）：停之前在鎖裡讀（[`busy_before_restart`]）。
//!   一鍵重啟要求閒置（`require_idle`），不會遇到。
//! - **啟動**（herdr 整個重啟後 `start?resume=native`）：舊 run 已經被 pane 消失收成 `exited`，看它最後記的
//!   `agent_status`（[`prior_run_ended_busy`]）。pane 是被外力收掉的，最後一次狀態就是當時的狀態。使用者自己停的
//!   （`stopped`）不算：停下來是他要的，接回來不替他續做。「上一個 run」用 `rowid` 挑：`started_at` 字串精度不一、
//!   ulid 同毫秒不單調，都排不出真正最後那個（#101、#100）。
//!
//! 不補的：codex／grok（沒有這個改變）；接不回、退回開新對話的（沒有「原本的工作」可續）；遠端主機的 bot
//! （transcript 在那台機器上，這裡讀不到尾巴，跟 `stuck_turns` 一樣只讀本機）。同一個 run 只補一次
//! （`client_request_id = resume-nudge:<run_id>`）。等待記在這個行程的記憶體裡，daemon 在這段時間重啟就不補。

#[cfg(all(test, feature = "daemon-test-harness"))]
use crate::capabilities::Db;
use super::s6_ports::ResumeNudgeContext;
use super::*;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// 送進 bot 的續行提示。
pub const NUDGE_TEXT: &str = "[來自 AG Man daemon] 重啟前你正在進行的工作被中斷了（重啟當下可能有工具還在跑，結果不明）。\
請先確認上一個工具的實際結果，再接著把原本的工作做完。";

/// 續行提示的 `client_request_id` 前綴，後面接排它的那個 run。
pub const CRID_PREFIX: &str = "resume-nudge:";

/// 接回驗證過、畫面閒置之後，還要再空著這麼久才送（#424 裁示）。
pub const IDLE_WAIT: Duration = Duration::from_secs(10);

/// 純規則：這次啟動要不要進候選。`busy` 是重啟前讀到的忙碌理由（`None`＝閒著）。
/// `blocked`（在等人按確認框／答問卷）不算：那一步只有人能決定（#423、#430）。
pub fn wants_nudge(kind: &str, resume_native: bool, busy: Option<&str>) -> bool {
    kind == "claude" && resume_native && matches!(busy, Some("working" | "turn_in_flight"))
}

/// 純規則：claude transcript 的尾巴是不是被砍在工具中途。從後往前找最後一則對話：模型的 `tool_use`（後面沒有結果）、
/// 或 `tool_result`（後面沒有模型回覆）＝是；模型的文字回覆、使用者打的字（含中斷標記、`<pasted_content>`）＝不是。
/// 系統列、`isMeta`、只有 thinking 的模型列略過；讀尾巴切到一半的第一行解析不了，也略過。空檔＝不是。
/// 2.1.281 接回時替沒有結果的呼叫補的「[Tool call interrupted: … outcome is unknown …]」本身就是 `tool_result`，照樣算。
pub fn ends_mid_tool(log: &str) -> bool {
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
pub async fn busy_before_restart(app: &impl crate::capabilities::Db, bot: &db::Bot, opts: &StartOpts) -> Option<&'static str> {
    if !wants_nudge(&bot.kind, opts.resume_native, Some("working")) {
        return None;
    }
    let run = match db::active_run(app.db(), &bot.id).await {
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
        _ => match db::in_flight_turn(app.db(), &run.id).await {
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
pub async fn prior_run_ended_busy(app: &impl crate::capabilities::Db, bot: &db::Bot, opts: &StartOpts) -> Option<&'static str> {
    if !wants_nudge(&bot.kind, opts.resume_native, Some("working")) {
        return None;
    }
    // 最後寫進去的那一列＝最後一個 run：`rowid` 單調，`started_at`（精度不一）與 ulid（同毫秒不單調）都不是（#430）。
    let last: Option<(String, String, String, Option<String>)> =
        match sqlx::query_as("SELECT state, agent_status, started_at, turn_error FROM runs WHERE bot_id = ? ORDER BY rowid DESC LIMIT 1")
            .bind(&bot.id)
            .fetch_optional(app.db())
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

/// 不在 `live` 裡的 bot 不留候選：bot 被刪掉就沒有人會再 disarm 它。
pub fn retain_bots(live: &[String]) {
    armed().lock().unwrap_or_else(|e| e.into_inner()).retain(|id, _| live.contains(id));
}

fn disarm(bot_id: &str, run_id: &str) {
    let mut g = armed().lock().unwrap_or_else(|e| e.into_inner());
    if g.get(bot_id).is_some_and(|a| a.run_id == run_id) {
        g.remove(bot_id);
    }
}

/// 新 run 起來之後記成候選，**不排佇列**。只在真的帶了 `--resume` 時記（接不回、開了新對話就沒有原本的工作）。
/// 呼叫端持 bot 鎖，而且已經用 [`busy_before_restart`]／[`prior_run_ended_busy`] 濾過。
pub async fn arm(app: &impl ResumeNudgeContext, bot: &db::Bot, run_id: &str, busy: &str) {
    let resumed: Option<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT resume_session_id, resume_outcome FROM runs WHERE id = ?").bind(run_id).fetch_optional(app.db()).await.ok().flatten();
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
pub fn poke(app: &impl ResumeNudgeContext, bot_id: &str) {
    if armed_run(bot_id).is_some() {
        schedule(app, bot_id, Duration::ZERO);
    }
}

/// 測試裡不排：測試直接叫 [`check_at`]、自己帶時間（同 `schedule_flush_queued`）。
fn schedule<C: ResumeNudgeContext>(app: &C, bot_id: &str, delay: Duration) {
    if cfg!(test) {
        return;
    }
    let app = (*app).clone();
    let bot_id = bot_id.to_string();
    tokio::spawn(Box::pin(async move {
        if delay.is_zero() {
            tokio::task::yield_now().await;
        } else {
            tokio::time::sleep(delay).await;
        }
        let lock = app.bot_lock(&bot_id).await;
        let _g = lock.lock_owned().await;
        check_at(&app, &bot_id, Instant::now()).await;
    }));
}

enum Verdict {
    /// 還不知道（還在啟動、接回還沒有結論、讀不到 DB）：下一個事件再看。
    Wait,
    /// 這一輪取消，不再補。
    Cancel(&'static str),
    /// 接回驗過、閒著、佇列空、這個 run 上沒送過別的回合。
    Idle,
}

async fn verdict(app: &impl crate::capabilities::Db, bot_id: &str, run_id: &str) -> Verdict {
    let run = match db::run(app.db(), run_id).await {
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
    let Ok(conv) = db::conversation_id(app.db(), bot_id).await else { return Verdict::Wait };
    let counts: Result<(i64, i64), _> = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM turns WHERE conversation_id = ? AND status = 'queued'),
                (SELECT COUNT(*) FROM turns WHERE run_id = ?)",
    )
    .bind(&conv)
    .bind(run_id)
    .fetch_one(app.db())
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
pub async fn check_at(app: &impl ResumeNudgeContext, bot_id: &str, now: Instant) -> bool {
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
    let (Ok(Some(bot)), Ok(Some(run))) = (db::bot(app.db(), bot_id).await, db::run(app.db(), &run_id).await) else { return false };
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
/// 這樣找）。cwd 目錄名取記過的那個路徑：換身份時那份記在舊帳號底下，但 `stage_cross_identity_transcript` 照同一個名字複製過來。
fn account_transcript(config_dir: &str, recorded: &str, session: &str) -> Option<std::path::PathBuf> {
    let cwd_key = std::path::Path::new(recorded).parent()?.file_name()?;
    Some(std::path::Path::new(config_dir).join("projects").join(cwd_key).join(format!("{session}.jsonl")))
}

/// 接回的那段 transcript 尾巴是不是被砍的工具（[`ends_mid_tool`]）。只讀本機（同 `stuck_turns`）；`Err`＝讀不到。
async fn transcript_ends_mid_tool(app: &(impl crate::capabilities::Db + crate::hosts::HostsAccess + super::s6_ports::ResumeNudgeServices), bot: &db::Bot, run: &db::Run) -> Result<bool, String> {
    let host = db::bot_host(app.db(), &bot.id).await.map_err(|e| e.to_string())?;
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
    .fetch_optional(app.db())
    .await
    .map_err(|e| e.to_string())?;
    let recorded = recorded.ok_or("no transcript path was ever recorded for this session")?;
    let config = app.identity_config_dir(&host, bot.identity.as_deref()).await.map_err(|e| e.to_string())?;
    let path = account_transcript(&config, &recorded, &session).ok_or("the recorded transcript path has no cwd directory")?;
    let shown = path.display().to_string();
    let log = tokio::task::spawn_blocking(move || super::transcript_origin::read_tail(&path)).await.ok().flatten();
    Ok(ends_mid_tool(&log.ok_or(format!("{shown} is not readable"))?))
}

/// 排續行提示進佇列（[`check_at`] 確定要送時）。回 `true`＝排進去了；排不進去只記 log。
async fn queue(app: &impl ResumeNudgeContext, bot: &db::Bot, run_id: &str) -> bool {
    let conv = match db::conversation_id(app.db(), &bot.id).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(bot = %bot.name, run = run_id, error = %e, "no conversation for the resume nudge");
            return false;
        }
    };
    let crid = format!("{CRID_PREFIX}{run_id}");
    match app.queue_nudge(&conv, &bot.id, NUDGE_TEXT, &crid, crate::agent_relay::DAEMON_SENDER).await {
        Ok(out) => {
            tracing::info!(bot = %bot.name, run = run_id, turn = %out.turn_id,
                           "resumed a claude that was killed mid-tool; queued a nudge to continue (2.1.281 no longer adds a hidden Continue)");
            app.schedule_queue_flush(&bot.id);
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
pub async fn withdraw_unless_resumed(app: &impl super::s6_ports::QueueContext, turn: &db::Turn, run: &db::Run) -> anyhow::Result<bool> {
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

#[cfg(all(test, not(feature = "daemon-test-harness")))]
mod retain_tests {
    use super::*;
    #[test]
    fn a_deleted_bots_resume_nudge_candidate_is_dropped() {
        for bot in ["nudge-gone", "nudge-kept"] {
            armed().lock().unwrap().insert(bot.to_string(), Armed { run_id: "r".into(), idle_since: None });
        }
        retain_bots(&["nudge-kept".to_string()]);
        assert!(armed_run("nudge-gone").is_none() && armed_run("nudge-kept").is_some());
        retain_bots(&[]);
    }
}

#[cfg(all(test, not(feature = "daemon-test-harness")))]
mod tests {
    use super::*;
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
}

#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub mod test_seams {
    //! #1862 L2：daemon 測試要碰的私有項，窄轉呼叫（只在測試建置存在）。
    use super::*;
    pub fn armed_run(bot_id: &str) -> Option<String> { super::armed_run(bot_id) }
}
