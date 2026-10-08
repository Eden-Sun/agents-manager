//! 每顆 bot「最後一次 API 活動」的時間，給網頁在主力晶片底色畫 prompt cache 倒數（SPEC §6.5j）。
//!
//! 不存新欄位：用現有資料推算、取最晚的那個——
//! * 最近一筆非 queued 回合的 `completed_at`（`turns`，持久）；
//! * claude statusLine 的「API 指紋」變了的那一刻（記憶體帳，run id → 時間）：statusLine 每則 assistant 訊息後
//!   都會重送，但閒置時的重繪也會送；只有 `cost.total_api_duration_ms`／`context_window` 用量變了才算真的打過 API；
//! * `blocked` 的 `agent_status_since`：停在提示上那一刻正是模型剛回完。
//!
//! 回合進行中（`agent_status == "working"` 或有 in-flight 回合）＝熱，`last_api_at` 給現在。
//! 主力的保溫回合（[`crate::primary_keep_warm`]，`client_request_id` 以 `keep-warm:` 開頭，舊資料是 `keepalive:`）**不算活動**：回合時間、statusLine 指紋、
//! 「進行中＝現在」都排除，`last_api_at` 照真實年齡往上數；保溫讓 cache 實際變熱的時間另放 `cache_kept_warm_at`，網頁用它算顏色；熱壓之後視為涼掉，`cache_kept_warm_at` 不含熱壓、也不含熱壓之前的保溫。
//! TTL 依 kind：claude／codex 3600 秒（量過：閒置 < 60 分 0–1% 冷、> 1 小時 81–91% 冷），grok 未知不帶。
//! 兩個欄位都掛在 run JSON（`/api/state` 的 `bot.run` 與 `bot_status` 事件的 `run`）；daemon 重啟後記憶體帳歸零，
//! 退回回合時間。

use crate::db;
use serde_json::Value;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, OnceLock};

/// 這種 kind 的 prompt cache 存活秒數；`None`＝不知道，網頁不畫倒數。
pub fn ttl_secs(kind: &str) -> Option<i64> {
    match kind {
        "claude" | "codex" => Some(3600),
        _ => None,
    }
}

/// 最近一筆真的送出去的回合（不含 queued）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LastTurn {
    pub status: String,
    pub completed_at: Option<String>,
    /// 最近一次成功的保溫回合讓 cache 變熱的時間；沒做過、或之後熱壓過（視為涼掉）是 `None`。
    pub kept_warm_at: Option<String>,
    /// 使用者按了「不用保溫」：這顆主力這一輪閒置跳過保溫與熱壓（`keep_warm_skip` 表有它的列）。
    pub keep_warm_skip: bool,
    /// 最近一次保溫回覆完成的時間；比它晚的非保溫回合（使用者送了新 prompt）一出現就是 `None`。
    pub keep_warm_replied_at: Option<String>,
}

fn store() -> &'static Mutex<HashMap<String, String>> {
    static S: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// statusLine 裡「打過 API 才會變」的那幾個值；一個都沒有回 `None`。
fn api_fingerprint(status_json: &str) -> Option<String> {
    let v: Value = serde_json::from_str(status_json).ok()?;
    let parts = [
        v.pointer("/cost/total_api_duration_ms"),
        v.pointer("/context_window/total_input_tokens"),
        v.pointer("/context_window/total_output_tokens"),
        v.pointer("/context_window/current_usage"),
    ];
    if parts.iter().all(|p| p.is_none_or(Value::is_null)) {
        return None;
    }
    serde_json::to_string(&parts).ok()
}

/// statusLine 換了內容：API 指紋前後都讀得到而且不同，才記下 `at`（閒置重繪、剛起來的第一份都不算）。
pub fn on_statusline(run_id: &str, old: Option<&str>, new: Option<&str>, at: &str) -> bool {
    // 保溫回合造成的指紋變化不算活動，否則保溫一次年齡就歸零（`primary_keep_warm`）。
    if crate::primary_keep_warm::window_open(run_id) {
        return false;
    }
    let (Some(old), Some(new)) = (old.and_then(api_fingerprint), new.and_then(api_fingerprint)) else {
        return false;
    };
    if old == new {
        return false;
    }
    store().lock().unwrap_or_else(|e| e.into_inner()).insert(run_id.to_string(), at.to_string());
    true
}

