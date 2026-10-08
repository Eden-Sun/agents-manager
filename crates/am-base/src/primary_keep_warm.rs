//! 主力 bot 的 prompt cache 保溫（58 分）與熱壓（110 分）（SPEC §6.5k，使用者 2026-10-04）。名詞：58 分那則叫**保溫**（keep-warm），
//! bot 對它的回覆叫**保溫回覆**，110 分的壓縮叫**熱壓**（warm compact），過了 TTL 叫**涼掉**（cold）。
//!
//! 使用者原話：「主力的規則如下：當 cache 至 58 分鐘時，下一個 prompt "any updates"，來讓 cache 延長，且 cache 時間照樣上數，
//! 達 110 分鐘時，強制做壓縮」。
//!
//! * **活動年齡**＝距離 [`cache_clock`] 推算的最後一次**真的**活動（`last_api_at`）。保溫回合本身不算活動
//!   （`turns.client_request_id` 以 [`KEEP_WARM_CRID_PREFIX`] 開頭，`cache_clock` 的最近回合 SQL 排除它；保溫期間的 statusLine 指紋變化
//!   由 [`window_open`] 擋掉），所以送完保溫年齡照樣往上數，直到下一次真的活動才歸零。
//!   cache 熱度則以 `max(last_api_at, cache_kept_warm_at)` 計算；時間來源沿用 [`cache_clock`]。
//! * 年齡 ≥ [`KEEP_WARM_AFTER_SECS`]、cache 未過 TTL、這個錨點之後還沒保溫過：透過 `TurnControl` 送
//!   [`KEEP_WARM_TEXT`]，`client_request_id = keep-warm:<錨點>`（同一錨點冪等）。保溫錯過 TTL 就不補送。
//! * 年齡 ≥ [`WARM_COMPACT_AFTER_SECS`]、cache 最近一次變熱未滿 TTL 並保留 2 分鐘餘裕、這個錨點之後還沒壓縮過：透過
//!   `TurnControl` 呼叫 `/compact`，並以 `SystemMessageWriter` 記下聊天室說明。cache 冷了就不壓縮，等真的活動重新計時。
//! * 對象只有主力（`bots.is_primary`）的 claude／codex，run 在跑、`agent_status == idle`、沒有在飛或排隊的回合。`blocked`（停在問題／
//!   權限提示）絕不送——打進去的字會變成回答那個問題。
//! * **不用保溫**（`keep_warm_skip` 表，使用者按鈕）：這顆主力「這一輪閒置」跳過保溫與熱壓；錨點（真的活動）晚於按下的時間就自動恢復，
//!   持久（daemon 重啟不忘）。
//!
//! 「這個錨點之後是否做過」不放記憶體：保溫看有沒有 `keep-warm:`（舊資料 `keepalive:`）回合比錨點晚；壓縮以 `primary_cache_actions`
//! 的收據為準（先 claim 再送，同一錨點只會有一列；舊資料另看比錨點晚的系統訊息 [`WARM_COMPACT_NOTE_PREFIX`]）。系統訊息只是聊天室看得到的
//! 說明：寫失敗會補寫（[`retry_pending_notes`]），不會因此重壓。daemon 重啟後不會重做。

use crate::cache_clock::{self, KEEP_WARM_CRID_PREFIX, WARM_COMPACT_NOTE_PREFIX};
use crate::db;
use am_core::{PromptRequest, TurnError};
use am_ports::{Clock, DbContext, EventSink, SystemMessageWriter, TurnControl};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 閒置到這麼久（活動年齡，秒）就考慮保溫。TTL 是 60 分鐘，留 2 分鐘餘裕。
pub const KEEP_WARM_AFTER_SECS: i64 = 58 * 60;
/// 年齡到這麼久才考慮壓縮；仍須確認 cache 熱度足夠。
pub const WARM_COMPACT_AFTER_SECS: i64 = 110 * 60;
/// 熱壓只在 context 用量**超過**這個百分比時才做（使用者 2026-10-05）：用量小的 context 重新建 cache 很便宜，不值得壓縮；
/// ≤ 這個值（或讀不到用量）到 110 分就什麼都不做，讓它涼掉。claude 的門檻。
pub const WARM_COMPACT_MIN_CONTEXT_PCT_CLAUDE: f64 = 30.0;
/// codex 的門檻（使用者同意）：視窗 258K、涼掉後重讀沒有寫入加價，損益點約 35%，所以比 claude 高。
pub const WARM_COMPACT_MIN_CONTEXT_PCT_CODEX: f64 = 50.0;

