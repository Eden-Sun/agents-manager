//! 主力 bot 的 prompt cache 續命與到點壓縮（SPEC §6.5k，使用者 2026-10-04）。
//!
//! 使用者原話：「主力的規則如下：當 cache 至 58 分鐘時，下一個 prompt "any updates"，來讓 cache 延長，且 cache 時間照樣上數，
//! 達 110 分鐘時，強制做壓縮」。
//!
//! * **活動年齡**＝距離 [`cache_clock`] 推算的最後一次**真的**活動（`last_api_at`）。續命回合本身不算活動
//!   （`turns.client_request_id` 以 [`KEEPALIVE_CRID_PREFIX`] 開頭，`cache_clock` 的最近回合 SQL 排除它；續命期間的 statusLine 指紋變化
//!   由 [`window_open`] 擋掉），所以送完續命年齡照樣往上數，直到下一次真的活動才歸零。
//!   cache 熱度則以 `max(last_api_at, cache_kept_alive_at)` 計算；時間來源沿用 [`cache_clock`]。
//! * 年齡 ≥ [`KEEPALIVE_AFTER_SECS`]、cache 未過 TTL、這個錨點之後還沒續過：用一般送 prompt 的路徑（[`crate::lifecycle::prompt`]）送
//!   [`KEEPALIVE_TEXT`]，`client_request_id = keepalive:<錨點>`（同一錨點冪等）。續命錯過 TTL 就不補送。
//! * 年齡 ≥ [`COMPACT_AFTER_SECS`]、cache 最近一次變熱未滿 TTL 並保留 2 分鐘餘裕、這個錨點之後還沒壓縮過：呼叫
//!   [`crate::lifecycle::compact`]（`/compact`）。cache 冷了就不壓縮，等真的活動重新計時。
//! * 對象只有主力（`bots.is_primary`）的 claude／codex，run 在跑、`agent_status == idle`、沒有在飛或排隊的回合。`blocked`（停在問題／
//!   權限提示）絕不送——打進去的字會變成回答那個問題。
//!
//! 「這個錨點之後是否做過」不放記憶體：續命看有沒有 `keepalive:` 回合比錨點晚，壓縮看對話裡有沒有比錨點晚的系統訊息
//! （[`COMPACT_NOTE_PREFIX`]，同時是聊天室看得到的說明），daemon 重啟後不會重做。

use crate::cache_clock::{self, KEEPALIVE_CRID_PREFIX};
use crate::db;
use crate::state::App;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 閒置到這麼久（活動年齡，秒）就考慮續命。TTL 是 60 分鐘，留 2 分鐘餘裕。
pub const KEEPALIVE_AFTER_SECS: i64 = 58 * 60;
/// 年齡到這麼久才考慮壓縮；仍須確認 cache 熱度足夠。
pub const COMPACT_AFTER_SECS: i64 = 110 * 60;
/// 壓縮至少要在 cache TTL 到期前留這麼多時間。
const COMPACT_TTL_MARGIN_SECS: i64 = 2 * 60;
/// 送進去的續命 prompt。
pub const KEEPALIVE_TEXT: &str = "any updates";
/// 聊天室系統訊息的開頭，同時是「這個錨點之後壓縮過了」的持久記號。
pub const COMPACT_NOTE_PREFIX: &str = "主力 cache 到點壓縮：";
/// 巡邏最多這麼久跑一輪。
const TICK_EVERY: Duration = Duration::from_secs(30);
/// 續命視窗至少開這麼久才允許被關（送出到 hook 回報 working 之間有空窗）。
const WINDOW_MIN: Duration = Duration::from_secs(60);
/// 視窗最多開這麼久（保險：回合沒收尾也不能永遠擋住 statusLine 活動）。
const WINDOW_MAX: Duration = Duration::from_secs(15 * 60);
/// 回合收尾後，statusLine 還會再來幾次重繪；視窗再撐這麼久才關。
const WINDOW_GRACE: Duration = Duration::from_secs(20);
/// 到點壓縮之後這麼久內冒出來的「活動」算壓縮自己造成的（`/compact` 讓 statusLine 指紋變、hook 報 working），
/// 不當成新的錨點；否則閒置的主力每兩小時就續命＋壓縮一輪，永遠停不下來。真的活動晚於這段才重新計時。
pub const COMPACT_ECHO_SECS: i64 = 10 * 60;

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

/// 這個 run 現在是否在續命回合的視窗裡（送出續命起、回合收尾後再寬限一下）。視窗裡的 statusLine API 指紋變化與 `working` 都不算活動。
pub fn window_open(run_id: &str) -> bool {
    windows().lock().unwrap_or_else(|e| e.into_inner()).get(run_id).is_some_and(|w| is_open(w, Instant::now()))
}

