//! 回合結束沒被偵測到、永遠停在 `in_flight` 的 turn，由這裡收尾（AGM 2026-09-16 交辦）。
//!
//! 正常收尾有兩條路：Stop hook（`hookrecv`）與 working→idle 那一邊的終端備援（[`super::arm_fallback`]）。
//! 兩條都會漏——hook 沒來；備援只收 `delivery='ok'` 的 turn、畫面上殘留 spinner 就放手、事件漏掉那一邊就根本沒排。
//! 漏掉的 turn 佔著 in_flight：同一對話的 queued 送不出去、還佔住「每對話一筆 queued」的名額，
//! 掛在上面的交辦停在 `delivered`。案例：k8bw2f `01M2MR95RXVHADA4TS8M0YHVH7`（09:20 起、09:59 已 idle、
//! 10:22 daemon 重啟才清，期間 6b84fa5 的部署交辦 409 七次被保險絲標 blocked）、
//! R-部署console-fork `01M2MG74HFY3PYD8FMBJKEJX8J`（AskUserQuestion 被中斷）、AGM-responder 09-15 08:49（卡 6.5 小時）。
//!
//! 規則：
//! - run 的 `agent_status` **持續** `idle` 超過門檻（預設 [`IDLE_MINS_DEFAULT`] 分鐘，環境變數 [`IDLE_MINS_ENV`]，
//!   看不懂／0／負數回預設）仍有 in_flight turn，才收。`working`／`blocked`（等人回答）一律不收；
//!   中間閃一下 working 就重算——herdr 的每個狀態事件都經過 [`observe`]，每輪掃描也拿 DB 的現況再對一次。
//! - 先讀 transcript（claude）／rollout（codex）：證得出「我們送的 prompt 之後已經有完整回覆」→ `completed` 並回填回覆；
//!   證不出 → `completed_fallback`，在對話裡寫明理由。**不標 failed**：回合多半是做完了，只是結束沒被看見。
//! - 收尾走 [`emit_turn`]（交辦照一般回合結束流程），並在同一把 bot 鎖裡立刻 flush 這顆 bot 的 queued。
//! - bot 鎖忙碌時立刻跳過這顆、下一輪再看，避免一顆握著鎖的 bot 延遲整輪掃描（#1201、#1264）。
//!
//! idle 計時放記憶體：daemon 重啟後從第一次看到 idle 重新算，寧可晚收，不要誤收。

#[cfg(all(test, feature = "daemon-test-harness"))]
use crate::capabilities::{BotLocks, Db};
use super::s6_ports::{StuckTurnContext, StuckTurnServices};
use super::*;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// idle 多久還沒收尾才算卡住。
pub const IDLE_MINS_DEFAULT: u64 = 5;
pub const IDLE_MINS_ENV: &str = "AM_STUCK_TURN_IDLE_MINS";
/// 定時掃描的間隔：reconcile 只在連線、agent 出現、子 pane 關掉時才跑，一顆靜靜 idle 的 bot 不會觸發它。
const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// transcript／rollout 只讀尾端這麼多：那個 prompt 如果不在這裡面，就當證不出來。
const LOG_TAIL_BYTES: u64 = 8 * 1024 * 1024;

pub fn parse_idle_mins(raw: Option<&str>) -> u64 {
    raw.and_then(|v| v.trim().parse::<i64>().ok()).filter(|m| *m > 0).map(|m| m as u64).unwrap_or(IDLE_MINS_DEFAULT)
}

pub fn idle_threshold() -> Duration {
    Duration::from_secs(parse_idle_mins(std::env::var(IDLE_MINS_ENV).ok().as_deref()) * 60)
}

fn idle_since() -> &'static Mutex<HashMap<String, Instant>> {
    static IDLE_SINCE: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    IDLE_SINCE.get_or_init(Default::default)
}

/// 記下 run 此刻的狀態。`idle` 只在第一次看到時起算；任何非 idle（working、blocked、unknown）把計時清掉。
pub fn observe(run_id: &str, agent_status: &str) {
    observe_at(run_id, agent_status, Instant::now());
}