/// 這個 kind 熱壓要超過的 context 用量百分比；沒有熱壓的 kind 回 `None`。
pub fn warm_compact_min_context_pct(kind: &str) -> Option<f64> {
    match kind {
        "claude" => Some(WARM_COMPACT_MIN_CONTEXT_PCT_CLAUDE),
        "codex" => Some(WARM_COMPACT_MIN_CONTEXT_PCT_CODEX),
        _ => None,
    }
}
/// 壓縮至少要在 cache TTL 到期前留這麼多時間。
const WARM_COMPACT_TTL_MARGIN_SECS: i64 = 2 * 60;
/// 送進去的保溫 prompt。
pub const KEEP_WARM_TEXT: &str = "any updates";
/// 保溫視窗至少開這麼久才允許被關（送出到 hook 回報 working 之間有空窗）。
pub const WINDOW_MIN: Duration = Duration::from_secs(60);
/// 視窗最多開這麼久（保險：回合沒收尾也不能永遠擋住 statusLine 活動）。
pub const WINDOW_MAX: Duration = Duration::from_secs(15 * 60);
/// 回合收尾後，statusLine 還會再來幾次重繪；視窗再撐這麼久才關。
pub const WINDOW_GRACE: Duration = Duration::from_secs(20);
/// 熱壓之後這麼久內冒出來的「活動」算壓縮自己造成的（`/compact` 讓 statusLine 指紋變、hook 報 working），
/// 不當成新的錨點；否則閒置的主力每兩小時就保溫＋壓縮一輪，永遠停不下來。真的活動晚於這段才重新計時。
pub const WARM_COMPACT_ECHO_SECS: i64 = 10 * 60;

#[derive(Debug, Clone, Copy)]
struct Window {
    opened: Instant,
    closed: Option<Instant>,
}

fn windows() -> &'static Mutex<HashMap<String, Window>> {
    static S: OnceLock<Mutex<HashMap<String, Window>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

#[cfg(feature = "test-hooks")]
#[doc(hidden)]
pub fn age_window_opened_for_test(run_id: &str, age: Duration) {
    if let Some(window) = windows().lock().unwrap_or_else(|e| e.into_inner()).get_mut(run_id) {
        window.opened = Instant::now() - age;
    }
}

#[cfg(feature = "test-hooks")]
#[doc(hidden)]
pub fn age_window_closed_for_test(run_id: &str, age: Duration) {
    if let Some(window) = windows().lock().unwrap_or_else(|e| e.into_inner()).get_mut(run_id) {
        window.closed = Some(Instant::now() - age);
    }
}

fn is_open(w: &Window, now: Instant) -> bool {
    now.checked_duration_since(w.opened).is_some_and(|d| d < WINDOW_MAX)
        && w.closed.is_none_or(|c| now.checked_duration_since(c).is_none_or(|d| d < WINDOW_GRACE))
}

/// 這個 run 現在是否在保溫回合的視窗裡（送出保溫起、回合收尾後再寬限一下）。視窗裡的 statusLine API 指紋變化與 `working` 都不算活動。
pub fn window_open(run_id: &str) -> bool {
    windows().lock().unwrap_or_else(|e| e.into_inner()).get(run_id).is_some_and(|w| is_open(w, Instant::now()))
}

pub fn open_window(run_id: &str) {
    windows().lock().unwrap_or_else(|e| e.into_inner()).insert(run_id.to_string(), Window { opened: Instant::now(), closed: None });
}

pub fn drop_window(run_id: &str) {
    windows().lock().unwrap_or_else(|e| e.into_inner()).remove(run_id);
}