/// 這個 run 最後一次從 statusLine 看到 API 活動的時間（記憶體帳）。
pub fn statusline_at(run_id: &str) -> Option<String> {
    store().lock().unwrap_or_else(|e| e.into_inner()).get(run_id).cloned()
}

/// 不在 `active` 裡的 run（結束了）不留記錄。帳是全域的，測試版的巡邏不呼叫（同 `prompt_suggestion::retain_runs`）。
#[cfg_attr(test, allow(dead_code))]
pub fn retain_runs(active: &[String]) {
    store().lock().unwrap_or_else(|e| e.into_inner()).retain(|id, _| active.contains(id));
    crate::primary_keep_warm::retain_runs(active);
}

/// 推算 `last_api_at`（純函式）：TTL 不明回 `None`；進行中回 `now`；否則各來源取最晚，一個都沒有回 `None`。
pub fn derive(
    kind: &str,
    agent_status: Option<&str>,
    status_since: Option<&str>,
    last_turn: Option<&LastTurn>,
    statusline: Option<&str>,
    now: &str,
) -> Option<String> {
    ttl_secs(kind)?;
    if agent_status == Some("working") || last_turn.is_some_and(|t| t.status == "in_flight") {
        return Some(now.to_string());
    }
    let blocked_since = status_since.filter(|_| agent_status == Some("blocked"));
    [last_turn.and_then(|t| t.completed_at.as_deref()), statusline, blocked_since]
        .into_iter()
        .flatten()
        .filter_map(db::parse_ts)
        .max()
        .map(db::iso_at)
}

/// 在 run JSON（`background_jobs::run_json` 的結果）上補 `last_api_at` 與 `cache_ttl_secs`；沒有 run（`null`）不動。
pub fn annotate(run_json: &mut Value, kind: &str, last_turn: Option<&LastTurn>) {
    let Some(o) = run_json.as_object_mut() else { return };
    let mut status = o.get("agent_status").and_then(Value::as_str).map(str::to_owned);
    // 保溫回合在跑（沒有真的回合在飛）：它不算活動，年齡照樣往上數，不因 `working` 歸零。
    let run_id = o.get("id").and_then(Value::as_str).map(str::to_owned);
    if status.as_deref() == Some("working")
        && last_turn.is_none_or(|t| t.status != "in_flight")
        && run_id.as_deref().is_some_and(crate::primary_keep_warm::window_open)
    {
        status = Some("idle".into());
    }
    let since = o.get("agent_status_since").and_then(Value::as_str).map(str::to_owned);
    let line = o.get("id").and_then(Value::as_str).and_then(statusline_at);
    let at = derive(kind, status.as_deref(), since.as_deref(), last_turn, line.as_deref(), &db::now());
    o.insert("last_api_at".into(), at.into());
    o.insert("cache_ttl_secs".into(), ttl_secs(kind).into());
    // 只有保溫過的主力才帶（網頁拿它算顏色；數字仍是 `last_api_at` 的真實年齡）。
    o.insert("cache_kept_warm_at".into(), last_turn.and_then(|t| t.kept_warm_at.clone()).into());
    o.insert("keep_warm_skip".into(), last_turn.is_some_and(|t| t.keep_warm_skip).into());
    o.insert("keep_warm_replied_at".into(), last_turn.and_then(|t| t.keep_warm_replied_at.clone()).into());
    crate::prompt_cache::annotate(run_json, kind, chrono::Utc::now().timestamp_millis());
}