pub fn observe_at(run_id: &str, agent_status: &str, at: Instant) {
    super::agy_busy::note_status(run_id, agent_status);
    let mut m = idle_since().lock().unwrap_or_else(|e| e.into_inner());
    if agent_status == "idle" {
        m.entry(run_id.to_string()).or_insert(at);
    } else {
        m.remove(run_id);
    }
}

/// 不在 `active` 裡的 run（結束了）不留計時：run 多半是 idle 時結束，只在「看到非 idle」才清的表會只增不減。
pub fn retain_runs(active: &[String]) {
    super::agy_busy::retain_runs(active);
    prune(&mut idle_since().lock().unwrap_or_else(|e| e.into_inner()), active);
}

/// 只留 `active` 裡的 run。獨立成函式讓單元測試用自己的表驗證：全域表被 `retain_runs(&[…])` 清掉的話，同時跑的其他測試剛記的計時會一起不見。
fn prune<V>(map: &mut HashMap<String, V>, active: &[String]) {
    map.retain(|id, _| active.contains(id));
}

/// 巡邏每輪把已經不 active 的 run 的行程級帳（idle 計時、閒置回收看的最後狀態）帶走。讀不到 DB 就這輪不清。
async fn forget_ended_runs(app: &(impl crate::capabilities::Db + StuckTurnServices)) {
    let ids: Result<Vec<String>, _> =
        sqlx::query_scalar(&format!("SELECT id FROM runs WHERE state IN {}", db::ACTIVE_STATES)).fetch_all(app.db()).await;
    match ids {
        Ok(ids) => {
            retain_runs(&ids);
            super::limit_banner::retain_runs(&ids);
            app.retain_supervisor_runs(&ids);
        }
        Err(e) => tracing::warn!(error = ?e, "could not list active runs; per-run idle state not pruned this round"),
    }
}

/// 從第一次看到 idle 到 `now` 過了多久；目前不是 idle（或還沒看過）是 `None`。
pub fn idle_for(run_id: &str, now: Instant) -> Option<Duration> {
    let m = idle_since().lock().unwrap_or_else(|e| e.into_inner());
    m.get(run_id).map(|since| now.saturating_duration_since(*since))
}

/// 這筆 turn 從**送達**（沒有 `delivered_at` 的舊列看建立時間）到 `now_wall` 過了多久；時間讀不懂就是 `None`。
fn turn_age(turn: &db::Turn, now_wall: chrono::DateTime<chrono::Utc>) -> Option<Duration> {
    let at = turn.delivered_at.as_deref().unwrap_or(&turn.created_at);
    let at = chrono::DateTime::parse_from_rfc3339(at).ok()?.with_timezone(&chrono::Utc);
    (now_wall - at).to_std().ok().or(Some(Duration::ZERO))
}

/// 這筆 turn 真的「送出之後一直沒動」多久：run 閒置多久，與這筆送達多久，**取短的**。
///
/// idle 計時是以 run 為單位、從第一次看到 idle 起算的。一顆已經閒置 15 分鐘的 bot 收到新 prompt，agent 還沒
/// 轉成 working 的那幾秒（貼上的圖片路徑正在轉成 `[Image #n]`、TUI 還沒重畫）run 仍是 idle——只看 run 的話，
/// 下一輪掃描就把**剛送出 9 秒**的回合當卡住收掉，連幫它補按 Enter 的 watchdog 都沒機會跑
/// （2026-09-16 13:17，AM-2-M turn `01M2N5T4R7Z4P9ARQWC4C27MEF`：`idle_mins=15`，建立 13:17:01、收掉 13:17:10）。
/// 時間讀不懂就當剛送出：寧可晚收，不要誤收。
pub fn stuck_for(run_idle: Duration, since_sent: Option<Duration>) -> Duration {
    run_idle.min(since_sent.unwrap_or(Duration::ZERO))
}