/// 巡邏看到保溫回合已經收尾（閒置、沒有回合在飛）：開始倒數寬限。視窗開著不到 [`WINDOW_MIN`] 不關。
pub fn settle_window(run_id: &str, idle_and_quiet: bool) {
    let mut m = windows().lock().unwrap_or_else(|e| e.into_inner());
    let Some(w) = m.get_mut(run_id) else { return };
    let now = Instant::now();
    if idle_and_quiet && w.closed.is_none() && now.checked_duration_since(w.opened).is_some_and(|d| d >= WINDOW_MIN) {
        w.closed = Some(now);
    }
    if !is_open(w, now) {
        m.remove(run_id);
    }
}

/// 不在 `active` 裡的 run（結束了）不留視窗（`cache_clock::retain_runs` 順手呼叫）。
#[cfg_attr(test, allow(dead_code))]
pub fn retain_runs(active: &[String]) {
    windows().lock().unwrap_or_else(|e| e.into_inner()).retain(|id, _| active.contains(id));
}

/// 這一輪對一顆主力該做什麼。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Wait,
    KeepWarm,
    WarmCompact,
}

/// 純規則：錨點是不是熱壓自己的回音（壓縮後 [`WARM_COMPACT_ECHO_SECS`] 內）。是的話這一輪什麼都不做。
pub fn is_warm_compact_echo(anchor: chrono::DateTime<chrono::Utc>, last_compact: Option<chrono::DateTime<chrono::Utc>>) -> bool {
    last_compact.is_some_and(|c| anchor >= c - chrono::Duration::seconds(5) && anchor <= c + chrono::Duration::seconds(WARM_COMPACT_ECHO_SECS))
}

/// 這顆 run 現在的 context 用量百分比（0–100）。claude 讀 statusLine 的 `context_window.used_percentage`（`runs.status_json`），
/// codex 讀 rollout 最近一筆 `token_count`（[`crate::prompt_cache::codex_context_pct`]）；讀不到回 `None`。
pub fn context_used_pct(kind: &str, run_id: &str, status_json: Option<&str>) -> Option<f64> {
    match kind {
        "claude" => status_json
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())?
            .pointer("/context_window/used_percentage")?
            .as_f64(),
        "codex" => crate::prompt_cache::codex_context_pct(run_id),
        _ => None,
    }
}

/// 純規則：活動年齡、cache 實際熱度年齡與「這個錨點之後做過沒」決定下一步。
/// 保溫只能在 58 分至 TTL 之間送；壓縮要在 cache 到期前保留 [`WARM_COMPACT_TTL_MARGIN_SECS`] 餘裕，
/// 而且 context 用量要超過 `min_context_pct`（[`warm_compact_min_context_pct`]，依 kind；讀不到用量保守不壓縮）。
/// 110 分之後不再保溫：已經涼掉（或不值得壓縮）的就讓它涼，直到真的活動重新計時。
pub fn decide(
    age_secs: i64,
    cache_age_secs: i64,
    ttl_secs: i64,
    kept_since_anchor: bool,
    compacted_since_anchor: bool,
    context_pct: Option<f64>,
    min_context_pct: f64,
) -> Step {
    if age_secs >= WARM_COMPACT_AFTER_SECS {
        if !compacted_since_anchor
            && cache_age_secs < ttl_secs - WARM_COMPACT_TTL_MARGIN_SECS
            && context_pct.is_some_and(|p| p > min_context_pct)
        {
            Step::WarmCompact
        } else {
            Step::Wait
        }
    } else if age_secs >= KEEP_WARM_AFTER_SECS
        && age_secs < ttl_secs
        && cache_age_secs < ttl_secs
        && !kept_since_anchor
    {
        Step::KeepWarm
    } else {
        Step::Wait
    }
}

/// 純規則：這顆 run 現在能不能被碰。`blocked`／`working`／`unknown` 一律不送。
pub fn eligible(kind: &str, is_primary: bool, run_state: &str, agent_status: &str, in_flight: bool, queued: bool) -> bool {
    is_primary && matches!(kind, "claude" | "codex") && run_state == "running" && agent_status == "idle" && !in_flight && !queued
}

