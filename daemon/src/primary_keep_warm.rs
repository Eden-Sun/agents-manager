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
//! * 年齡 ≥ [`KEEP_WARM_AFTER_SECS`]、cache 未過 TTL、這個錨點之後還沒保溫過：用一般送 prompt 的路徑（[`crate::lifecycle::prompt`]）送
//!   [`KEEP_WARM_TEXT`]，`client_request_id = keep-warm:<錨點>`（同一錨點冪等）。保溫錯過 TTL 就不補送。
//! * 年齡 ≥ [`WARM_COMPACT_AFTER_SECS`]、cache 最近一次變熱未滿 TTL 並保留 2 分鐘餘裕、這個錨點之後還沒壓縮過：呼叫
//!   [`crate::lifecycle::compact`]（`/compact`）。cache 冷了就不壓縮，等真的活動重新計時。
//! * 對象只有主力（`bots.is_primary`）的 claude／codex，run 在跑、`agent_status == idle`、沒有在飛或排隊的回合。`blocked`（停在問題／
//!   權限提示）絕不送——打進去的字會變成回答那個問題。
//! * **不用保溫**（`keep_warm_skip` 表，使用者按鈕）：這顆主力「這一輪閒置」跳過保溫與熱壓；錨點（真的活動）晚於按下的時間就自動恢復，
//!   持久（daemon 重啟不忘）。
//!
//! 「這個錨點之後是否做過」不放記憶體：保溫看有沒有 `keep-warm:`（舊資料 `keepalive:`）回合比錨點晚，壓縮看對話裡有沒有比錨點晚的系統訊息
//! （[`WARM_COMPACT_NOTE_PREFIX`]，同時是聊天室看得到的說明），daemon 重啟後不會重做。

use crate::cache_clock::{self, KEEP_WARM_CRID_PREFIX, WARM_COMPACT_NOTE_PREFIX};
use crate::db;
use crate::state::App;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 閒置到這麼久（活動年齡，秒）就考慮保溫。TTL 是 60 分鐘，留 2 分鐘餘裕。
pub const KEEP_WARM_AFTER_SECS: i64 = 58 * 60;
/// 年齡到這麼久才考慮壓縮；仍須確認 cache 熱度足夠。
pub const WARM_COMPACT_AFTER_SECS: i64 = 110 * 60;
/// 壓縮至少要在 cache TTL 到期前留這麼多時間。
const WARM_COMPACT_TTL_MARGIN_SECS: i64 = 2 * 60;
/// 送進去的保溫 prompt。
pub const KEEP_WARM_TEXT: &str = "any updates";
/// 巡邏最多這麼久跑一輪。
const TICK_EVERY: Duration = Duration::from_secs(30);
/// 保溫視窗至少開這麼久才允許被關（送出到 hook 回報 working 之間有空窗）。
const WINDOW_MIN: Duration = Duration::from_secs(60);
/// 視窗最多開這麼久（保險：回合沒收尾也不能永遠擋住 statusLine 活動）。
const WINDOW_MAX: Duration = Duration::from_secs(15 * 60);
/// 回合收尾後，statusLine 還會再來幾次重繪；視窗再撐這麼久才關。
const WINDOW_GRACE: Duration = Duration::from_secs(20);
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

fn is_open(w: &Window, now: Instant) -> bool {
    now.checked_duration_since(w.opened).is_some_and(|d| d < WINDOW_MAX)
        && w.closed.is_none_or(|c| now.checked_duration_since(c).is_none_or(|d| d < WINDOW_GRACE))
}

/// 這個 run 現在是否在保溫回合的視窗裡（送出保溫起、回合收尾後再寬限一下）。視窗裡的 statusLine API 指紋變化與 `working` 都不算活動。
pub fn window_open(run_id: &str) -> bool {
    windows().lock().unwrap_or_else(|e| e.into_inner()).get(run_id).is_some_and(|w| is_open(w, Instant::now()))
}

fn open_window(run_id: &str) {
    windows().lock().unwrap_or_else(|e| e.into_inner()).insert(run_id.to_string(), Window { opened: Instant::now(), closed: None });
}

fn drop_window(run_id: &str) {
    windows().lock().unwrap_or_else(|e| e.into_inner()).remove(run_id);
}