/// 把測試用的 `now: Instant` 換成同一刻的牆上時間（正式呼叫 `now` 就是 `Instant::now()`，差值是 0）。
fn wall_at(now: Instant) -> chrono::DateTime<chrono::Utc> {
    let real = Instant::now();
    let wall = chrono::Utc::now();
    match now.checked_duration_since(real) {
        Some(ahead) => wall + chrono::Duration::from_std(ahead).unwrap_or_default(),
        None => wall - chrono::Duration::from_std(real.saturating_duration_since(now)).unwrap_or_default(),
    }
}

/// 每 [`SWEEP_EVERY`] 掃一次所有主機。
pub fn spawn_stuck_turn_sweeper(app: impl StuckTurnContext) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(SWEEP_EVERY).await;
            super::dead_panes::sweep(&app).await;
            sweep(&app, None).await;
            forget_ended_runs(&app).await;
            revoke_all_orphaned_queued_turns(&app).await;
            app.sweep_child_alerts().await;
            app.sweep_child_done().await;
        }
    });
}

/// 收掉卡住的 turn。`host` 給了只看那一台（reconcile 那一輪用），`None` 全部。回傳收掉的 turn id。
pub async fn sweep(app: &impl StuckTurnContext, host: Option<&str>) -> Vec<String> {
    sweep_at(app, host, Instant::now(), idle_threshold()).await
}

pub async fn sweep_at(app: &impl StuckTurnContext, host: Option<&str>, now: Instant, threshold: Duration) -> Vec<String> {
    sweep_with(app, host, now, threshold).await
}

async fn sweep_with(app: &impl StuckTurnContext, host: Option<&str>, now: Instant, threshold: Duration) -> Vec<String> {
    let rows: Vec<(String, String, String, String)> = match sqlx::query_as(
        concat!("SELECT t.id, r.id, r.bot_id, r.agent_status
           FROM turns t
           JOIN runs r ON r.id = t.run_id
           JOIN bots b ON b.id = r.bot_id
           JOIN projects p ON p.id = b.project_id
          WHERE t.status = 'in_flight' AND r.state = 'running' AND (? IS NULL OR p.host = ?)
            AND p.handed_off_to IS NULL
            AND b.kind NOT IN ", crate::config::api_kinds_sql!(), "
          ORDER BY t.rowid"),
    )
    .bind(host)
    .bind(host)
    .fetch_all(app.db())
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = ?e, "stuck turn sweep: could not list in-flight turns");
            return Vec::new();
        }
    };
    let mut closed = Vec::new();
    for (turn_id, run_id, bot_id, status) in rows {
        observe_at(&run_id, &status, now);
        if !idle_for(&run_id, now).is_some_and(|d| d >= threshold) {
            continue;
        }
        let bot_lock = app.bot_lock(&bot_id).await;
        let Ok(_g) = bot_lock.try_lock() else {
            tracing::warn!(bot = %bot_id, turn = %turn_id, "stuck turn sweep: bot lock is busy; skipping this bot until the next round");
            continue;
        };
        let Ok(Some(run)) = db::run(app.db(), &run_id).await else { continue };
        observe_at(&run_id, &run.agent_status, now);
        if run.state != "running" || run.agent_status != "idle" {
            continue;
        }
        let Some(idle) = idle_for(&run_id, now).filter(|d| *d >= threshold) else { continue };
        let Ok(Some(turn)) = db::in_flight_turn(app.db(), &run_id).await else { continue };
        if turn.id != turn_id {
            continue;
        }
        if hook_active_within(app, &bot_id, threshold).await {
            observe_at(&run_id, "working", now);
            continue;
        }
        if db::bot(app.db(), &bot_id).await.ok().flatten().is_some_and(|b| b.kind == "agy") {
            let screen = super::unsent_draft::read_screen(app, &run).await.unwrap_or_default();
            if super::agy_busy::should_hold(&run_id, &turn.id, &screen) {
                tracing::debug!(turn = %turn.id, "agy is not fully idle; stuck-turn sweep leaves the turn in flight");
                continue;
            }
        }
        let stuck = stuck_for(idle, turn_age(&turn, wall_at(now)));
        if stuck < threshold {
            continue;
        }
        match close_stuck(app, &run, &turn, CloseWhy::Idle(stuck)).await {
            Ok(true) => {
                closed.push(turn.id.clone());
                if let Err(e) = super::flush_queued_locked(app, &bot_id).await {
                    tracing::warn!(bot = %bot_id, error = ?e, "queued prompt flush after closing a stuck turn failed");
                }
            }
            Ok(false) => {}
            Err(e) => tracing::warn!(turn = %turn.id, error = ?e, "could not close a stuck turn"),
        }
    }
    closed
}