/// 保溫回合的 `client_request_id`：同一個錨點只有一個。
pub fn keep_warm_crid(anchor: &str) -> String {
    format!("{KEEP_WARM_CRID_PREFIX}{anchor}")
}

/// 一次判斷的結果：下一步與它所依據的錨點（最後一次真的活動）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub step: Step,
    pub anchor: String,
    pub age_secs: i64,
}

async fn latest(db: &DbContext<SqlitePool>, sql: &str, bot_id: &str) -> anyhow::Result<Option<chrono::DateTime<chrono::Utc>>> {
    let at: Option<String> = sqlx::query_scalar(sql).bind(bot_id).fetch_one(db.pool()).await?;
    Ok(at.as_deref().and_then(db::parse_ts))
}

/// 最後一次**真的**活動（錨點，[`cache_clock::derive`]）與最近回合。計畫與跳過狀態收斂共用。
async fn anchor_of(
    db: &DbContext<SqlitePool>,
    bot: &db::Bot,
    run: &db::Run,
    now: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<(Option<String>, Option<cache_clock::LastTurn>)> {
    let last_turn = cache_clock::last_turn_for_bot(db.pool(), &bot.id).await?;
    let line = cache_clock::statusline_at(&run.id);
    let anchor = cache_clock::derive(
        &bot.kind,
        Some(&run.agent_status),
        run.agent_status_since.as_deref(),
        last_turn.as_ref(),
        line.as_deref(),
        &db::iso_at(now),
    );
    Ok((anchor, last_turn))
}

/// 「不用保溫」按下的時間；沒按（或已恢復）是 `None`。存在 `keep_warm_skip` 表，daemon 重啟不忘。
pub async fn skip_since(pool: &sqlx::SqlitePool, bot_id: &str) -> anyhow::Result<Option<chrono::DateTime<chrono::Utc>>> {
    let at: Option<String> = sqlx::query_scalar("SELECT since FROM keep_warm_skip WHERE bot_id = ?").bind(bot_id).fetch_optional(pool).await?;
    Ok(at.as_deref().and_then(db::parse_ts))
}

/// 設定／取消「不用保溫」。回傳狀態是否真的變了（再按同一個值是 no-op，不廣播）。
pub async fn set_skip(pool: &sqlx::SqlitePool, bot_id: &str, skip: bool) -> anyhow::Result<bool> {
    let n = if skip {
        sqlx::query("INSERT INTO keep_warm_skip (bot_id, since) VALUES (?, ?) ON CONFLICT(bot_id) DO NOTHING")
            .bind(bot_id)
            .bind(db::now())
            .execute(pool)
            .await?
            .rows_affected()
    } else {
        sqlx::query("DELETE FROM keep_warm_skip WHERE bot_id = ?").bind(bot_id).execute(pool).await?.rows_affected()
    };
    Ok(n > 0)
}

/// 真的活動（錨點晚於按下的時間）→ 自動恢復保溫並廣播。保溫／熱壓自己造成的活動（視窗開著）不算。
pub async fn settle_skip_with<E: EventSink>(db: &DbContext<SqlitePool>, events: &E, bot: &db::Bot, run: &db::Run, now: chrono::DateTime<chrono::Utc>) {
    let Ok(Some(since)) = skip_since(db.pool(), &bot.id).await else { return };
    if window_open(&run.id) {
        return;
    }
    let Ok((Some(anchor), _)) = anchor_of(db, bot, run, now).await else { return };
    if db::parse_ts(&anchor).is_some_and(|a| a > since) {
        clear_skip_with(db, events, &bot.id).await;
    }
}

async fn clear_skip_with<E: EventSink>(db: &DbContext<SqlitePool>, events: &E, bot_id: &str) {
    match set_skip(db.pool(), bot_id, false).await {
        Ok(true) => {
            if let Err(e) = events.bot_status_changed(bot_id).await {
                tracing::warn!(bot_id, error = ?e, "keep-warm skip: could not publish status after activity");
            }
        }
        Ok(false) => {}
        Err(e) => tracing::warn!(bot_id, error = ?e, "keep-warm skip: could not clear after activity"),
    }
}

/// 送 prompt 的入口呼叫：不是保溫本身的 prompt＝使用者或 bot 的新回合，就是真的活動，立刻恢復保溫（不等巡邏）。
/// 保溫回覆旗標（`keep_warm_replied_at`）不用清：它從回合紀錄推算，新回合一出現就是 `null`，下一次 `bot_status` 帶出去。
pub async fn note_prompt(app: &(impl crate::capabilities::Db + crate::capabilities::Emit + crate::capabilities::BotStatusEmit), bot_id: &str, client_request_id: &str) {
    if !cache_clock::is_keep_warm_crid(client_request_id) {
        let db = DbContext::new(app.db().clone());
        match set_skip(db.pool(), bot_id, false).await {
            Ok(true) => {
                app.emit_bot_status(bot_id).await;
            }
            Ok(false) => {}
            Err(e) => tracing::warn!(bot_id, error = ?e, "keep-warm skip: could not clear after activity"),
        }
    }
}

/// `POST /api/bots/{id}/keep-warm/skip`（使用者專用）body `{"skip": bool}` → `{"keep_warm_skip": bool}`。
/// 只有主力的 claude／codex 有保溫；其他回 400 `not_primary`。
pub async fn skip_route(app: &(impl crate::capabilities::Db + crate::capabilities::Emit + crate::capabilities::BotStatusEmit), bot_id: &str, skip: bool) -> Result<bool, crate::lc_error::LcError> {
    use crate::lc_error::LcError;
    let db = DbContext::new(app.db().clone());
    let bot = db::bot(db.pool(), bot_id)
        .await
        .map_err(|e| LcError::Upstream(e.to_string()))?
        .filter(|b| b.deleted_at.is_none())
        .ok_or_else(|| LcError::NotFound("bot".into()))?;
    if bot.is_primary == 0 || cache_clock::ttl_secs(&bot.kind).is_none() {
        return Err(LcError::BadValue(serde_json::json!({
            "error": "not_primary",
            "message": "keep-warm only applies to a primary claude or codex bot",
        })));
    }
    if set_skip(db.pool(), bot_id, skip).await.map_err(|e| LcError::Upstream(e.to_string()))? {
        app.emit_bot_status(bot_id).await;
    }
    Ok(skip)
}

/// 熱壓的聊天室說明（`WARM_COMPACT_NOTE_PREFIX` 開頭）。
fn warm_compact_note(age_min: i64) -> String {
    format!("{WARM_COMPACT_NOTE_PREFIX}cache 年齡已 {age_min} 分鐘，自動送出 /compact（熱壓）")
}

/// 送 `/compact` 之前先 claim 這個錨點。回 `true`＝這一輪可以送；`false`＝已經 claim／做過了，什麼都不要送。
pub async fn claim_warm_compact(pool: &SqlitePool, bot_id: &str, anchor: &str, run_id: &str, age_min: i64) -> anyhow::Result<bool> {
    let n = sqlx::query(
        "INSERT INTO primary_cache_actions (bot_id, anchor, kind, run_id, status, age_min, claimed_at)
         VALUES (?, ?, 'warm_compact', ?, 'claimed', ?, ?) ON CONFLICT(bot_id, anchor, kind) DO NOTHING",
    )
    .bind(bot_id)
    .bind(anchor)
    .bind(run_id)
    .bind(age_min)
    .bind(db::now())
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n == 1)
}