fn open_window(run_id: &str) {
    windows().lock().unwrap_or_else(|e| e.into_inner()).insert(run_id.to_string(), Window { opened: Instant::now(), closed: None });
}

fn drop_window(run_id: &str) {
    windows().lock().unwrap_or_else(|e| e.into_inner()).remove(run_id);
}

/// 巡邏看到續命回合已經收尾（閒置、沒有回合在飛）：開始倒數寬限。視窗開著不到 [`WINDOW_MIN`] 不關。
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
    KeepAlive,
    Compact,
}

/// 純規則：錨點是不是到點壓縮自己的回音（壓縮後 [`COMPACT_ECHO_SECS`] 內）。是的話這一輪什麼都不做。
pub fn is_compact_echo(anchor: chrono::DateTime<chrono::Utc>, last_compact: Option<chrono::DateTime<chrono::Utc>>) -> bool {
    last_compact.is_some_and(|c| anchor >= c - chrono::Duration::seconds(5) && anchor <= c + chrono::Duration::seconds(COMPACT_ECHO_SECS))
}

/// 純規則：活動年齡、cache 實際熱度年齡與「這個錨點之後做過沒」決定下一步。
/// 續命只能在 58 分至 TTL 之間送；壓縮要在 cache 到期前保留 [`COMPACT_TTL_MARGIN_SECS`] 餘裕。
pub fn decide(
    age_secs: i64,
    cache_age_secs: i64,
    ttl_secs: i64,
    kept_since_anchor: bool,
    compacted_since_anchor: bool,
) -> Step {
    if age_secs >= COMPACT_AFTER_SECS && !compacted_since_anchor {
        if cache_age_secs < ttl_secs - COMPACT_TTL_MARGIN_SECS {
            Step::Compact
        } else {
            Step::Wait
        }
    } else if age_secs >= KEEPALIVE_AFTER_SECS
        && age_secs < ttl_secs
        && cache_age_secs < ttl_secs
        && !kept_since_anchor
    {
        Step::KeepAlive
    } else {
        Step::Wait
    }
}

/// 純規則：這顆 run 現在能不能被碰。`blocked`／`working`／`unknown` 一律不送。
pub fn eligible(kind: &str, is_primary: bool, run_state: &str, agent_status: &str, in_flight: bool, queued: bool) -> bool {
    is_primary && matches!(kind, "claude" | "codex") && run_state == "running" && agent_status == "idle" && !in_flight && !queued
}