/// 這顆 bot 在門檻內有沒有收到過回合中的 hook（工具、子代理、Stop…；不含 SessionStart 這種開場事件）。以 daemon 處理的時間
/// （`processed_at`，本機鐘）判斷，遠端主機的鐘不準也不影響。收件匣表不在（舊 DB）或查不到當作沒有。
async fn hook_active_within(app: &impl StuckTurnContext, bot_id: &str, threshold: Duration) -> bool {
    let since = db::iso_in(-(threshold.as_secs() as i64));
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM hook_events WHERE bot_id = ? AND processed_at >= ?
            AND COALESCE(json_extract(body_json, '$.payload.hook_event_name'), '') NOT IN ('SessionStart', 'StatusLine'))",
    )
    .bind(bot_id)
    .bind(&since)
    .fetch_one(app.db())
    .await
    .unwrap_or(false)
}

/// 為什麼收：閒置太久（一般），或 claude 的 Session paused 選單剛關掉、沒有回覆（不必等滿閒置門檻）。
#[derive(Clone, Copy)]
pub enum CloseWhy {
    Idle(Duration),
    SessionPaused,
}

/// Session paused 選單關掉之後：run 還是 idle、選單關閉時捕捉的 turn 仍是這個 run 的 in-flight，才馬上收，
/// 不等 5 分鐘的閒置門檻（2026-09-26 使用者：「判斷為等待中，實際上並沒有再輸出」）。選了「換模型重試」的話
/// claude 會接著跑同一回合、herdr 報 working，這裡在鎖內重讀看到不是 idle 就不動。舊 turn 已結束或 turn id 改變也不動。
/// 回傳收掉的 turn id。
pub async fn close_after_session_paused(app: &impl StuckTurnContext, run_id: &str, expected_turn_id: &str) -> Option<String> {
    let run = db::run(app.db(), run_id).await.ok()??;
    let bot_lock = app.bot_lock(&run.bot_id).await;
    let _g = bot_lock.lock().await;
    let run = db::run(app.db(), run_id).await.ok()??;
    if run.state != "running" || run.agent_status != "idle" {
        return None;
    }
    let bot = db::bot(app.db(), &run.bot_id).await.ok()??;
    if crate::config::is_api_kind(&bot.kind) {
        return None;
    }
    let turn = db::in_flight_turn(app.db(), run_id).await.ok()??;
    if turn.id != expected_turn_id {
        return None;
    }
    match close_stuck(app, &run, &turn, CloseWhy::SessionPaused).await {
        Ok(true) => {
            if let Err(e) = super::flush_queued_locked(app, &run.bot_id).await {
                tracing::warn!(bot = %run.bot_id, error = ?e, "queued prompt flush after a session-paused close failed");
            }
            Some(turn.id)
        }
        Ok(false) => None,
        Err(e) => {
            tracing::warn!(turn = %turn.id, error = ?e, "could not close the turn after the session-paused menu closed");
            None
        }
    }
}

/// 卡住的回合怎麼收：`delivery='pending'` ＝ 從來沒送出去（送達結果還沒寫、也沒證據字進過 pane），
/// 不能收成 `completed_fallback`（那等於說「agent 做完了」），收成 failed 請使用者重送（#903）。
/// `unknown` 若仍未驗證，收成 failed 並保留 unknown；已確認送達才可走完成備援。
async fn close_stuck(app: &impl StuckTurnContext, run: &db::Run, turn: &db::Turn, why: CloseWhy) -> anyhow::Result<bool> {
    if let Ok(Some(bot)) = db::bot(app.db(), &run.bot_id).await {
        if crate::config::is_api_kind(&bot.kind) {
            return Ok(false);
        }
    }
    if turn.delivery == "pending" {
        return fail_never_sent(app, run, turn).await;
    }
    if matches!(why, CloseWhy::Idle(_)) {
        match super::unsent_draft::before_idle_close(app, run, turn).await? {
            super::unsent_draft::Recovery::Proceed => {}
            super::unsent_draft::Recovery::Wait => return Ok(false),
            super::unsent_draft::Recovery::Failed => return Ok(true),
        }
    }
    close_locked(app, run, turn, why).await
}