/// `/compact` 已送出：收據定案。
pub async fn settle_warm_compact(pool: &SqlitePool, bot_id: &str, anchor: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE primary_cache_actions SET status = 'done', done_at = ? WHERE bot_id = ? AND anchor = ? AND kind = 'warm_compact'")
        .bind(db::now())
        .bind(bot_id)
        .bind(anchor)
        .execute(pool)
        .await?;
    Ok(())
}

/// 確定**沒打字**（鎖內前置檢查就擋下）時撤回 claim，下一輪可依新狀態重判。已定案（`done`）的不動。
pub async fn release_warm_compact(pool: &SqlitePool, bot_id: &str, anchor: &str) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM primary_cache_actions WHERE bot_id = ? AND anchor = ? AND kind = 'warm_compact' AND status = 'claimed'")
        .bind(bot_id)
        .bind(anchor)
        .execute(pool)
        .await?;
    Ok(())
}

/// 這顆 bot 最近一次熱壓（claim 或定案）的時間；沒有收據是 `None`。
pub async fn last_warm_compact_at(pool: &SqlitePool, bot_id: &str) -> anyhow::Result<Option<chrono::DateTime<chrono::Utc>>> {
    let at: Option<String> = sqlx::query_scalar(
        "SELECT MAX(COALESCE(done_at, claimed_at)) FROM primary_cache_actions WHERE bot_id = ? AND kind = 'warm_compact'",
    )
    .bind(bot_id)
    .fetch_one(pool)
    .await?;
    Ok(at.as_deref().and_then(db::parse_ts))
}

