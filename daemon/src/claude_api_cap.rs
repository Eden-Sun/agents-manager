//! claude-api 總花費上限（Total Spend Cap，issue #1944）。
//! 所有 claude-api bot 合計的總花費上限（美元），從設定上限的那一刻（total_cap_since）起算。
//! 花費＝api_bot_attempts 中 created_at >= total_cap_since 的 recorded + reserved + unknown。

use std::sync::Arc;
use am_base::lc_error::LcError;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::state::App;

/// 查詢指定時間起算的所有 claude-api bot 合計花費（微美元）
/// 花費＝created_at >= since_rfc3339 且 state IN ('recorded', 'reserved', 'unknown')
/// recorded: estimated_microusd
/// reserved: reserved_microusd
/// unknown: COALESCE(estimated_microusd, reserved_microusd)
pub async fn query_total_cap_spent_microusd(app: &Arc<App>, since_rfc3339: &str) -> Result<i64> {
    let spent: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(
            CASE WHEN state='recorded' THEN COALESCE(estimated_microusd, 0)
                 WHEN state='reserved' THEN reserved_microusd
                 WHEN state='unknown' THEN COALESCE(estimated_microusd, reserved_microusd)
                 ELSE 0 END
         ), 0)
         FROM api_bot_attempts
         WHERE created_at >= ? AND state IN ('recorded', 'reserved', 'unknown')",
    )
    .bind(since_rfc3339)
    .fetch_one(&app.db)
    .await?;

    Ok(spent)
}

/// 檢查是否會超過總花費上限。
/// 若有設定總上限，且 (目前花費 + reserve_microusd) > cap_microusd，回傳 Err(RefusalMsg)；
/// 否則回傳 Ok(())。
pub async fn check_total_cap_allowed(
    app: &Arc<App>,
    reserve_microusd: i64,
) -> Result<(), (f64, f64)> {
    let cfg = app.cfg.get().await;
    let cap_usd = match cfg.claude_api.api_bot.total_cap_usd {
        Some(c) if c >= 0.0 => c,
        _ => return Ok(()),
    };
    let since = match cfg.claude_api.api_bot.total_cap_since.as_deref() {
        Some(s) if !s.is_empty() => s,
        _ => return Ok(()),
    };

    let spent_microusd = query_total_cap_spent_microusd(app, since).await.unwrap_or(0);
    let cap_microusd = (cap_usd * 1_000_000.0).round() as i64;
    let total_with_reservation = spent_microusd.saturating_add(reserve_microusd.max(0));

    if total_with_reservation > cap_microusd {
        let spent_usd = spent_microusd as f64 / 1_000_000.0;
        Err((cap_usd, spent_usd))
    } else {
        Ok(())
    }
}

/// 格式化金額（去除多餘小數位或保留美觀，如 10 或 10.5 或 10.25）
pub fn format_usd_pretty(val: f64) -> String {
    let s = format!("{:.4}", val);
    let s = s.trim_end_matches('0');
    let s = s.trim_end_matches('.');
    if s.is_empty() {
        "0".to_string()
    } else {
        s.to_string()
    }
}

/// GET /api/claude-api/cap
pub async fn get_claude_api_cap(app: Arc<App>) -> Result<Value, LcError> {
    let cfg = app.cfg.get().await;
    let cap_usd = cfg.claude_api.api_bot.total_cap_usd;
    let since = cfg.claude_api.api_bot.total_cap_since.clone();

    let spent_usd = if let Some(ref s) = since {
        let microusd = query_total_cap_spent_microusd(&app, s)
            .await
            .map_err(|e| LcError::Bad(format!("query cap spent error: {e}")))?;
        microusd as f64 / 1_000_000.0
    } else {
        0.0
    };

    Ok(json!({
        "cap_usd": cap_usd,
        "since": since,
        "spent_usd": spent_usd,
    }))
}

/// PUT /api/claude-api/cap 請求 body
#[derive(Debug, Deserialize)]
pub struct PutCapRequest {
    pub cap_usd: Option<f64>,
}

/// PUT /api/claude-api/cap
pub async fn put_claude_api_cap(app: Arc<App>, body: PutCapRequest) -> Result<Value, LcError> {
    let (new_cap, new_since) = match body.cap_usd {
        Some(cap) => {
            if cap < 0.0 || !cap.is_finite() {
                return Err(LcError::BadValue(json!({"error": "bad_request", "reason": "invalid_cap_usd"})));
            }
            (Some(cap), Some(am_base::db::now()))
        }
        None => (None, None),
    };

    let cap_val = new_cap;
    let since_val = new_since.clone();

    app.cfg
        .update(move |cfg| {
            cfg.claude_api.api_bot.total_cap_usd = cap_val;
            cfg.claude_api.api_bot.total_cap_since = since_val;
            Ok(())
        })
        .await
        .map_err(crate::api::cfg_err)?;

    let spent_usd = if let Some(ref s) = new_since {
        let microusd = query_total_cap_spent_microusd(&app, s)
            .await
            .map_err(|e| LcError::Bad(format!("query cap spent error: {e}")))?;
        microusd as f64 / 1_000_000.0
    } else {
        0.0
    };

    Ok(json!({
        "cap_usd": new_cap,
        "since": new_since,
        "spent_usd": spent_usd,
    }))
}