/// 從未送出的 in_flight 回合：failed＋`delivery='failed'`＋一則系統訊息，同一個交易。`Ok(false)`：別的路先收掉了。
async fn fail_never_sent(app: &impl StuckTurnContext, run: &db::Run, turn: &db::Turn) -> anyhow::Result<bool> {
    let Some(bot) = db::bot(app.db(), &run.bot_id).await? else { return Ok(false) };
    if crate::config::is_api_kind(&bot.kind) {
        return Ok(false);
    }
    let mut tx = app.db().begin().await?;
    let out = super::turn_controller::fail_on(&mut tx, &turn.id, super::turn_controller::DeliveryOnFail::Failed, "送出前就卡住").await?;
    if out != super::turn_controller::Outcome::Applied {
        return Ok(false);
    }
    let note = "這一則在送出前就卡住（一個字都沒送出），由 reconcile 收成失敗；請重送。";
    let m = insert_message_tx(&mut tx, &turn.conversation_id, Some(&turn.id), "system", note, "system", false, None).await?;
    tx.commit().await?;
    tracing::warn!(turn = %turn.id, bot = %bot.name, "closed a never-sent turn stuck in flight as failed");
    emit_message_added(app, &bot.id, m).await;
    app.emit_turn(&turn.id).await;
    Ok(true)
}

async fn fail_unconfirmed(app: &impl StuckTurnContext, run: &db::Run, turn: &db::Turn) -> anyhow::Result<bool> {
    let Some(bot) = db::bot(app.db(), &run.bot_id).await? else { return Ok(false) };
    if crate::config::is_api_kind(&bot.kind) {
        return Ok(false);
    }
    let mut tx = app.db().begin().await?;
    let out = super::turn_controller::fail_on(&mut tx, &turn.id, super::turn_controller::DeliveryOnFail::Keep, "送出未確認").await?;
    if out != super::turn_controller::Outcome::Applied { return Ok(false) }
    let m = insert_message_tx(&mut tx, &turn.conversation_id, Some(&turn.id), "system", "送出未確認：無法證明這則訊息已送出或收到回覆。請檢查終端輸入框，再決定是否送出；不要盲目重送。", "system", false, None).await?;
    tx.commit().await?;
    emit_message_added(app, &bot.id, m).await;
    app.emit_turn(&turn.id).await;
    Ok(true)
}