async fn mark_note_written(pool: &SqlitePool, bot_id: &str, anchor: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE primary_cache_actions SET note_written = 1 WHERE bot_id = ? AND anchor = ? AND kind = 'warm_compact'")
        .bind(bot_id)
        .bind(anchor)
        .execute(pool)
        .await?;
    Ok(())
}

/// 壓縮已送出、聊天室說明沒寫成的收據（`done` 且 `note_written = 0`）補寫一次說明；成功才設 1。不再 compact。
async fn retry_pending_notes<M: SystemMessageWriter>(db: &DbContext<SqlitePool>, messages: &M, bot: &db::Bot) {
    let rows: Vec<(String, i64)> = match sqlx::query_as(
        "SELECT anchor, age_min FROM primary_cache_actions
          WHERE bot_id = ? AND kind = 'warm_compact' AND status = 'done' AND note_written = 0 ORDER BY claimed_at",
    )
    .bind(&bot.id)
    .fetch_all(db.pool())
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(bot = %bot.name, error = %format!("{e:#}"), "primary keep-warm: could not read pending compaction notes");
            return;
        }
    };
    for (anchor, age_min) in rows {
        match messages.append_system_message(bot.id.clone(), warm_compact_note(age_min)).await {
            Ok(_) => {
                if let Err(e) = mark_note_written(db.pool(), &bot.id, &anchor).await {
                    tracing::warn!(bot = %bot.name, error = %format!("{e:#}"), "primary keep-warm: could not mark the compaction note written");
                }
            }
            Err(e) => tracing::warn!(bot = %bot.name, error = ?e, "primary keep-warm: compaction note still not written; will retry"),
        }
    }
}