#[allow(unused_imports)]
pub use crate::db::predicates::{
    is_keep_warm_crid, keep_warm_crid_sql, warm_compact_note_sql, KEEP_WARM_CRID_PREFIX, LEGACY_KEEP_WARM_CRID_PREFIX,
    LEGACY_WARM_COMPACT_NOTE_PREFIX, WARM_COMPACT_NOTE_PREFIX,
};

/// 最近一筆**不是保溫**的回合，加上最近一次讓 cache 變熱的保溫（成功的保溫回合，完成時間，在飛中用建立時間）、
/// 「不用保溫」旗標，以及最近一次保溫回覆（完成的保溫回合，且之後沒有非保溫回合）。
/// **熱壓不算讓 cache 變熱**（使用者 2026-10-05）：熱壓之後視為涼掉，所以比最近一次熱壓訊息早的保溫不再算——
/// 晶片從熱壓那刻起顯示涼，直到真的活動重新計時（新的保溫時間自然比熱壓晚）。
static LAST_TURN_SQL: LazyLock<String> = LazyLock::new(|| {
    let kw_k = keep_warm_crid_sql("k.client_request_id");
    let kw_u = keep_warm_crid_sql("u.client_request_id");
    let kw_t = keep_warm_crid_sql("client_request_id");
    let note = warm_compact_note_sql("m.content");
    format!(
        "SELECT c.bot_id, t.status, t.completed_at,
       NULLIF(COALESCE((SELECT MAX(COALESCE(k.completed_at, k.created_at)) FROM turns k
                         WHERE k.conversation_id = c.id AND {kw_k}
                           AND k.status IN ('in_flight','completed','completed_fallback')
                           AND COALESCE(k.completed_at, k.created_at) > COALESCE((SELECT MAX(m.created_at) FROM messages m
                                WHERE m.conversation_id = c.id AND m.role = 'system' AND {note}), '')), ''), '') AS kept_warm_at,
       EXISTS(SELECT 1 FROM keep_warm_skip s WHERE s.bot_id = c.bot_id) AS keep_warm_skip,
       (SELECT COALESCE(k.completed_at, k.created_at) FROM turns k
         WHERE k.conversation_id = c.id AND {kw_k} AND k.status IN ('completed','completed_fallback')
           AND NOT EXISTS (SELECT 1 FROM turns u WHERE u.conversation_id = c.id
                             AND (u.client_request_id IS NULL OR NOT {kw_u}) AND u.rowid > k.rowid)
         ORDER BY k.rowid DESC LIMIT 1) AS keep_warm_replied_at
     FROM conversations c
     JOIN turns t ON t.id = (SELECT id FROM turns WHERE conversation_id = c.id AND status != 'queued'
                              AND (client_request_id IS NULL OR NOT {kw_t})
                              ORDER BY created_at DESC, rowid DESC LIMIT 1)"
    )
});

type LastTurnRow = (String, String, Option<String>, Option<String>, i64, Option<String>);

fn last_turn_of(row: LastTurnRow) -> (String, LastTurn) {
    let (bot, status, completed_at, kept_warm_at, skip, replied_at) = row;
    (bot, LastTurn { status, completed_at, kept_warm_at, keep_warm_skip: skip != 0, keep_warm_replied_at: replied_at })
}

/// 每顆 bot 最近一筆送出去的回合，一次讀完（`/api/state` 用，免得逐顆查）。
pub async fn last_turns_by_bot(pool: &SqlitePool) -> anyhow::Result<HashMap<String, LastTurn>> {
    let rows: Vec<LastTurnRow> = sqlx::query_as(&LAST_TURN_SQL).fetch_all(pool).await?;
    Ok(rows.into_iter().map(last_turn_of).collect())
}