/// 收一筆。`Ok(false)`：CAS 沒搶到（別的路剛好收掉了）。
async fn close_locked(app: &impl StuckTurnContext, run: &db::Run, turn: &db::Turn, why: CloseWhy) -> anyhow::Result<bool> {
    let Some(bot) = db::bot(app.db(), &run.bot_id).await? else { return Ok(false) };
    if crate::config::is_api_kind(&bot.kind) {
        return Ok(false);
    }
    let reply = proven_reply(app, &bot, run, turn).await?;
    if turn.delivery == "unknown" && turn.delivery_verified == 0 {
        return fail_unconfirmed(app, run, turn).await;
    }
    let already_answered: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM messages WHERE turn_id = ? AND role = 'assistant')")
            .bind(&turn.id)
            .fetch_one(app.db())
            .await?;
    let mins = match why {
        CloseWhy::Idle(idle) => idle.as_secs() / 60,
        CloseWhy::SessionPaused => 0,
    };
    let status = if reply.is_some() { "completed" } else { "completed_fallback" };
    let quiet = reply.is_none() && matches!(why, CloseWhy::Idle(_)) && compaction_only(app, &bot.id, turn).await?;

    let mut tx = app.db().begin().await?;
    let claimed =
        super::turn_controller::set_status_on(&mut tx, &turn.id, "in_flight", status, "reconcile 收掉閒置太久的回合").await?;
    if claimed != super::turn_controller::Outcome::Applied {
        return Ok(false);
    }
    let message = match reply.as_deref() {
        Some(_) if already_answered => None,
        Some(text) => Some(insert_message_tx(&mut tx, &turn.conversation_id, Some(&turn.id), "assistant", text, "transcript", false, None).await?),
        None if quiet => None,
        None => {
            let why = match why {
                CloseWhy::SessionPaused => "claude 的 Session paused 選單關掉了、這個回合沒有回覆（被暫停），直接收尾，不再顯示等待中。".to_string(),
                CloseWhy::Idle(_) => format!(
                    "這個回合沒有偵測到結束：agent 已經閒置 {mins} 分鐘，turn 仍是 in_flight，由 reconcile 收尾。\
                     transcript 證不出這則 prompt 之後有完整回覆，所以標成 completed_fallback（不是失敗）。"
                ),
            };
            Some(insert_message_tx(&mut tx, &turn.conversation_id, Some(&turn.id), "system", &why, "system", false, None).await?)
        }
    };
    tx.commit().await?;
    tracing::warn!(turn = %turn.id, bot = %bot.name, status, idle_mins = mins, "closed a turn stuck in flight after the agent went idle");
    if let Some(m) = message {
        emit_message_added(app, &bot.id, m).await;
    }
    app.emit_turn(&turn.id).await;
    Ok(true)
}

/// #1758：claude 閒置時自己壓縮對話，pane 轉 working 開了一筆 external 空回合；壓縮只送 SubagentStop 與
/// `SessionStart`（source=compact），不送 Stop，閒置門檻到了由這裡收。這筆回合沒有 prompt、沒有任何訊息，
/// 回合開始後又收過 compact 的 SessionStart，就是 CLI 自己的壓縮，不是使用者的回合沒結束，不必在對話裡留說明。
async fn compaction_only(app: &impl StuckTurnContext, bot_id: &str, turn: &db::Turn) -> anyhow::Result<bool> {
    if turn.origin != "external" || turn.prompt_text.as_deref().is_some_and(|p| !p.trim().is_empty()) {
        return Ok(false);
    }
    let compacted: bool = sqlx::query_scalar(
        "SELECT NOT EXISTS(SELECT 1 FROM messages WHERE turn_id = ?)
            AND EXISTS(SELECT 1 FROM hook_events WHERE bot_id = ? AND received_at >= ?
                AND json_extract(body_json, '$.payload.hook_event_name') = 'SessionStart'
                AND json_extract(body_json, '$.payload.source') = 'compact')",
    )
    .bind(&turn.id)
    .bind(bot_id)
    .bind(&turn.created_at)
    .fetch_one(app.db())
    .await
    .unwrap_or(false);
    Ok(compacted)
}

/// 讀 transcript／rollout 尾端：我們送的 prompt 之後，agent 有沒有完整回完。本機 claude／codex 才讀得到。
///
/// 讀不到主機、讀不到送了什麼都回錯（#193），這一輪不收、下一輪再看：以前當成「證不出回覆」，照樣收成
/// `completed_fallback`，transcript 裡那份完整的回覆就沒存。
async fn proven_reply(app: &impl StuckTurnContext, bot: &db::Bot, run: &db::Run, turn: &db::Turn) -> anyhow::Result<Option<String>> {
    if db::bot_host(app.db(), &bot.id).await? != LOCAL_HOST {
        return Ok(None);
    }
    let mut sent = turn_echo_texts(app, &turn.id).await?;
    if let Some(p) = turn.prompt_text.as_deref().filter(|p| !p.trim().is_empty()) {
        sent.push(p.to_string());
    }
    if sent.is_empty() {
        return Ok(None);
    }
    Ok(logged_reply(app, bot, run, &sent).await)
}

