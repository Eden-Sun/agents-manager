//! openai 的總花費上限與預留（只算 `provider = 'openai'` 的 attempt；不佔 Claude 的額，Claude 也不佔它的）。
//!
//! 餘額檢查與寫入 reserved attempt 在**同一個 `BEGIN IMMEDIATE` 交易**裡（`db::api_bot::reserve_scoped`）：兩筆並發
//! 只有一筆能拿到最後的預算。本片沒有 production caller（切片 D 才接 lifecycle）。

use am_base::db::api_bot::{self as db_api, ProviderScope, TotalCap, PROVIDER_OPENAI};
use am_config::ConfigFile;

/// openai 的 `[openai_api.api_bot]` 總上限（`since`, 微美元）；沒設或沒有起算點就是不限（跟 Claude 的規則一致）。
pub(crate) fn total_cap(cfg: &ConfigFile) -> Option<(String, i64)> {
    let cap_usd = cfg.openai_api.api_bot.total_cap_usd.filter(|c| *c >= 0.0 && c.is_finite())?;
    let since = cfg.openai_api.api_bot.total_cap_since.clone().filter(|s| !s.is_empty())?;
    Some((since, (cap_usd * 1_000_000.0).round() as i64))
}

/// 預留一次 OpenAI 呼叫。`cap` 來自 [`total_cap`]；`service_tier` 寫進 attempt 供結算（第一版只有 `standard`）。
pub(crate) async fn reserve(
    pool: &sqlx::SqlitePool,
    fence: &db_api::Fence<'_>,
    attempt: &db_api::Attempt<'_>,
    checkpoint: &db_api::Checkpoint,
    max_turn_microusd: i64,
    max_per_hour: i64,
    cap: Option<&(String, i64)>,
) -> anyhow::Result<db_api::Reservation> {
    let scope = ProviderScope {
        provider: PROVIDER_OPENAI,
        service_tier: Some("standard"),
        total_cap: cap.map(|(since, cap_microusd)| TotalCap { since: since.as_str(), cap_microusd: *cap_microusd }),
    };
    db_api::reserve_scoped(pool, fence, attempt, checkpoint, max_turn_microusd, max_per_hour, &scope).await
}

#[cfg(test)]
mod tests {
    use super::super::pricing::{input_token_bound, quote_upper_bound};

    /// 低價短回合：預留（上界）與結算都進位，不會是 0 微美元。
    #[test]
    fn a_tiny_reservation_is_at_least_one_microusd() {
        let bound = input_token_bound("gpt-6-luna", 1).unwrap();
        assert!(quote_upper_bound("gpt-6-luna", bound, 1).unwrap() >= 1);
        assert_eq!(quote_upper_bound("gpt-6-luna", 1, 1).unwrap(), 1, "(1×$0.125 + 1×$0.50) 百萬分之一美元進位成 1 微美元");
    }
}