/// 巡邏看到保溫回合已經收尾（閒置、沒有回合在飛）：開始倒數寬限。視窗開著不到 [`WINDOW_MIN`] 不關。
fn settle_window(run_id: &str, idle_and_quiet: bool) {
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

/// 純規則：活動年齡、cache 實際熱度年齡與「這個錨點之後做過沒」決定下一步。
/// 保溫只能在 58 分至 TTL 之間送；壓縮要在 cache 到期前保留 [`WARM_COMPACT_TTL_MARGIN_SECS`] 餘裕。
pub fn decide(
    age_secs: i64,
    cache_age_secs: i64,
    ttl_secs: i64,
    kept_since_anchor: bool,
    compacted_since_anchor: bool,
) -> Step {
    if age_secs >= WARM_COMPACT_AFTER_SECS && !compacted_since_anchor {
        if cache_age_secs < ttl_secs - WARM_COMPACT_TTL_MARGIN_SECS {
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

async fn latest(pool: &sqlx::SqlitePool, sql: &str, bot_id: &str) -> anyhow::Result<Option<chrono::DateTime<chrono::Utc>>> {
    let at: Option<String> = sqlx::query_scalar(sql).bind(bot_id).fetch_one(pool).await?;
    Ok(at.as_deref().and_then(db::parse_ts))
}

/// 最後一次**真的**活動（錨點，[`cache_clock::derive`]）與最近回合。`plan` 和 [`settle_skip`] 共用。
async fn anchor_of(
    app: &Arc<App>,
    bot: &db::Bot,
    run: &db::Run,
    now: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<(Option<String>, Option<cache_clock::LastTurn>)> {
    let last_turn = cache_clock::last_turn_for_bot(&app.db, &bot.id).await?;
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
async fn settle_skip(app: &Arc<App>, bot: &db::Bot, run: &db::Run, now: chrono::DateTime<chrono::Utc>) {
    let Ok(Some(since)) = skip_since(&app.db, &bot.id).await else { return };
    if window_open(&run.id) {
        return;
    }
    let Ok((Some(anchor), _)) = anchor_of(app, bot, run, now).await else { return };
    if db::parse_ts(&anchor).is_some_and(|a| a > since) {
        clear_skip(app, &bot.id).await;
    }
}

async fn clear_skip(app: &Arc<App>, bot_id: &str) {
    match set_skip(&app.db, bot_id, false).await {
        Ok(true) => app.emit_bot_status(bot_id).await,
        Ok(false) => {}
        Err(e) => tracing::warn!(bot_id, error = ?e, "keep-warm skip: could not clear after activity"),
    }
}

/// 送 prompt 的入口呼叫：不是保溫本身的 prompt＝使用者或 bot 的新回合，就是真的活動，立刻恢復保溫（不等巡邏）。
/// 保溫回覆旗標（`keep_warm_replied_at`）不用清：它從回合紀錄推算，新回合一出現就是 `null`，下一次 `bot_status` 帶出去。
pub async fn note_prompt(app: &Arc<App>, bot_id: &str, client_request_id: &str) {
    if !cache_clock::is_keep_warm_crid(client_request_id) {
        clear_skip(app, bot_id).await;
    }
}

/// `POST /api/bots/{id}/keep-warm/skip`（使用者專用）body `{"skip": bool}` → `{"keep_warm_skip": bool}`。
/// 只有主力的 claude／codex 有保溫；其他回 400 `not_primary`。
pub async fn skip_route(app: &Arc<App>, bot_id: &str, skip: bool) -> Result<bool, crate::lifecycle::LcError> {
    use crate::lifecycle::LcError;
    let bot = db::bot(&app.db, bot_id)
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
    if set_skip(&app.db, bot_id, skip).await.map_err(|e| LcError::Upstream(e.to_string()))? {
        app.emit_bot_status(bot_id).await;
    }
    Ok(skip)
}

/// 判斷這顆主力現在要不要動。`None`＝不對象（不是主力、忙著、沒有任何活動紀錄…）。只讀，不送任何東西。
pub async fn plan(app: &Arc<App>, bot: &db::Bot, run: &db::Run, now: chrono::DateTime<chrono::Utc>) -> anyhow::Result<Option<Plan>> {
    let in_flight = db::in_flight_turn(&app.db, &run.id).await?.is_some();
    let queued = db::queued_turn_for_bot(&app.db, &bot.id).await?.is_some();
    if !eligible(&bot.kind, bot.is_primary != 0, &run.state, &run.agent_status, in_flight, queued) {
        return Ok(None);
    }
    let (anchor, last_turn) = anchor_of(app, bot, run, now).await?;
    let Some(anchor) = anchor else { return Ok(None) };
    let Some(anchor_t) = db::parse_ts(&anchor) else { return Ok(None) };
    // 「不用保溫」：按下之後沒有真的活動（錨點不晚於按下的時間）就跳過這一輪，不保溫也不熱壓。
    if skip_since(&app.db, &bot.id).await?.is_some_and(|since| anchor_t <= since) {
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
        &app.db,
        &format!(
            "SELECT MAX(t.created_at) FROM turns t JOIN conversations c ON c.id = t.conversation_id
              WHERE c.bot_id = ? AND {}",
            cache_clock::keep_warm_crid_sql("t.client_request_id")
        ),
        &bot.id,
    )
    .await?
    .is_some_and(|t| t > anchor_t);
    let last_compact = latest(
        &app.db,
        &format!(
            "SELECT MAX(m.created_at) FROM messages m JOIN conversations c ON c.id = m.conversation_id
              WHERE c.bot_id = ? AND m.role = 'system' AND {}",
            cache_clock::warm_compact_note_sql("m.content")
        ),
        &bot.id,
    )
    .await?;
    if is_warm_compact_echo(anchor_t, last_compact) {
        return Ok(None);
    }
    let compacted = last_compact.is_some_and(|t| t > anchor_t);
    Ok(Some(Plan { step: decide(age_secs, cache_age_secs, cache_ttl_secs, kept, compacted), anchor, age_secs }))
}

/// 照 [`plan`] 做一步。送不出去（忙著、被擋）只記 log；下一輪重試仍須符合 cache 熱度條件。
async fn act(app: &Arc<App>, bot: &db::Bot, run: &db::Run, plan: &Plan) {
    match plan.step {
        Step::Wait => {}
        Step::KeepWarm => {
            // 先開視窗再送：送出後 statusLine 馬上會動，不能把這次當成活動。送不出去就撤回。
            open_window(&run.id);
            match crate::lifecycle::prompt(app, &bot.id, KEEP_WARM_TEXT, &keep_warm_crid(&plan.anchor)).await {
                Ok(out) => tracing::info!(bot = %bot.name, turn = %out.turn_id, age_min = plan.age_secs / 60,
                                          "primary keep-warm: sent {KEEP_WARM_TEXT:?}"),
                Err(e) => {
                    drop_window(&run.id);
                    tracing::info!(bot = %bot.name, error = ?e, "primary keep-warm not sent this round; will retry");
                }
            }
        }
        Step::WarmCompact => match {
            // 同保溫：壓縮造成的 statusLine 變化與 working 不算活動（另見 `is_warm_compact_echo`）。
            open_window(&run.id);
            crate::lifecycle::compact(app, &bot.id).await
        } {
            Ok(_) => {
                tracing::info!(bot = %bot.name, age_min = plan.age_secs / 60, "primary cache age reached the limit: sent /compact");
                let note = format!("{WARM_COMPACT_NOTE_PREFIX}cache 年齡已 {} 分鐘，自動送出 /compact（熱壓）", plan.age_secs / 60);
                if let Ok(conv) = db::conversation_id(&app.db, &bot.id).await {
                    if let Err(e) = crate::lifecycle::insert_message(app, &conv, None, "system", &note, "system", false, None).await {
                        tracing::warn!(bot = %bot.name, error = ?e, "could not record the compaction note; it may be sent again");
                    }
                }
            }
            Err(e) => {
                drop_window(&run.id);
                tracing::info!(bot = %bot.name, error = ?e, "primary cache compaction not sent this round; will retry");
            }
        },
    }
}

/// 巡一輪。序列處理（每顆都要拿 per-bot 鎖）。
async fn sweep(app: &Arc<App>) {
    let runs = match db::all_active_runs(&app.db).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "primary keep-warm: could not list active runs");
            return;
        }
    };
    let now = chrono::Utc::now();
    for run in runs.into_iter().filter(|r| r.state == "running") {
        if app.shutdown.is_cancelled() {
            return;
        }
        let bot = match db::bot(&app.db, &run.bot_id).await {
            Ok(Some(b)) if b.is_primary != 0 && b.deleted_at.is_none() => b,
            _ => continue,
        };
        if windows().lock().unwrap_or_else(|e| e.into_inner()).contains_key(&run.id) {
            let quiet = run.agent_status == "idle" && db::in_flight_turn(&app.db, &run.id).await.ok().flatten().is_none();
            settle_window(&run.id, quiet);
        }
        settle_skip(app, &bot, &run, now).await;
        match plan(app, &bot, &run, now).await {
            Ok(Some(p)) if p.step != Step::Wait => act(app, &bot, &run, &p).await,
            Ok(_) => {}
            Err(e) => tracing::warn!(bot = %bot.name, error = %format!("{e:#}"), "primary keep-warm: could not read this bot's state"),
        }
    }
}

static SWEEPING: AtomicBool = AtomicBool::new(false);
static LAST_SWEEP: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();

/// 控制迴圈每一拍呼叫一次；真正的巡邏最多每 [`TICK_EVERY`] 一次，丟到背景跑（送 prompt 要等 pane，不卡住迴圈）。
pub fn tick(app: &Arc<App>) {
    if cfg!(test) || app.shutdown.is_cancelled() {
        return;
    }
    let now = Instant::now();
    {
        let mut last = LAST_SWEEP.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|l| now.checked_duration_since(l).is_some_and(|d| d < TICK_EVERY)) {
            return;
        }
        if SWEEPING.swap(true, Ordering::SeqCst) {
            return;
        }
        *last = Some(now);
    }
    let app = app.clone();
    let tasks = app.background_tasks.clone();
    tasks.spawn(async move {
        sweep(&app).await;
        SWEEPING.store(false, Ordering::SeqCst);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing as tt;

    #[test]
    fn thresholds_are_58_and_110_minutes() {
        assert_eq!((KEEP_WARM_AFTER_SECS, WARM_COMPACT_AFTER_SECS), (3480, 6600));
    }

    #[test]
    fn the_warm_compactions_own_activity_is_not_a_new_anchor() {
        let c = db::parse_ts("2026-10-04T12:00:00.000Z").unwrap();
        let at = |m: i64| c + chrono::Duration::minutes(m);
        assert!(is_warm_compact_echo(at(0), Some(c)));
        assert!(is_warm_compact_echo(at(3), Some(c)), "/compact 讓 statusLine 變、報 working");
        assert!(!is_warm_compact_echo(at(30), Some(c)), "之後真的有活動就重新計時");
        assert!(!is_warm_compact_echo(at(-120), Some(c)), "壓縮之前的錨點照常");
        assert!(!is_warm_compact_echo(at(0), None));
    }

    #[test]
    fn cold_cache_is_skipped_and_hot_cache_is_warm_compacted_with_margin() {
        let m = |min: i64| min * 60;
        let ttl = cache_clock::ttl_secs("claude").unwrap();
        assert_eq!(ttl, m(60));

        assert_eq!(decide(m(57), m(57), ttl, false, false), Step::Wait);
        assert_eq!(decide(m(58), m(58), ttl, false, false), Step::KeepWarm);
        assert_eq!(decide(m(59), m(59), ttl, false, false), Step::KeepWarm, "TTL 到期前仍可保溫");
        assert_eq!(decide(m(60), m(60), ttl, false, false), Step::Wait, "超過保溫視窗後 cache 已冷，不補送");
        assert_eq!(decide(m(80), m(80), ttl, false, false), Step::Wait, "冷 cache 不保溫");

        assert_eq!(decide(m(110), m(110), ttl, true, false), Step::Wait, "冷 cache 不壓縮");
        assert_eq!(decide(m(110), m(52), ttl, true, false), Step::WarmCompact, "58 分保溫後，110 分時 cache 仍熱就壓縮");
        assert_eq!(decide(m(110), m(58), ttl, true, false), Step::Wait, "壓縮前留兩分鐘 TTL 餘裕");
        assert_eq!(decide(m(24 * 60), m(24 * 60), ttl, false, false), Step::Wait, "daemon 重啟後發現年齡很大的 cache 不碰");
        assert_eq!(decide(m(300), m(52), ttl, true, true), Step::Wait, "同一個錨點只壓縮一次");
    }

    #[test]
    fn only_an_idle_primary_claude_or_codex_with_nothing_pending_is_touched() {
        assert!(eligible("claude", true, "running", "idle", false, false));
        assert!(eligible("codex", true, "running", "idle", false, false));
        assert!(!eligible("grok", true, "running", "idle", false, false));
        assert!(!eligible("agy", true, "running", "idle", false, false));
        assert!(!eligible("claude", false, "running", "idle", false, false), "不是主力");
        assert!(!eligible("claude", true, "stopping", "idle", false, false));
        for status in ["blocked", "working", "unknown"] {
            assert!(!eligible("claude", true, "running", status, false, false), "{status}：絕不送");
        }
        assert!(!eligible("claude", true, "running", "idle", true, false), "有回合在飛");
        assert!(!eligible("claude", true, "running", "idle", false, true), "有排隊的回合");
    }

    #[test]
    fn the_window_swallows_statusline_activity_until_it_settles() {
        let run = "r-keep-warm-window";
        let a = r#"{"cost":{"total_api_duration_ms":100},"context_window":{"total_output_tokens":5}}"#;
        let b = r#"{"cost":{"total_api_duration_ms":180},"context_window":{"total_output_tokens":9}}"#;
        let c = r#"{"cost":{"total_api_duration_ms":260},"context_window":{"total_output_tokens":14}}"#;
        assert!(!window_open(run));
        open_window(run);
        assert!(window_open(run));
        assert!(!cache_clock::on_statusline(run, Some(a), Some(b), "2026-10-04T11:00:00.000Z"), "保溫造成的指紋變化不算活動");
        assert_eq!(cache_clock::statusline_at(run), None);
        // 太早（不到 WINDOW_MIN）不會被巡邏關掉；寬限過了才關。
        settle_window(run, true);
        assert!(window_open(run));
        windows().lock().unwrap().get_mut(run).unwrap().opened = Instant::now() - WINDOW_MIN;
        settle_window(run, false);
        assert!(window_open(run), "回合還在跑就不收");
        settle_window(run, true);
        assert!(window_open(run), "收尾後還有寬限");
        windows().lock().unwrap().get_mut(run).unwrap().closed = Some(Instant::now() - WINDOW_GRACE);
        settle_window(run, true);
        assert!(!window_open(run));
        assert!(cache_clock::on_statusline(run, Some(b), Some(c), "2026-10-04T12:00:00.000Z"), "視窗關了，真的活動照算");
        // 視窗有最長期限。
        open_window(run);
        windows().lock().unwrap().get_mut(run).unwrap().opened = Instant::now() - WINDOW_MAX;
        assert!(!window_open(run));
        drop_window(run);
    }

    #[test]
    fn a_keep_warm_in_flight_does_not_reset_the_age() {
        let run = "r-keep-warm-annotate";
        let t = cache_clock::LastTurn { status: "completed".into(), completed_at: Some("2026-10-04T10:00:00.000Z".into()), kept_warm_at: None, ..Default::default() };
        let mut json = serde_json::json!({"id": run, "agent_status": "working"});
        open_window(run);
        cache_clock::annotate(&mut json, "claude", Some(&t));
        assert_eq!(json["last_api_at"], "2026-10-04T10:00:00.000Z", "保溫回合在跑：年齡照真實的算");
        // 真的有使用者回合在飛（last_turn 是 in_flight）就是熱的。
        let live = cache_clock::LastTurn { status: "in_flight".into(), completed_at: None, kept_warm_at: None, ..Default::default() };
        let mut json = serde_json::json!({"id": run, "agent_status": "working"});
        cache_clock::annotate(&mut json, "claude", Some(&live));
        assert_ne!(json["last_api_at"], "2026-10-04T10:00:00.000Z");
        drop_window(run);
    }

    async fn primary(env: &tt::Env, name: &str, agent_status: &str) -> (db::Bot, db::Run) {
        let bot = tt::claude_bot(&env.app, &env.project_id, name).await;
        sqlx::query("UPDATE bots SET is_primary = 1 WHERE id = ?").bind(&bot.id).execute(&env.app.db).await.unwrap();
        let run_id = tt::fake_run(&env.app, &bot.id).await;
        sqlx::query("UPDATE runs SET agent_status = ? WHERE id = ?").bind(agent_status).bind(&run_id).execute(&env.app.db).await.unwrap();
        let bot = db::bot(&env.app.db, &bot.id).await.unwrap().unwrap();
        let run = db::active_run(&env.app.db, &bot.id).await.unwrap().unwrap();
        (bot, run)
    }

    async fn turn(env: &tt::Env, bot: &db::Bot, id: &str, crid: Option<&str>, status: &str, created: &str, completed: Option<&str>) {
        let conv = db::conversation_id(&env.app.db, &bot.id).await.unwrap();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, origin, status, delivery, client_request_id, created_at, completed_at)
             VALUES (?,?, 'web', ?, 'ok', ?, ?, ?)",
        )
        .bind(id)
        .bind(&conv)
        .bind(status)
        .bind(crid)
        .bind(created)
        .bind(completed)
        .execute(&env.app.db)
        .await
        .unwrap();
    }

    fn at(min_ago: i64, now: chrono::DateTime<chrono::Utc>) -> String {
        db::iso_at(now - chrono::Duration::minutes(min_ago))
    }

    /// 錨點＝最後一筆真的回合；保溫回合（持久的 `keepalive:` 前綴）不算，保溫過就不再續，壓縮靠系統訊息記號。
    #[tokio::test]
    async fn plan_reads_the_anchor_from_real_turns_only_and_remembers_what_it_did() {
        let env = tt::env().await;
        let now = chrono::Utc::now();
        let (bot, run) = primary(&env, "kw-plan", "idle").await;

        // 沒有任何活動紀錄：不編時間。
        assert_eq!(plan(&env.app, &bot, &run, now).await.unwrap(), None);

        // 59 分鐘前的真回合 → cache 還熱，該保溫。
        turn(&env, &bot, "t-real", None, "completed", &at(64, now), Some(&at(59, now))).await;
        let p = plan(&env.app, &bot, &run, now).await.unwrap().unwrap();
        assert_eq!((p.step, p.anchor.as_str()), (Step::KeepWarm, at(59, now).as_str()));
        assert_eq!(keep_warm_crid(&p.anchor), format!("keep-warm:{}", at(59, now)));

        // 保溫回合剛跑完：年齡仍從真回合算（不被重置），而且不再續。
        let kept_at = db::iso_at(now);
        turn(&env, &bot, "t-keep", Some(&keep_warm_crid(&p.anchor)), "completed", &kept_at, Some(&kept_at)).await;
        let after = plan(&env.app, &bot, &run, now).await.unwrap().unwrap();
        assert_eq!((after.step, after.anchor.as_str()), (Step::Wait, at(59, now).as_str()), "保溫回合不算活動，且同錨點只續一次");
        let last = cache_clock::last_turn_for_bot(&env.app.db, &bot.id).await.unwrap().unwrap();
        assert_eq!(last.completed_at.as_deref(), Some(at(59, now).as_str()));
        assert_eq!(last.kept_warm_at.as_deref(), Some(kept_at.as_str()), "顏色用的「保溫時間」來自保溫回合");

        // 同一錨點到 110 分鐘：壓縮；記號一寫就不再壓縮。
        let later = now + chrono::Duration::minutes(52);
        let p = plan(&env.app, &bot, &run, later).await.unwrap().unwrap();
        assert_eq!((p.step, p.age_secs / 60), (Step::WarmCompact, 111));
        let conv = db::conversation_id(&env.app.db, &bot.id).await.unwrap();
        let note = format!("{WARM_COMPACT_NOTE_PREFIX}cache 年齡已 111 分鐘，自動送出 /compact");
        crate::lifecycle::insert_message(&env.app, &conv, None, "system", &note, "system", false, None).await.unwrap();
        let p = plan(&env.app, &bot, &run, later + chrono::Duration::seconds(5)).await.unwrap().unwrap();
        assert_eq!(p.step, Step::Wait, "壓縮記號比錨點晚，同一錨點不再壓縮");

        // 真的新活動（使用者回合）→ 錨點更新、計數重來。
        let fresh = later + chrono::Duration::seconds(10);
        turn(&env, &bot, "t-user", None, "completed", &db::iso_at(fresh), Some(&db::iso_at(fresh))).await;
        assert_eq!(plan(&env.app, &bot, &run, fresh + chrono::Duration::minutes(1)).await.unwrap(), None);
    }

    #[tokio::test]
    async fn plan_never_touches_a_blocked_busy_or_non_primary_bot() {
        let env = tt::env().await;
        let now = chrono::Utc::now();
        for status in ["blocked", "working", "unknown"] {
            let (bot, run) = primary(&env, &format!("kw-{status}"), status).await;
            turn(&env, &bot, &format!("t-{status}"), None, "completed", &at(75, now), Some(&at(70, now))).await;
            assert_eq!(plan(&env.app, &bot, &run, now).await.unwrap(), None, "{status}");
        }
        let (bot, run) = primary(&env, "kw-queued", "idle").await;
        turn(&env, &bot, "t-q1", None, "completed", &at(75, now), Some(&at(70, now))).await;
        turn(&env, &bot, "t-q2", None, "queued", &at(1, now), None).await;
        assert_eq!(plan(&env.app, &bot, &run, now).await.unwrap(), None, "有排隊的回合");
        let (mut bot, run) = primary(&env, "kw-plain", "idle").await;
        turn(&env, &bot, "t-p1", None, "completed", &at(75, now), Some(&at(70, now))).await;
        bot.is_primary = 0;
        assert_eq!(plan(&env.app, &bot, &run, now).await.unwrap(), None, "不是主力");
    }

    /// 把保溫那一輪（持久的 crid）塞進去：舊資料用 `keepalive:` 前綴。
    async fn plain_message(env: &tt::Env, bot: &db::Bot, turn_id: &str, role: &str, at: &str) -> String {
        let conv = db::conversation_id(&env.app.db, &bot.id).await.unwrap();
        let id = db::ulid();
        sqlx::query("INSERT INTO messages (id, conversation_id, turn_id, role, content, source, created_at) VALUES (?,?,?,?,'x','hook',?)")
            .bind(&id)
            .bind(&conv)
            .bind(turn_id)
            .bind(role)
            .bind(at)
            .execute(&env.app.db)
            .await
            .unwrap();
        id
    }

    /// 「不用保溫」：按下之後這一輪跳過；真的活動（新回合）後自動恢復；新的錨點照樣 58 分才保溫。
    #[tokio::test]
    async fn skip_passes_this_round_and_real_activity_restores_keep_warm() {
        let env = tt::env().await;
        let now = chrono::Utc::now();
        let (bot, run) = primary(&env, "kw-skip", "idle").await;
        turn(&env, &bot, "t-real", None, "completed", &at(64, now), Some(&at(59, now))).await;
        assert_eq!(plan(&env.app, &bot, &run, now).await.unwrap().unwrap().step, Step::KeepWarm);

        assert!(set_skip(&env.app.db, &bot.id, true).await.unwrap(), "第一次按：狀態變了");
        assert!(!set_skip(&env.app.db, &bot.id, true).await.unwrap(), "再按同一個值是 no-op");
        assert_eq!(plan(&env.app, &bot, &run, now).await.unwrap(), None, "跳過這一輪：不保溫");
        let later = now + chrono::Duration::minutes(55);
        assert_eq!(plan(&env.app, &bot, &run, later).await.unwrap(), None, "也不熱壓");
        // 沒有活動：巡邏不會把它清掉。
        settle_skip(&env.app, &bot, &run, later).await;
        assert!(skip_since(&env.app.db, &bot.id).await.unwrap().is_some());
        // 保溫自己的回合（視窗開著）不算活動。
        open_window(&run.id);
        let fresh = now + chrono::Duration::minutes(1);
        turn(&env, &bot, "t-kw-self", Some(&keep_warm_crid("x")), "completed", &db::iso_at(fresh), Some(&db::iso_at(fresh))).await;
        settle_skip(&env.app, &bot, &run, later).await;
        assert!(skip_since(&env.app.db, &bot.id).await.unwrap().is_some());
        drop_window(&run.id);

        // 真的活動：使用者回合在按下之後完成 → 巡邏恢復。
        turn(&env, &bot, "t-user", None, "completed", &db::iso_at(fresh), Some(&db::iso_at(fresh))).await;
        settle_skip(&env.app, &bot, &run, later).await;
        assert!(skip_since(&env.app.db, &bot.id).await.unwrap().is_none(), "活動後自動恢復");
        let p = plan(&env.app, &bot, &run, fresh + chrono::Duration::minutes(59)).await.unwrap().unwrap();
        assert_eq!(p.step, Step::KeepWarm, "新的錨點照樣到 58 分才保溫");
    }

    /// 即使巡邏還沒來，新的（非保溫）prompt 一進來就恢復；保溫自己的 prompt 不算。
    #[tokio::test]
    async fn a_new_prompt_clears_skip_but_the_keep_warm_prompt_does_not() {
        let env = tt::env().await;
        let (bot, _run) = primary(&env, "kw-note", "idle").await;
        set_skip(&env.app.db, &bot.id, true).await.unwrap();
        note_prompt(&env.app, &bot.id, &keep_warm_crid("2026-10-04T10:00:00.000Z")).await;
        note_prompt(&env.app, &bot.id, "keepalive:2026-10-04T10:00:00.000Z").await;
        assert!(skip_since(&env.app.db, &bot.id).await.unwrap().is_some());
        note_prompt(&env.app, &bot.id, "web-123").await;
        assert!(skip_since(&env.app.db, &bot.id).await.unwrap().is_none());
    }

    /// 跳過的狀態在 DB：daemon 重啟（重開 DB、記憶體視窗歸零）後仍然跳過。
    #[tokio::test]
    async fn skip_survives_a_daemon_restart() {
        let env = tt::env().await;
        let now = chrono::Utc::now();
        let (bot, run) = primary(&env, "kw-restart", "idle").await;
        turn(&env, &bot, "t-real", None, "completed", &at(64, now), Some(&at(59, now))).await;
        set_skip(&env.app.db, &bot.id, true).await.unwrap();
        drop_window(&run.id);
        let reopened = db::open(&env.dir.join("data").join("db.sqlite3")).await.unwrap();
        assert!(skip_since(&reopened, &bot.id).await.unwrap().is_some(), "重開 DB 後還在");
        assert_eq!(plan(&env.app, &bot, &run, now).await.unwrap(), None);
        let last = cache_clock::last_turn_for_bot(&reopened, &bot.id).await.unwrap().unwrap();
        assert!(last.keep_warm_skip, "run JSON 的 keep_warm_skip 也從 DB 來");
    }

    #[tokio::test]
    async fn skip_route_only_accepts_a_primary_claude_or_codex() {
        let env = tt::env().await;
        let (bot, _run) = primary(&env, "kw-route", "idle").await;
        assert!(skip_route(&env.app, &bot.id, true).await.unwrap());
        assert!(skip_since(&env.app.db, &bot.id).await.unwrap().is_some());
        assert!(!skip_route(&env.app, &bot.id, false).await.unwrap());
        assert!(skip_since(&env.app.db, &bot.id).await.unwrap().is_none(), "再按一次取消");

        let plain = tt::claude_bot(&env.app, &env.project_id, "kw-plain").await;
        match skip_route(&env.app, &plain.id, true).await {
            Err(crate::lifecycle::LcError::BadValue(v)) => assert_eq!(v["error"], "not_primary"),
            other => panic!("not_primary expected: {other:?}"),
        }
        assert!(skip_since(&env.app.db, &plain.id).await.unwrap().is_none());
        assert!(matches!(skip_route(&env.app, "no-such-bot", true).await, Err(crate::lifecycle::LcError::NotFound(_))));
    }

    /// 保溫回覆：保溫回合完成後才有；使用者送新 prompt（非保溫回合）就清成 `null`。
    #[tokio::test]
    async fn keep_warm_reply_is_set_after_the_reply_and_cleared_by_the_next_user_prompt() {
        let env = tt::env().await;
        let now = chrono::Utc::now();
        let (bot, _run) = primary(&env, "kw-replied", "idle").await;
        turn(&env, &bot, "t-real", None, "completed", &at(64, now), Some(&at(59, now))).await;
        let last = cache_clock::last_turn_for_bot(&env.app.db, &bot.id).await.unwrap().unwrap();
        assert_eq!(last.keep_warm_replied_at, None, "還沒保溫");
        turn(&env, &bot, "t-kw", Some(&keep_warm_crid("a")), "in_flight", &at(1, now), None).await;
        let last = cache_clock::last_turn_for_bot(&env.app.db, &bot.id).await.unwrap().unwrap();
        assert_eq!(last.keep_warm_replied_at, None, "保溫還沒回覆完");
        sqlx::query("UPDATE turns SET status='completed', completed_at=? WHERE id='t-kw'").bind(at(0, now)).execute(&env.app.db).await.unwrap();
        let last = cache_clock::last_turn_for_bot(&env.app.db.clone(), &bot.id).await.unwrap().unwrap();
        assert_eq!(last.keep_warm_replied_at.as_deref(), Some(at(0, now).as_str()));
        // 帶進 run JSON。
        let mut json = serde_json::json!({"id": "r-kw-replied", "agent_status": "idle"});
        cache_clock::annotate(&mut json, "claude", Some(&last));
        assert_eq!(json["keep_warm_replied_at"], at(0, now));
        // 使用者送出新的 prompt → 清成 null。
        let fresh = db::iso_at(now + chrono::Duration::seconds(30));
        turn(&env, &bot, "t-user2", None, "in_flight", &fresh, None).await;
        let last = cache_clock::last_turn_for_bot(&env.app.db, &bot.id).await.unwrap().unwrap();
        assert_eq!(last.keep_warm_replied_at, None);
        let mut json = serde_json::json!({"id": "r-kw-replied", "agent_status": "working"});
        cache_clock::annotate(&mut json, "claude", Some(&last));
        assert!(json["keep_warm_replied_at"].is_null());
    }

    /// 舊資料的 `keepalive:` 前綴一樣被認得（cache_clock、keep_warm 標記、未讀、保溫過沒）。
    #[tokio::test]
    async fn the_legacy_keepalive_prefix_is_still_recognised() {
        let env = tt::env().await;
        let now = chrono::Utc::now();
        let (bot, run) = primary(&env, "kw-legacy", "idle").await;
        turn(&env, &bot, "t-real", None, "completed", &at(64, now), Some(&at(59, now))).await;
        // 舊前綴的保溫已經送過 → 同錨點不再保溫，kept_warm_at 與保溫回覆也照算。
        turn(&env, &bot, "t-old", Some("keepalive:old-anchor"), "completed", &at(1, now), Some(&at(0, now))).await;
        let after = plan(&env.app, &bot, &run, now).await.unwrap().unwrap();
        assert_eq!(after.step, Step::Wait, "舊前綴的保溫回合也算「這個錨點之後保溫過了」");
        let last = cache_clock::last_turn_for_bot(&env.app.db, &bot.id).await.unwrap().unwrap();
        assert_eq!((last.completed_at.as_deref(), last.kept_warm_at.as_deref()), (Some(at(59, now).as_str()), Some(at(0, now).as_str())));
        assert_eq!(last.keep_warm_replied_at.as_deref(), Some(at(0, now).as_str()));

        // 訊息標記與未讀：新舊前綴的訊息都標 keep_warm、都不算未讀；一般回合的 assistant 才算。
        turn(&env, &bot, "t-new", Some("keep-warm:new-anchor"), "completed", &at(2, now), Some(&at(2, now))).await;
        turn(&env, &bot, "t-chat", None, "completed", &at(3, now), Some(&at(3, now))).await;
        let mut ids = vec![];
        for (t, role) in [("t-old", "user"), ("t-old", "assistant"), ("t-new", "user"), ("t-new", "assistant"), ("t-chat", "user"), ("t-chat", "assistant")] {
            ids.push((t, role, plain_message(&env, &bot, t, role, &at(0, now)).await));
        }
        for (t, role, id) in ids {
            let flag: i64 = sqlx::query_scalar("SELECT keep_warm FROM messages WHERE id=?").bind(&id).fetch_one(&env.app.db).await.unwrap();
            assert_eq!(flag, i64::from(t != "t-chat"), "{t} {role}");
        }
        let unread = crate::read_marks::unread_counts(&env.app.db).await.unwrap();
        assert_eq!(unread.get(&bot.id), Some(&1), "只有一般回合的回覆算未讀");
        // 新增的 migrate 會把舊列補上標記。
        sqlx::query("UPDATE messages SET keep_warm = 0").execute(&env.app.db).await.unwrap();
        let reopened = db::open(&env.dir.join("data").join("db.sqlite3")).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE keep_warm = 1").fetch_one(&reopened).await.unwrap();
        assert_eq!(n, 4, "舊資料回填");
    }
}