/// 單顆 bot 的 [`last_turns_by_bot`]（`bot_status` 事件用）。
pub async fn last_turn_for_bot(pool: &SqlitePool, bot_id: &str) -> anyhow::Result<Option<LastTurn>> {
    let row: Option<LastTurnRow> = sqlx::query_as(&format!("{} WHERE c.bot_id = ?", &*LAST_TURN_SQL)).bind(bot_id).fetch_optional(pool).await?;
    Ok(row.map(|r| last_turn_of(r).1))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: &str = "2026-10-04T12:00:00.000Z";

    fn done(at: &str) -> LastTurn {
        LastTurn { status: "completed".into(), completed_at: Some(at.into()), kept_warm_at: None, ..Default::default() }
    }

    #[test]
    fn ttl_is_an_hour_for_claude_and_codex_and_unknown_for_grok() {
        assert_eq!(ttl_secs("claude"), Some(3600));
        assert_eq!(ttl_secs("codex"), Some(3600));
        assert_eq!(ttl_secs("grok"), None);
        assert_eq!(ttl_secs("agy"), None, "agy 的 TUI 路徑沒有 cache 讀數（設計 #19），網頁不畫倒數");
    }

    #[test]
    fn takes_the_latest_of_turn_statusline_and_blocked() {
        let t = done("2026-10-04T11:00:00.000Z");
        // statusLine 比回合晚：取 statusLine（格式不同也照時刻比，輸出統一成 db::iso_at）。
        assert_eq!(
            derive("claude", Some("idle"), None, Some(&t), Some("2026-10-04T19:10:00+08:00"), NOW).as_deref(),
            Some("2026-10-04T11:10:00.000Z")
        );
        // 回合比 statusLine 晚：取回合。
        assert_eq!(
            derive("claude", Some("idle"), None, Some(&t), Some("2026-10-04T10:00:00.000Z"), NOW).as_deref(),
            Some("2026-10-04T11:00:00.000Z")
        );
        // blocked 的起點最晚：取它；不是 blocked 時同一個 since 不算。
        let since = Some("2026-10-04T11:30:00.000Z");
        assert_eq!(derive("codex", Some("blocked"), since, Some(&t), None, NOW).as_deref(), Some("2026-10-04T11:30:00.000Z"));
        assert_eq!(derive("codex", Some("idle"), since, Some(&t), None, NOW).as_deref(), Some("2026-10-04T11:00:00.000Z"));
        // 什麼來源都沒有：不編時間。
        assert_eq!(derive("claude", Some("idle"), None, None, None, NOW), None);
    }

    #[test]
    fn a_turn_in_progress_is_hot() {
        assert_eq!(derive("claude", Some("working"), None, Some(&done("2026-10-04T08:00:00.000Z")), None, NOW).as_deref(), Some(NOW));
        let in_flight = LastTurn { status: "in_flight".into(), completed_at: None, kept_warm_at: None, ..Default::default() };
        assert_eq!(derive("codex", Some("idle"), None, Some(&in_flight), None, NOW).as_deref(), Some(NOW));
    }

    #[test]
    fn grok_never_carries_a_countdown() {
        assert_eq!(derive("grok", Some("working"), None, Some(&done(NOW)), Some(NOW), NOW), None);
        let mut run = serde_json::json!({"id": "r-grok", "agent_status": "idle"});
        annotate(&mut run, "grok", Some(&done(NOW)));
        assert!(run["last_api_at"].is_null() && run["cache_ttl_secs"].is_null());
        // 沒有 run 不補欄位。
        let mut none = Value::Null;
        annotate(&mut none, "claude", Some(&done(NOW)));
        assert!(none.is_null());
    }

    #[test]
    fn annotate_adds_both_fields_for_claude() {
        let mut run = serde_json::json!({"id": "r-annotate", "agent_status": "idle"});
        annotate(&mut run, "claude", Some(&done("2026-10-04T11:00:00Z")));
        assert_eq!(run["last_api_at"], "2026-10-04T11:00:00.000Z");
        assert_eq!(run["cache_ttl_secs"], 3600);
        assert_eq!((&run["keep_warm_skip"], &run["keep_warm_replied_at"], &run["cache_kept_warm_at"]), (&false.into(), &Value::Null, &Value::Null));
        let t = LastTurn { keep_warm_skip: true, keep_warm_replied_at: Some("2026-10-04T11:30:00.000Z".into()), ..done("2026-10-04T11:00:00Z") };
        annotate(&mut run, "claude", Some(&t));
        assert_eq!((&run["keep_warm_skip"], &run["keep_warm_replied_at"]), (&true.into(), &"2026-10-04T11:30:00.000Z".into()));
    }

    #[test]
    fn both_crid_prefixes_are_keep_warm() {
        assert!(is_keep_warm_crid("keep-warm:2026-10-04T11:00:00.000Z"));
        assert!(is_keep_warm_crid("keepalive:2026-10-04T11:00:00.000Z"), "改名前寫進 DB 的舊資料");
        assert!(!is_keep_warm_crid("web-1234") && !is_keep_warm_crid(""));
    }

    #[test]
    fn statusline_counts_only_when_the_api_fingerprint_moves() {
        let a = r#"{"cost":{"total_api_duration_ms":100},"context_window":{"total_output_tokens":5},"status_line":"x"}"#;
        let redraw = r#"{"cost":{"total_api_duration_ms":100},"context_window":{"total_output_tokens":5},"session_name":"改名"}"#;
        let b = r#"{"cost":{"total_api_duration_ms":180},"context_window":{"total_output_tokens":9}}"#;
        let run = "r-statusline-fp";
        // 第一份（沒有舊的可比）、閒置重繪都不算。
        assert!(!on_statusline(run, None, Some(a), "2026-10-04T11:00:00.000Z"));
        assert!(!on_statusline(run, Some(a), Some(redraw), "2026-10-04T11:01:00.000Z"));
        assert_eq!(statusline_at(run), None);
        assert!(on_statusline(run, Some(redraw), Some(b), "2026-10-04T11:02:00.000Z"));
        assert_eq!(statusline_at(run).as_deref(), Some("2026-10-04T11:02:00.000Z"));
        // 沒有任何指紋欄位（舊版 claude）：不算。
        assert!(!on_statusline("r-statusline-empty", Some("{}"), Some(r#"{"model":{}}"#), NOW));
    }

    #[tokio::test]
    async fn last_turn_skips_queued_and_picks_the_newest() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE conversations (id TEXT PRIMARY KEY, bot_id TEXT NOT NULL);
             CREATE TABLE turns (id TEXT PRIMARY KEY, conversation_id TEXT NOT NULL, status TEXT NOT NULL,
                                 created_at TEXT NOT NULL, completed_at TEXT, client_request_id TEXT);
             CREATE TABLE messages (id TEXT PRIMARY KEY, conversation_id TEXT NOT NULL, role TEXT NOT NULL,
                                    content TEXT NOT NULL, created_at TEXT NOT NULL);
             CREATE TABLE keep_warm_skip (bot_id TEXT PRIMARY KEY, since TEXT NOT NULL);
             INSERT INTO conversations VALUES ('c1','b1'),('c2','b2'),('c3','b3'),('c4','b4'),('c5','b5'),('c6','b6');
             INSERT INTO keep_warm_skip VALUES ('b4','2026-10-04T11:30:00.000Z');
             -- 「之後」以寫入順序（rowid）判：t3 要排在 k1 後面插入。
             INSERT INTO turns VALUES
               ('t1','c1','completed','2026-10-04T10:00:00.000Z','2026-10-04T10:05:00.000Z',NULL),
               ('t2','c1','completed','2026-10-04T11:00:00.000Z','2026-10-04T11:04:00.000Z',NULL),
               ('t4','c2','in_flight','2026-10-04T11:50:00.000Z',NULL,NULL),
               ('t5','c3','queued','2026-10-04T11:50:00.000Z',NULL,NULL),
               ('k1','c1','completed','2026-10-04T11:20:00.000Z','2026-10-04T11:21:00.000Z','keepalive:2026-10-04T11:04:00.000Z'),
               ('k2','c1','failed','2026-10-04T11:40:00.000Z','2026-10-04T11:40:01.000Z','keepalive:x'),
               ('t3','c1','queued','2026-10-04T11:30:00.000Z',NULL,NULL),
               ('k3','c3','in_flight','2026-10-04T11:50:00.000Z',NULL,'keepalive:y'),
               ('t6','c4','completed','2026-10-04T10:00:00.000Z','2026-10-04T10:05:00.000Z',NULL),
               ('k4','c4','completed','2026-10-04T11:20:00.000Z','2026-10-04T11:21:00.000Z','keep-warm:2026-10-04T10:05:00.000Z'),
               ('t7','c5','completed','2026-10-04T10:00:00.000Z','2026-10-04T10:05:00.000Z',NULL),
               ('k5','c5','completed','2026-10-04T11:20:00.000Z','2026-10-04T11:21:00.000Z','keep-warm:z'),
               ('t8','c5','completed','2026-10-04T11:30:00.000Z','2026-10-04T11:31:00.000Z',NULL),
               ('t9','c6','completed','2026-10-04T10:00:00.000Z','2026-10-04T10:05:00.000Z',NULL),
               ('k6a','c6','completed','2026-10-04T10:58:00.000Z','2026-10-04T10:59:00.000Z','keep-warm:a'),
               ('t10','c6','completed','2026-10-04T12:00:00.000Z','2026-10-04T12:01:00.000Z',NULL),
               ('k6b','c6','completed','2026-10-04T12:58:00.000Z','2026-10-04T12:59:00.000Z','keep-warm:b');
             INSERT INTO messages VALUES ('m1','c2','system','主力 cache 到點壓縮：cache 年齡已 115 分鐘','2026-10-04T11:55:00.000Z'),
                                        ('m2','c1','assistant','主力熱壓：不是系統訊息','2026-10-04T11:59:00.000Z'),
                                        ('m3','c5','system','主力熱壓：cache 年齡已 111 分鐘','2026-10-04T11:45:00.000Z'),
                                        ('m4','c6','system','主力熱壓：cache 年齡已 111 分鐘','2026-10-04T11:45:00.000Z');",
        )
        .execute(&pool)
        .await
        .unwrap();
        let all = last_turns_by_bot(&pool).await.unwrap();
        // 保溫回合（keep-warm: 前綴，舊資料 keepalive:）不算「最近一筆回合」，只留下「保溫時間」（失敗的不算）。
        let b1 = LastTurn { kept_warm_at: Some("2026-10-04T11:21:00.000Z".into()), ..done("2026-10-04T11:04:00.000Z") };
        assert_eq!(all.get("b1"), Some(&b1));
        // 熱壓不算讓 cache 變熱（只有熱壓訊息的 b2 沒有保溫時間）；只有保溫回合在飛的 bot（b3）沒有「最近回合」可報。
        assert_eq!(all.get("b2").map(|t| (t.status.as_str(), t.kept_warm_at.as_deref())), Some(("in_flight", None)));
        assert!(!all.contains_key("b3"));
        // 保溫回覆：保溫回合完成後還沒有非保溫回合（b4）才有；之後使用者又送了回合（b1 排隊中的 t3、b5 的 t8）就清成 None。
        assert_eq!(b1.keep_warm_replied_at, None);
        let b4 = all.get("b4").unwrap();
        assert_eq!(b4.keep_warm_replied_at.as_deref(), Some("2026-10-04T11:21:00.000Z"));
        assert_eq!(b4.kept_warm_at.as_deref(), Some("2026-10-04T11:21:00.000Z"), "新前綴 keep-warm: 一樣算保溫時間");
        assert!(b4.keep_warm_skip && !b1.keep_warm_skip, "keep_warm_skip 表有列才算「不用保溫」");
        let b5 = all.get("b5").unwrap();
        assert_eq!((b5.keep_warm_replied_at.as_deref(), b5.completed_at.as_deref()), (None, Some("2026-10-04T11:31:00.000Z")));
        assert_eq!(b5.kept_warm_at, None, "熱壓（11:45）比保溫（11:21）晚：熱壓後視為涼掉，保溫不再算");
        // 熱壓之後真的活動（t10），新一輪的保溫（k6b）又算；熱壓之前的保溫（k6a）不算。
        assert_eq!(all.get("b6").and_then(|t| t.kept_warm_at.as_deref()), Some("2026-10-04T12:59:00.000Z"));
        assert_eq!(last_turn_for_bot(&pool, "b1").await.unwrap(), Some(b1));
        assert_eq!(last_turn_for_bot(&pool, "b3").await.unwrap(), None);
    }

    async fn bare_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE conversations (id TEXT PRIMARY KEY, bot_id TEXT NOT NULL);
             CREATE TABLE turns (id TEXT PRIMARY KEY, conversation_id TEXT NOT NULL, status TEXT NOT NULL,
                                 created_at TEXT NOT NULL, completed_at TEXT, client_request_id TEXT);
             CREATE TABLE messages (id TEXT PRIMARY KEY, conversation_id TEXT NOT NULL, role TEXT NOT NULL,
                                    content TEXT NOT NULL, created_at TEXT NOT NULL);
             CREATE TABLE keep_warm_skip (bot_id TEXT PRIMARY KEY, since TEXT NOT NULL);
             INSERT INTO conversations VALUES ('c','b');",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    /// 同一毫秒建立的兩筆回合：ULID 字典序不保證等於寫入順序，第二鍵要用 `rowid`（同 `native_resume_plan`）。
    #[tokio::test]
    async fn same_millisecond_turns_are_ordered_by_insertion_not_ulid() {
        let pool = bare_pool().await;
        // 'tz' 先寫、'ta' 後寫；'tz' > 'ta'（字典序故意相反）。
        sqlx::query(
            "INSERT INTO turns VALUES
               ('tz','c','completed','2026-10-04T11:00:00.000Z','2026-10-04T11:01:00.000Z',NULL),
               ('ta','c','failed','2026-10-04T11:00:00.000Z','2026-10-04T11:02:00.000Z',NULL);",
        )
        .execute(&pool)
        .await
        .unwrap();
        let last = last_turn_for_bot(&pool, "b").await.unwrap().unwrap();
        assert_eq!((last.status.as_str(), last.completed_at.as_deref()), ("failed", Some("2026-10-04T11:02:00.000Z")));
    }

    #[tokio::test]
    async fn keep_warm_reply_then_same_millisecond_user_turn_clears_the_badge() {
        let pool = bare_pool().await;
        sqlx::query(
            "INSERT INTO turns VALUES
               ('t0','c','completed','2026-10-04T10:00:00.000Z','2026-10-04T10:05:00.000Z',NULL),
               ('k','c','completed','2026-10-04T11:00:00.000Z','2026-10-04T11:01:00.000Z','keep-warm:a'),
               ('u','c','completed','2026-10-04T11:00:00.000Z','2026-10-04T11:02:00.000Z',NULL);",
        )
        .execute(&pool)
        .await
        .unwrap();
        let last = last_turn_for_bot(&pool, "b").await.unwrap().unwrap();
        assert_eq!(last.keep_warm_replied_at, None);
    }

    #[tokio::test]
    async fn user_turn_then_same_millisecond_keep_warm_reply_keeps_the_badge() {
        let pool = bare_pool().await;
        sqlx::query(
            "INSERT INTO turns VALUES
               ('t0','c','completed','2026-10-04T10:00:00.000Z','2026-10-04T10:05:00.000Z',NULL),
               ('u','c','completed','2026-10-04T11:00:00.000Z','2026-10-04T11:02:00.000Z',NULL),
               ('k','c','completed','2026-10-04T11:00:00.000Z','2026-10-04T11:01:00.000Z','keep-warm:a');",
        )
        .execute(&pool)
        .await
        .unwrap();
        let last = last_turn_for_bot(&pool, "b").await.unwrap().unwrap();
        assert_eq!(last.keep_warm_replied_at.as_deref(), Some("2026-10-04T11:01:00.000Z"));
    }
}