/// 判斷這顆主力現在要不要動。`None`＝不對象（不是主力、忙著、沒有任何活動紀錄…）。只讀，不送任何東西。
async fn plan_with_db(db: &DbContext<SqlitePool>, bot: &db::Bot, run: &db::Run, now: chrono::DateTime<chrono::Utc>) -> anyhow::Result<Option<Plan>> {
    let in_flight = db::in_flight_turn(db.pool(), &run.id).await?.is_some();
    let queued = db::queued_turn_for_bot(db.pool(), &bot.id).await?.is_some();
    if !eligible(&bot.kind, bot.is_primary != 0, &run.state, &run.agent_status, in_flight, queued) {
        return Ok(None);
    }
    let (anchor, last_turn) = anchor_of(db, bot, run, now).await?;
    let Some(anchor) = anchor else { return Ok(None) };
    let Some(anchor_t) = db::parse_ts(&anchor) else { return Ok(None) };
    // 「不用保溫」：按下之後沒有真的活動（錨點不晚於按下的時間）就跳過這一輪，不保溫也不熱壓。
    if skip_since(db.pool(), &bot.id).await?.is_some_and(|since| anchor_t <= since) {
        return Ok(None);
    }
    let age_secs = (now - anchor_t).num_seconds();
    if age_secs < KEEP_WARM_AFTER_SECS {
        return Ok(None);
    }
    let Some(cache_ttl_secs) = cache_clock::ttl_secs(&bot.kind) else { return Ok(None) };
    let cache_kept_warm_at = last_turn
        .as_ref()
        .and_then(|turn| turn.kept_warm_at.as_deref())
        .and_then(db::parse_ts);
    let cache_warm_at = cache_kept_warm_at.map_or(anchor_t, |kept_at| kept_at.max(anchor_t));
    let cache_age_secs = (now - cache_warm_at).num_seconds();
    let kept = latest(
        db,
        &format!(
            "SELECT MAX(t.created_at) FROM turns t JOIN conversations c ON c.id = t.conversation_id
              WHERE c.bot_id = ? AND {}",
            cache_clock::keep_warm_crid_sql("t.client_request_id")
        ),
        &bot.id,
    )
    .await?
    .is_some_and(|t| t > anchor_t);
    // 舊資料只有 note、新資料以收據為準：兩者都算，取較晚的。
    let last_note = latest(
        db,
        &format!(
            "SELECT MAX(m.created_at) FROM messages m JOIN conversations c ON c.id = m.conversation_id
              WHERE c.bot_id = ? AND m.role = 'system' AND {}",
            cache_clock::warm_compact_note_sql("m.content")
        ),
        &bot.id,
    )
    .await?;
    let last_compact = last_note.max(last_warm_compact_at(db.pool(), &bot.id).await?);
    if is_warm_compact_echo(anchor_t, last_compact) {
        return Ok(None);
    }
    let compacted = last_compact.is_some_and(|t| t > anchor_t);
    Ok(Some(Plan { step: decide(
            age_secs,
            cache_age_secs,
            cache_ttl_secs,
            kept,
            compacted,
            context_used_pct(&bot.kind, &run.id, run.status_json.as_deref()),
            warm_compact_min_context_pct(&bot.kind).unwrap_or(f64::INFINITY),
    ), anchor, age_secs }))
}

#[cfg(any(test, feature = "test-hooks"))]
pub async fn plan(app: &impl crate::capabilities::Db, bot: &db::Bot, run: &db::Run, now: chrono::DateTime<chrono::Utc>) -> anyhow::Result<Option<Plan>> {
    plan_with_db(&DbContext::new(app.db().clone()), bot, run, now).await
}

