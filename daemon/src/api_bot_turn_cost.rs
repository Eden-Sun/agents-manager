//! `GET /api/bots/{id}/api-usage` 的 `turns` 欄位：最近 50 個 turn 各自的花費（同一 turn_id 的 `api_bot_attempts` 加總）。
use serde_json::{json, Value};
use sqlx::SqlitePool;

type Row = (String, i64, i64, i64, i64, i64, i64, i64, i64, Option<String>);

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
                COALESCE(SUM(CASE WHEN state='recorded' THEN COALESCE(json_extract(usage_json,'$.cache_creation_input_tokens'),0) END),0),
                MAX(requested_speed)
         FROM api_bot_attempts WHERE bot_id=? AND turn_id IS NOT NULL AND turn_id<>''
         GROUP BY turn_id ORDER BY MAX(created_at) DESC, MAX(ordinal) DESC LIMIT 50",
    )
    .bind(bot_id).fetch_all(db).await?;
    Ok(rows.into_iter().map(|r| json!({
        "turn_id": r.0, "requests": r.1,
        "cost_usd": r.2 as f64 / 1e6, "reserved_usd": r.3 as f64 / 1e6, "in_flight": r.4 > 0,
        "input_tokens": r.5, "output_tokens": r.6, "cache_read_tokens": r.7, "cache_write_tokens": r.8,
        // 這個 turn 送出請求用的速度（`standard`／`fast`）：每回合開始讀一次 bots.fast、整回合固定，改設定要下一回合才生效。
        "speed": r.9,
    })).collect())
}

/// 最近一次請求的 context 大小（token）：最後一個 iteration 的 input＋cache_read＋cache_creation。
/// 伺服器端壓縮時 `iterations` 是 [compaction（壓縮前整段）, message（壓縮後的請求）]，所以壓縮後數字會掉下來；
/// 沒有 `iterations`（沒壓縮）就用頂層欄位。
pub(crate) fn context_tokens(usage: &Value) -> i64 {
    let src = usage.get("iterations").and_then(Value::as_array).and_then(|a| a.last()).filter(|v| v.is_object()).unwrap_or(usage);
    ["input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"].iter().map(|k| src.get(*k).and_then(Value::as_i64).unwrap_or(0)).sum()
}

#[cfg(test)]
mod context_tests {
    use super::*;

    #[test]
    fn context_is_the_last_iteration_after_compaction_and_top_level_otherwise() {
        // 2026-11-15 pt-hub 實際回應：壓縮後的請求只有 25k，不是壓縮前的 130k。
        let real = json!({"cache_creation_input_tokens": 4332, "cache_read_input_tokens": 20653, "input_tokens": 2, "output_tokens": 533,
            "iterations": [{"type": "compaction", "cache_creation_input_tokens": 130581, "input_tokens": 38, "output_tokens": 8192},
                           {"type": "message", "cache_creation_input_tokens": 4332, "cache_read_input_tokens": 20653, "input_tokens": 2, "output_tokens": 533}]});
        assert_eq!(context_tokens(&real), 24_987);
        let plain = json!({"input_tokens": 2, "cache_read_input_tokens": 146_154, "cache_creation_input_tokens": 1856, "output_tokens": 828});
        assert_eq!(context_tokens(&plain), 148_012);
        assert_eq!(context_tokens(&json!({})), 0);
    }
}