async fn logged_reply(app: &impl StuckTurnContext, bot: &db::Bot, run: &db::Run, sent: &[String]) -> Option<String> {
    match bot.kind.as_str() {
        "claude" => {
            let raw_path = run.transcript_path.as_deref().filter(|p| !p.trim().is_empty())?;
            if !app.local_transcript_allowed(bot, raw_path).await {
                return None;
            }
            let path = std::path::PathBuf::from(raw_path);
            let log = tokio::task::spawn_blocking(move || read_tail(&path, LOG_TAIL_BYTES)).await.ok()??;
            claude_reply_after(&log, sent)
        }
        "codex" => {
            if let Some(reply) = app.codex_exact_reply(bot, run, sent).await {
                return Some(reply);
            }
            let home = app.codex_home(bot).await?;
            let session = run.native_session_id.clone().filter(|s| !s.trim().is_empty())?;
            let log = tokio::task::spawn_blocking(move || {
                let path = codex_session_log(&home, &session)?;
                read_tail(&path, LOG_TAIL_BYTES)
            })
            .await
            .ok()??;
            let reply = codex_reply_after(&log, sent);
            if reply.is_some() {
                crate::codex_history::note("reply", crate::codex_history::Evidence::Rollout);
            }
            reply
        }
        _ => None,
    }
}

/// 檔案最後 `max` 個位元組（第一行可能被切到一半，解析時自然會跳過）。
fn read_tail(path: &std::path::Path, max: u64) -> Option<String> {
    crate::transcript_read::read_tail(path, max)
}

