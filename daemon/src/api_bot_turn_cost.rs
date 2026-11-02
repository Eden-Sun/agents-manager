//! `GET /api/bots/{id}/api-usage` 的 `turns` 欄位：最近 50 個 turn 各自的花費（同一 turn_id 的 `api_bot_attempts` 加總）。
use serde_json::{json, Value};
use sqlx::SqlitePool;

type Row = (String, i64, i64, i64, i64, i64, i64, i64, i64);

/// 金額換成美元（微美元 ÷ 1e6）；`recorded`＝cost_usd，`reserved` 狀態的預留額＝reserved_usd。
pub(crate) async fn recent_turns(db: &SqlitePool, bot_id: &str) -> Result<Vec<Value>, sqlx::Error> {
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT turn_id, COUNT(*),
                COALESCE(SUM(CASE WHEN state='recorded' THEN COALESCE(estimated_microusd,0) END),0),
                COALESCE(SUM(CASE WHEN state='reserved' THEN reserved_microusd END),0),
                COALESCE(SUM(state='reserved'),0),
                COALESCE(SUM(CASE WHEN state='recorded' THEN COALESCE(json_extract(usage_json,'$.input_tokens'),0) END),0),
                COALESCE(SUM(CASE WHEN state='recorded' THEN COALESCE(json_extract(usage_json,'$.output_tokens'),0) END),0),
                COALESCE(SUM(CASE WHEN state='recorded' THEN COALESCE(json_extract(usage_json,'$.cache_read_input_tokens'),0) END),0),
                COALESCE(SUM(CASE WHEN state='recorded' THEN COALESCE(json_extract(usage_json,'$.cache_creation_input_tokens'),0) END),0)
         FROM api_bot_attempts WHERE bot_id=? AND turn_id IS NOT NULL AND turn_id<>''
         GROUP BY turn_id ORDER BY MAX(created_at) DESC, MAX(ordinal) DESC LIMIT 50",
    )
    .bind(bot_id).fetch_all(db).await?;
    Ok(rows.into_iter().map(|r| json!({
        "turn_id": r.0, "requests": r.1,
        "cost_usd": r.2 as f64 / 1e6, "reserved_usd": r.3 as f64 / 1e6, "in_flight": r.4 > 0,
        "input_tokens": r.5, "output_tokens": r.6, "cache_read_tokens": r.7, "cache_write_tokens": r.8,
    })).collect())
}
