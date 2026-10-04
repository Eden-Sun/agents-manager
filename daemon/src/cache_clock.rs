//! 每顆 bot「最後一次 API 活動」的時間，給網頁在主力晶片底色畫 prompt cache 倒數（SPEC §6.5j）。
//!
//! 不存新欄位：用現有資料推算、取最晚的那個——
//! * 最近一筆非 queued 回合的 `completed_at`（`turns`，持久）；
//! * claude statusLine 的「API 指紋」變了的那一刻（記憶體帳，run id → 時間）：statusLine 每則 assistant 訊息後
//!   都會重送，但閒置時的重繪也會送；只有 `cost.total_api_duration_ms`／`context_window` 用量變了才算真的打過 API；
//! * `blocked` 的 `agent_status_since`：停在提示上那一刻正是模型剛回完。
//!
//! 回合進行中（`agent_status == "working"` 或有 in-flight 回合）＝熱，`last_api_at` 給現在。
//! 主力的續命回合（[`crate::primary_keepalive`]，`client_request_id` 以 `keepalive:` 開頭）**不算活動**：回合時間、statusLine 指紋、
//! 「進行中＝現在」都排除，`last_api_at` 照真實年齡往上數；續命讓 cache 實際變熱的時間另放 `cache_kept_alive_at`，網頁用它算顏色。
//! TTL 依 kind：claude／codex 3600 秒（量過：閒置 < 60 分 0–1% 冷、> 1 小時 81–91% 冷），grok 未知不帶。
//! 兩個欄位都掛在 run JSON（`/api/state` 的 `bot.run` 與 `bot_status` 事件的 `run`）；daemon 重啟後記憶體帳歸零，
//! 退回回合時間。

use crate::db;
use serde_json::Value;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

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
    /// 最近一次主力保養（成功的續命回合，或到點壓縮）讓 cache 變熱的時間；沒做過是 `None`。
    pub kept_alive_at: Option<String>,
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
    // 續命回合造成的指紋變化不算活動，否則續命一次年齡就歸零（`primary_keepalive`）。
    if crate::primary_keepalive::window_open(run_id) {
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
    crate::primary_keepalive::retain_runs(active);
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
    // 續命回合在跑（沒有真的回合在飛）：它不算活動，年齡照樣往上數，不因 `working` 歸零。
    let run_id = o.get("id").and_then(Value::as_str).map(str::to_owned);
    if status.as_deref() == Some("working")
        && last_turn.is_none_or(|t| t.status != "in_flight")
        && run_id.as_deref().is_some_and(crate::primary_keepalive::window_open)
    {
        status = Some("idle".into());
    }
    let since = o.get("agent_status_since").and_then(Value::as_str).map(str::to_owned);
    let line = o.get("id").and_then(Value::as_str).and_then(statusline_at);
    let at = derive(kind, status.as_deref(), since.as_deref(), last_turn, line.as_deref(), &db::now());
    o.insert("last_api_at".into(), at.into());
    o.insert("cache_ttl_secs".into(), ttl_secs(kind).into());
    // 只有續命過的主力才帶（網頁拿它算顏色；數字仍是 `last_api_at` 的真實年齡）。
    o.insert("cache_kept_alive_at".into(), last_turn.and_then(|t| t.kept_alive_at.clone()).into());
    crate::prompt_cache::annotate(run_json, kind, chrono::Utc::now().timestamp_millis());
}

/// 續命回合的 `client_request_id` 前綴（後面接錨點時間，見 `primary_keepalive`）。不加欄位：這個前綴本身就持久、跨重啟認得出來。
pub const KEEPALIVE_CRID_PREFIX: &str = "keepalive:";

/// 最近一筆**不是續命**的回合，加上最近一次讓 cache 變熱的主力保養（成功的續命回合，完成時間，在飛中用建立時間；或到點壓縮的系統訊息）。
const LAST_TURN_SQL: &str = "SELECT c.bot_id, t.status, t.completed_at,
       NULLIF(MAX(
         COALESCE((SELECT MAX(COALESCE(k.completed_at, k.created_at)) FROM turns k
                    WHERE k.conversation_id = c.id AND k.client_request_id LIKE 'keepalive:%'
                      AND k.status IN ('in_flight','completed','completed_fallback')), ''),
         COALESCE((SELECT MAX(m.created_at) FROM messages m
                    WHERE m.conversation_id = c.id AND m.role = 'system' AND m.content LIKE '主力 cache 到點壓縮：%'), '')
       ), '') AS kept_alive_at
     FROM conversations c
     JOIN turns t ON t.id = (SELECT id FROM turns WHERE conversation_id = c.id AND status != 'queued'
                              AND (client_request_id IS NULL OR client_request_id NOT LIKE 'keepalive:%')
                              ORDER BY created_at DESC, id DESC LIMIT 1)";