/// claude transcript：最後一次出現我們送的 prompt 之後，有 `stop_reason: end_turn` 的 assistant 訊息＝回合真的結束，
/// 回它的文字。之間又出現別的 user prompt（下一個回合）、或只有 tool_use（還在做）、或被中斷沒有 end_turn，都是 `None`。
pub fn claude_reply_after(log: &str, sent: &[String]) -> Option<String> {
    let lines: Vec<&str> = log.lines().collect();
    let start = lines
        .iter()
        .rposition(|l| transcript_user_text(l).is_some_and(|t| sent.iter().any(|s| super::pasted_content::is_sent(&t, s))))?;
    for line in &lines[start + 1..] {
        if transcript_user_text(line).is_some() {
            return None;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if v.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let msg = v.get("message")?;
        if msg.get("stop_reason").and_then(Value::as_str) != Some("end_turn") {
            continue;
        }
        let text = msg
            .get("content")
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        if !text.trim().is_empty() {
            return Some(text);
        }
    }
    None
}

/// codex rollout：最後一次出現我們送的 prompt 之後，有 `event_msg`／`task_complete` 且帶 `last_agent_message`＝回合結束，
/// 回那句話。`last_agent_message` 是空的（例如撞額度，只有 `error`）不算完整回覆。
pub fn codex_reply_after(log: &str, sent: &[String]) -> Option<String> {
    let lines: Vec<&str> = log.lines().collect();
    let start = lines.iter().rposition(|l| codex_user_text(l).is_some_and(|t| sent.iter().any(|s| s == &t)))?;
    for line in &lines[start + 1..] {
        if codex_user_text(line).is_some() {
            return None;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let p = v.get("payload");
        if v.get("type").and_then(Value::as_str) != Some("event_msg") || p.and_then(|p| p.get("type")).and_then(Value::as_str) != Some("task_complete") {
            continue;
        }
        return p
            .and_then(|p| p.get("last_agent_message"))
            .and_then(Value::as_str)
            .filter(|t| !t.trim().is_empty())
            .map(str::to_string);
    }
    None
}

#[cfg(all(test, not(feature = "daemon-test-harness")))]
mod tests {
    use super::*;

    const MIN: Duration = Duration::from_secs(60);

    fn claude_user(text: &str) -> String {
        json!({"type": "user", "message": {"role": "user", "content": text}}).to_string()
    }

    fn claude_assistant(stop: &str, parts: Value) -> String {
        json!({"type": "assistant", "message": {"role": "assistant", "stop_reason": stop, "content": parts}}).to_string()
    }

    /// run 結束時多半是 idle：只在「看到非 idle」才清的計時表，結束的 run 會一直留著（run id 每次都是新的，只增不減）。
    /// 巡邏每輪把不再 active 的 run 帶走（呼叫端傳的是 DB 裡還活著的 run id）。
    #[test]
    fn an_ended_runs_idle_timer_is_dropped() {
        let mut timers: HashMap<String, Instant> = HashMap::new();
        timers.insert("idle-timer-gone".into(), Instant::now());
        timers.insert("idle-timer-kept".into(), Instant::now());
        prune(&mut timers, &["idle-timer-kept".to_string()]);
        assert!(!timers.contains_key("idle-timer-gone"), "結束的 run 不留計時");
        assert!(timers.contains_key("idle-timer-kept"), "還活著的 run 的計時不動");
    }

    #[test]
    fn a_bad_threshold_falls_back_to_five_minutes() {
        assert_eq!(parse_idle_mins(None), 5);
        assert_eq!(parse_idle_mins(Some("12")), 12);
        assert_eq!(parse_idle_mins(Some(" 3 ")), 3);
        for bad in ["0", "-4", "five", "", "1.5"] {
            assert_eq!(parse_idle_mins(Some(bad)), 5, "{bad:?}");
        }
    }

    #[test]
    fn a_claude_turn_is_proven_only_by_an_end_turn_after_our_prompt() {
        let sent = vec!["部署 6b84fa5".to_string()];
        let done = [
            claude_user("舊的問題"),
            claude_assistant("end_turn", json!([{"type": "text", "text": "舊的回答"}])),
            claude_user("部署 6b84fa5"),
            claude_assistant("tool_use", json!([{"type": "text", "text": "我先看一下"}])),
            claude_assistant("tool_use", json!([{"type": "tool_use", "name": "Bash"}])),
            json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "content": "ok"}]}}).to_string(),
            claude_assistant("end_turn", json!([{"type": "text", "text": "部署完成：pid 42894"}])),
            json!({"type": "system", "subtype": "stop_hook_summary"}).to_string(),
        ]
        .join("\n");
        assert_eq!(claude_reply_after(&done, &sent).as_deref(), Some("部署完成：pid 42894"), "最後那句，不是中途的旁白");

        let interrupted = [claude_user("部署 6b84fa5"), claude_assistant("tool_use", json!([{"type": "tool_use", "name": "AskUserQuestion"}]))].join("\n");
        assert_eq!(claude_reply_after(&interrupted, &sent), None);
        let only_old = [claude_user("舊的問題"), claude_assistant("end_turn", json!([{"type": "text", "text": "舊的回答"}])), claude_user("部署 6b84fa5")].join("\n");
        assert_eq!(claude_reply_after(&only_old, &sent), None);
        let next = [claude_user("部署 6b84fa5"), claude_user("另一件事"), claude_assistant("end_turn", json!([{"type": "text", "text": "另一件事的回答"}]))].join("\n");
        assert_eq!(claude_reply_after(&next, &sent), None);
        assert_eq!(claude_reply_after(&done, &["沒送過".to_string()]), None);
    }

    #[test]
    fn a_turn_is_stuck_only_as_long_as_both_the_run_and_the_turn_have_been_quiet() {
        assert_eq!(stuck_for(15 * MIN, Some(Duration::from_secs(9))), Duration::from_secs(9));
        assert_eq!(stuck_for(6 * MIN, Some(20 * MIN)), 6 * MIN, "turn 早就送出、run 最近才閒置：看 run");
        assert_eq!(stuck_for(15 * MIN, None), Duration::ZERO, "時間讀不懂就當剛送出，不收");
    }
}

#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub mod test_seams {
    use super::*;
    pub async fn sweep_with(app: &impl StuckTurnContext, host: Option<&str>, now: Instant, threshold: Duration) -> Vec<String> { super::sweep_with(app, host, now, threshold).await }
    pub async fn close_locked(app: &impl StuckTurnContext, run: &db::Run, turn: &db::Turn, why: CloseWhy) -> anyhow::Result<bool> { super::close_locked(app, run, turn, why).await }
    pub async fn logged_reply(app: &impl StuckTurnContext, bot: &db::Bot, run: &db::Run, sent: &[String]) -> Option<String> { super::logged_reply(app, bot, run, sent).await }
}