/// 續命回合的 `client_request_id`：同一個錨點只有一個。
pub fn keepalive_crid(anchor: &str) -> String {
    format!("{KEEPALIVE_CRID_PREFIX}{anchor}")
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

/// 判斷這顆主力現在要不要動。`None`＝不對象（不是主力、忙著、沒有任何活動紀錄…）。只讀，不送任何東西。
pub async fn plan(app: &Arc<App>, bot: &db::Bot, run: &db::Run, now: chrono::DateTime<chrono::Utc>) -> anyhow::Result<Option<Plan>> {
    let in_flight = db::in_flight_turn(&app.db, &run.id).await?.is_some();
    let queued = db::queued_turn_for_bot(&app.db, &bot.id).await?.is_some();
    if !eligible(&bot.kind, bot.is_primary != 0, &run.state, &run.agent_status, in_flight, queued) {
        return Ok(None);
    }
    let last_turn = cache_clock::last_turn_for_bot(&app.db, &bot.id).await?;
    let line = cache_clock::statusline_at(&run.id);
    let Some(anchor) = cache_clock::derive(
        &bot.kind,
        Some(&run.agent_status),
        run.agent_status_since.as_deref(),
        last_turn.as_ref(),
        line.as_deref(),
        &db::iso_at(now),
    ) else {
        return Ok(None);
    };
    let Some(anchor_t) = db::parse_ts(&anchor) else { return Ok(None) };
    let age_secs = (now - anchor_t).num_seconds();
    if age_secs < KEEPALIVE_AFTER_SECS {
        return Ok(None);
    }
    let Some(cache_ttl_secs) = cache_clock::ttl_secs(&bot.kind) else { return Ok(None) };
    let cache_kept_alive_at = last_turn
        .as_ref()
        .and_then(|turn| turn.kept_alive_at.as_deref())
        .and_then(db::parse_ts);
    let cache_warm_at = cache_kept_alive_at.map_or(anchor_t, |kept_at| kept_at.max(anchor_t));
    let cache_age_secs = (now - cache_warm_at).num_seconds();
    let kept = latest(
        &app.db,
        "SELECT MAX(t.created_at) FROM turns t JOIN conversations c ON c.id = t.conversation_id
          WHERE c.bot_id = ? AND t.client_request_id LIKE 'keepalive:%'",
        &bot.id,
    )
    .await?
    .is_some_and(|t| t > anchor_t);
    let last_compact = latest(
        &app.db,
        &format!(
            "SELECT MAX(m.created_at) FROM messages m JOIN conversations c ON c.id = m.conversation_id
              WHERE c.bot_id = ? AND m.role = 'system' AND m.content LIKE '{COMPACT_NOTE_PREFIX}%'"
        ),
        &bot.id,
    )
    .await?;
    if is_compact_echo(anchor_t, last_compact) {
        return Ok(None);
    }
    let compacted = last_compact.is_some_and(|t| t > anchor_t);
    Ok(Some(Plan { step: decide(age_secs, cache_age_secs, cache_ttl_secs, kept, compacted), anchor, age_secs }))
}

/// 照 [`plan`] 做一步。送不出去（忙著、被擋）只記 log；下一輪重試仍須符合 cache 熱度條件。
async fn act(app: &Arc<App>, bot: &db::Bot, run: &db::Run, plan: &Plan) {
    match plan.step {
        Step::Wait => {}
        Step::KeepAlive => {
            // 先開視窗再送：送出後 statusLine 馬上會動，不能把這次當成活動。送不出去就撤回。
            open_window(&run.id);
            match crate::lifecycle::prompt(app, &bot.id, KEEPALIVE_TEXT, &keepalive_crid(&plan.anchor)).await {
                Ok(out) => tracing::info!(bot = %bot.name, turn = %out.turn_id, age_min = plan.age_secs / 60,
                                          "primary cache keepalive: sent {KEEPALIVE_TEXT:?}"),
                Err(e) => {
                    drop_window(&run.id);
                    tracing::info!(bot = %bot.name, error = ?e, "primary cache keepalive not sent this round; will retry");
                }
            }
        }
        Step::Compact => match {
            // 同續命：壓縮造成的 statusLine 變化與 working 不算活動（另見 `is_compact_echo`）。
            open_window(&run.id);
            crate::lifecycle::compact(app, &bot.id).await
        } {
            Ok(_) => {
                tracing::info!(bot = %bot.name, age_min = plan.age_secs / 60, "primary cache age reached the limit: sent /compact");
                let note = format!("{COMPACT_NOTE_PREFIX}cache 年齡已 {} 分鐘，自動送出 /compact", plan.age_secs / 60);
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
            tracing::warn!(error = %e, "primary keepalive: could not list active runs");
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
        match plan(app, &bot, &run, now).await {
            Ok(Some(p)) if p.step != Step::Wait => act(app, &bot, &run, &p).await,
            Ok(_) => {}
            Err(e) => tracing::warn!(bot = %bot.name, error = %format!("{e:#}"), "primary keepalive: could not read this bot's state"),
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
        assert_eq!((KEEPALIVE_AFTER_SECS, COMPACT_AFTER_SECS), (3480, 6600));
    }

    #[test]
    fn the_compactions_own_activity_is_not_a_new_anchor() {
        let c = db::parse_ts("2026-10-04T12:00:00.000Z").unwrap();
        let at = |m: i64| c + chrono::Duration::minutes(m);
        assert!(is_compact_echo(at(0), Some(c)));
        assert!(is_compact_echo(at(3), Some(c)), "/compact 讓 statusLine 變、報 working");
        assert!(!is_compact_echo(at(30), Some(c)), "之後真的有活動就重新計時");
        assert!(!is_compact_echo(at(-120), Some(c)), "壓縮之前的錨點照常");
        assert!(!is_compact_echo(at(0), None));
    }

    #[test]
    fn cold_cache_is_skipped_and_hot_cache_is_compacted_with_margin() {
        let m = |min: i64| min * 60;
        let ttl = cache_clock::ttl_secs("claude").unwrap();
        assert_eq!(ttl, m(60));

        assert_eq!(decide(m(57), m(57), ttl, false, false), Step::Wait);
        assert_eq!(decide(m(58), m(58), ttl, false, false), Step::KeepAlive);
        assert_eq!(decide(m(59), m(59), ttl, false, false), Step::KeepAlive, "TTL 到期前仍可續命");
        assert_eq!(decide(m(60), m(60), ttl, false, false), Step::Wait, "超過續命視窗後 cache 已冷，不補送");
        assert_eq!(decide(m(80), m(80), ttl, false, false), Step::Wait, "冷 cache 不續命");

        assert_eq!(decide(m(110), m(110), ttl, true, false), Step::Wait, "冷 cache 不壓縮");
        assert_eq!(decide(m(110), m(52), ttl, true, false), Step::Compact, "58 分續命後，110 分時 cache 仍熱就壓縮");
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
        let run = "r-keepalive-window";
        let a = r#"{"cost":{"total_api_duration_ms":100},"context_window":{"total_output_tokens":5}}"#;
        let b = r#"{"cost":{"total_api_duration_ms":180},"context_window":{"total_output_tokens":9}}"#;
        let c = r#"{"cost":{"total_api_duration_ms":260},"context_window":{"total_output_tokens":14}}"#;
        assert!(!window_open(run));
        open_window(run);
        assert!(window_open(run));
        assert!(!cache_clock::on_statusline(run, Some(a), Some(b), "2026-10-04T11:00:00.000Z"), "續命造成的指紋變化不算活動");
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
    fn a_keepalive_in_flight_does_not_reset_the_age() {
        let run = "r-keepalive-annotate";
        let t = cache_clock::LastTurn { status: "completed".into(), completed_at: Some("2026-10-04T10:00:00.000Z".into()), kept_alive_at: None };
        let mut json = serde_json::json!({"id": run, "agent_status": "working"});
        open_window(run);
        cache_clock::annotate(&mut json, "claude", Some(&t));
        assert_eq!(json["last_api_at"], "2026-10-04T10:00:00.000Z", "續命回合在跑：年齡照真實的算");
        // 真的有使用者回合在飛（last_turn 是 in_flight）就是熱的。
        let live = cache_clock::LastTurn { status: "in_flight".into(), completed_at: None, kept_alive_at: None };
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

    /// 錨點＝最後一筆真的回合；續命回合（持久的 `keepalive:` 前綴）不算，續命過就不再續，壓縮靠系統訊息記號。
    #[tokio::test]
    async fn plan_reads_the_anchor_from_real_turns_only_and_remembers_what_it_did() {
        let env = tt::env().await;
        let now = chrono::Utc::now();
        let (bot, run) = primary(&env, "ka-plan", "idle").await;

        // 沒有任何活動紀錄：不編時間。
        assert_eq!(plan(&env.app, &bot, &run, now).await.unwrap(), None);

        // 59 分鐘前的真回合 → cache 還熱，該續命。
        turn(&env, &bot, "t-real", None, "completed", &at(64, now), Some(&at(59, now))).await;
        let p = plan(&env.app, &bot, &run, now).await.unwrap().unwrap();
        assert_eq!((p.step, p.anchor.as_str()), (Step::KeepAlive, at(59, now).as_str()));
        assert_eq!(keepalive_crid(&p.anchor), format!("keepalive:{}", at(59, now)));

        // 續命回合剛跑完：年齡仍從真回合算（不被重置），而且不再續。
        let kept_at = db::iso_at(now);
        turn(&env, &bot, "t-keep", Some(&keepalive_crid(&p.anchor)), "completed", &kept_at, Some(&kept_at)).await;
        let after = plan(&env.app, &bot, &run, now).await.unwrap().unwrap();
        assert_eq!((after.step, after.anchor.as_str()), (Step::Wait, at(59, now).as_str()), "續命回合不算活動，且同錨點只續一次");
        let last = cache_clock::last_turn_for_bot(&env.app.db, &bot.id).await.unwrap().unwrap();
        assert_eq!(last.completed_at.as_deref(), Some(at(59, now).as_str()));
        assert_eq!(last.kept_alive_at.as_deref(), Some(kept_at.as_str()), "顏色用的「續命時間」來自續命回合");

        // 同一錨點到 110 分鐘：壓縮；記號一寫就不再壓縮。
        let later = now + chrono::Duration::minutes(52);
        let p = plan(&env.app, &bot, &run, later).await.unwrap().unwrap();
        assert_eq!((p.step, p.age_secs / 60), (Step::Compact, 111));
        let conv = db::conversation_id(&env.app.db, &bot.id).await.unwrap();
        let note = format!("{COMPACT_NOTE_PREFIX}cache 年齡已 111 分鐘，自動送出 /compact");
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
            let (bot, run) = primary(&env, &format!("ka-{status}"), status).await;
            turn(&env, &bot, &format!("t-{status}"), None, "completed", &at(75, now), Some(&at(70, now))).await;
            assert_eq!(plan(&env.app, &bot, &run, now).await.unwrap(), None, "{status}");
        }
        let (bot, run) = primary(&env, "ka-queued", "idle").await;
        turn(&env, &bot, "t-q1", None, "completed", &at(75, now), Some(&at(70, now))).await;
        turn(&env, &bot, "t-q2", None, "queued", &at(1, now), None).await;
        assert_eq!(plan(&env.app, &bot, &run, now).await.unwrap(), None, "有排隊的回合");
        let (mut bot, run) = primary(&env, "ka-plain", "idle").await;
        turn(&env, &bot, "t-p1", None, "completed", &at(75, now), Some(&at(70, now))).await;
        bot.is_primary = 0;
        assert_eq!(plan(&env.app, &bot, &run, now).await.unwrap(), None, "不是主力");
    }
}