/// 每顆 bot 最近一筆送出去的回合，一次讀完（`/api/state` 用，免得逐顆查）。
pub async fn last_turns_by_bot(pool: &SqlitePool) -> anyhow::Result<HashMap<String, LastTurn>> {
    let rows: Vec<(String, String, Option<String>, Option<String>)> = sqlx::query_as(LAST_TURN_SQL).fetch_all(pool).await?;
    Ok(rows.into_iter().map(|(bot, status, completed_at, kept_alive_at)| (bot, LastTurn { status, completed_at, kept_alive_at })).collect())
}

/// 單顆 bot 的 [`last_turns_by_bot`]（`bot_status` 事件用）。
pub async fn last_turn_for_bot(pool: &SqlitePool, bot_id: &str) -> anyhow::Result<Option<LastTurn>> {
    let row: Option<(String, String, Option<String>, Option<String>)> =
        sqlx::query_as(&format!("{LAST_TURN_SQL} WHERE c.bot_id = ?")).bind(bot_id).fetch_optional(pool).await?;
    Ok(row.map(|(_, status, completed_at, kept_alive_at)| LastTurn { status, completed_at, kept_alive_at }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: &str = "2026-10-04T12:00:00.000Z";

    fn done(at: &str) -> LastTurn {
        LastTurn { status: "completed".into(), completed_at: Some(at.into()), kept_alive_at: None }
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
        let in_flight = LastTurn { status: "in_flight".into(), completed_at: None, kept_alive_at: None };
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
             INSERT INTO conversations VALUES ('c1','b1'),('c2','b2'),('c3','b3');
             INSERT INTO turns VALUES
               ('t1','c1','completed','2026-10-04T10:00:00.000Z','2026-10-04T10:05:00.000Z',NULL),
               ('t2','c1','completed','2026-10-04T11:00:00.000Z','2026-10-04T11:04:00.000Z',NULL),
               ('t3','c1','queued','2026-10-04T11:30:00.000Z',NULL,NULL),
               ('t4','c2','in_flight','2026-10-04T11:50:00.000Z',NULL,NULL),
               ('t5','c3','queued','2026-10-04T11:50:00.000Z',NULL,NULL),
               ('k1','c1','completed','2026-10-04T11:20:00.000Z','2026-10-04T11:21:00.000Z','keepalive:2026-10-04T11:04:00.000Z'),
               ('k2','c1','failed','2026-10-04T11:40:00.000Z','2026-10-04T11:40:01.000Z','keepalive:x'),
               ('k3','c3','in_flight','2026-10-04T11:50:00.000Z',NULL,'keepalive:y');
             INSERT INTO messages VALUES ('m1','c2','system','主力 cache 到點壓縮：cache 年齡已 115 分鐘','2026-10-04T11:55:00.000Z'),
                                        ('m2','c1','assistant','主力 cache 到點壓縮：不是系統訊息','2026-10-04T11:59:00.000Z');",
        )
        .execute(&pool)
        .await
        .unwrap();
        let all = last_turns_by_bot(&pool).await.unwrap();
        // 續命回合（keepalive: 前綴）不算「最近一筆回合」，只留下「續命時間」（失敗的不算）。
        let b1 = LastTurn { kept_alive_at: Some("2026-10-04T11:21:00.000Z".into()), ..done("2026-10-04T11:04:00.000Z") };
        assert_eq!(all.get("b1"), Some(&b1));
        // 到點壓縮的系統訊息也算讓 cache 變熱；只有續命回合在飛的 bot（b3）沒有「最近回合」可報。
        assert_eq!(all.get("b2").map(|t| (t.status.as_str(), t.kept_alive_at.as_deref())), Some(("in_flight", Some("2026-10-04T11:55:00.000Z"))));
        assert!(!all.contains_key("b3"));
        assert_eq!(last_turn_for_bot(&pool, "b1").await.unwrap(), Some(b1));
        assert_eq!(last_turn_for_bot(&pool, "b3").await.unwrap(), None);
    }
}