/// 照計畫做一步。送不出去（忙著、被擋）只記 log；下一輪重試仍須符合 cache 熱度條件。
/// 計畫綁的是 `run`：送出時帶 `expected_run_id`，bot 鎖內 active run 已換掉就不送（#872）。
async fn act<T: TurnControl, M: SystemMessageWriter>(db: &DbContext<SqlitePool>, turns: &T, messages: &M, bot: &db::Bot, run: &db::Run, plan: &Plan) {
    match plan.step {
        Step::Wait => {}
        Step::KeepWarm => {
            // 先開視窗再送：送出後 statusLine 馬上會動，不能把這次當成活動。送不出去就撤回。
            open_window(&run.id);
            match turns.send_prompt(PromptRequest {
                bot_id: bot.id.clone(),
                text: KEEP_WARM_TEXT.to_string(),
                client_request_id: Some(keep_warm_crid(&plan.anchor)),
                expected_run_id: Some(run.id.clone()),
            }).await {
                Ok(turn_id) => tracing::info!(bot = %bot.name, turn = %turn_id, age_min = plan.age_secs / 60,
                                          "primary keep-warm: sent {KEEP_WARM_TEXT:?}"),
                Err(e) => {
                    drop_window(&run.id);
                    tracing::info!(bot = %bot.name, error = ?e, "primary keep-warm not sent this round; will retry");
                }
            }
        }
        Step::WarmCompact => {
            let age_min = plan.age_secs / 60;
            // 先 claim 再送（#864）：收據在 DB，note 寫失敗或 daemon 在兩者之間掛掉都不會對同一錨點重壓。
            match claim_warm_compact(db.pool(), &bot.id, &plan.anchor, &run.id, age_min).await {
                Ok(true) => {}
                Ok(false) => return,
                Err(e) => {
                    tracing::warn!(bot = %bot.name, error = %format!("{e:#}"), "primary cache compaction: could not claim the anchor; not sent");
                    return;
                }
            }
            // 同保溫：壓縮造成的 statusLine 變化與 working 不算活動（另見 `is_warm_compact_echo`）。
            open_window(&run.id);
            match turns.compact_bot(bot.id.clone(), Some(run.id.clone())).await {
                Ok(()) => {
                    tracing::info!(bot = %bot.name, age_min, "primary cache age reached the limit: sent /compact");
                    if let Err(e) = settle_warm_compact(db.pool(), &bot.id, &plan.anchor).await {
                        tracing::warn!(bot = %bot.name, error = %format!("{e:#}"), "could not settle the compaction receipt; the claim still blocks a resend");
                    }
                    match messages.append_system_message(bot.id.clone(), warm_compact_note(age_min)).await {
                        Ok(_) => {
                            if let Err(e) = mark_note_written(db.pool(), &bot.id, &plan.anchor).await {
                                tracing::warn!(bot = %bot.name, error = %format!("{e:#}"), "could not mark the compaction note written");
                            }
                        }
                        Err(e) => tracing::warn!(bot = %bot.name, error = ?e, "could not record the compaction note; it will be written again later (no recompaction)"),
                    }
                }
                // 鎖內前置檢查擋下、一個鍵都沒打（含 #872 的 `superseded_run`）：撤回 claim，下一輪依新狀態重判。
                Err(e @ (TurnError::Busy(_) | TurnError::BotNotFound(_) | TurnError::InvalidRequest(_) | TurnError::QuotaBlocked(_))) => {
                    drop_window(&run.id);
                    if let Err(re) = release_warm_compact(db.pool(), &bot.id, &plan.anchor).await {
                        tracing::warn!(bot = %bot.name, error = %format!("{re:#}"), "could not release the compaction claim");
                    }
                    tracing::info!(bot = %bot.name, error = ?e, "primary cache compaction not sent this round; will retry");
                }
                // 可能已送出 `/compact`（例如 send_slash_line 中途失敗）：保留 claim，不重壓這個錨點。
                Err(e) => {
                    drop_window(&run.id);
                    tracing::warn!(bot = %bot.name, error = ?e, "primary cache compaction: result unknown; not retrying this anchor");
                }
            }
        }
    }
}

/// 巡一輪。序列處理（每顆都要拿 per-bot 鎖）。
pub async fn sweep_with<T: TurnControl, M: SystemMessageWriter, E: EventSink, C: Clock>(
    db: &DbContext<SqlitePool>,
    turns: &T,
    messages: &M,
    events: &E,
    clock: &C,
    shutdown: &tokio_util::sync::CancellationToken,
) {
    let runs = match db::all_active_runs(db.pool()).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "primary keep-warm: could not list active runs");
            return;
        }
    };
    let now = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(clock.now_unix_ms()).unwrap_or_else(chrono::Utc::now);
    for run in runs.into_iter().filter(|r| r.state == "running") {
        if shutdown.is_cancelled() {
            return;
        }
        let bot = match db::bot(db.pool(), &run.bot_id).await {
            Ok(Some(b)) if b.is_primary != 0 && b.deleted_at.is_none() => b,
            _ => continue,
        };
        if windows().lock().unwrap_or_else(|e| e.into_inner()).contains_key(&run.id) {
            let quiet = run.agent_status == "idle" && db::in_flight_turn(db.pool(), &run.id).await.ok().flatten().is_none();
            settle_window(&run.id, quiet);
        }
        settle_skip_with(db, events, &bot, &run, now).await;
        retry_pending_notes(db, messages, &bot).await;
        match plan_with_db(db, &bot, &run, now).await {
            Ok(Some(p)) if p.step != Step::Wait => act(db, turns, messages, &bot, &run, &p).await,
            Ok(_) => {}
            Err(e) => tracing::warn!(bot = %bot.name, error = %format!("{e:#}"), "primary keep-warm: could not read this bot's state"),
        }
    }
}
