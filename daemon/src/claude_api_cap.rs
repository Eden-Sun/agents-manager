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
        "turn_timeout_secs": cfg.claude_api.api_bot.turn_timeout_secs,
        "max_api_calls_per_turn": cfg.claude_api.api_bot.max_api_calls_per_turn,
    }))
}

/// PUT /api/claude-api/cap 請求 body
#[derive(Debug, Deserialize)]
pub struct PutCapRequest {
    /// 沒帶＝不動總上限；帶 `null`＝清除；帶數字＝設定。
    #[serde(default, deserialize_with = "present")]
    pub cap_usd: Option<Option<f64>>,
    /// 每回合時間上限（秒，1..=3600，且不得小於 `command_timeout_secs`）。沒帶＝不動。
    #[serde(default)]
    pub turn_timeout_secs: Option<u64>,
    /// 每回合 API 呼叫次數上限（1..=64）。沒帶＝不動。
    #[serde(default)]
    pub max_api_calls_per_turn: Option<u32>,
}

fn present<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// PUT /api/claude-api/cap
pub async fn put_claude_api_cap(app: Arc<App>, body: PutCapRequest) -> Result<Value, LcError> {
    let bad = |reason: &str| LcError::BadValue(json!({"error": "bad_request", "reason": reason}));
    if let Some(Some(cap)) = body.cap_usd {
        if cap < 0.0 || !cap.is_finite() {
            return Err(bad("invalid_cap_usd"));
        }
    }
    let current = app.cfg.get().await.claude_api.api_bot.clone();
    if let Some(t) = body.turn_timeout_secs {
        if !(1..=3600).contains(&t) {
            return Err(bad("invalid_turn_timeout_secs"));
        }
        if current.command_timeout_secs > t {
            return Err(bad("turn_timeout_below_command_timeout"));
        }
    }
    if body.max_api_calls_per_turn.is_some_and(|n| !(1..=64).contains(&n)) {
        return Err(bad("invalid_max_api_calls_per_turn"));
    }

    let cap_change = body.cap_usd;
    let (timeout, calls) = (body.turn_timeout_secs, body.max_api_calls_per_turn);
    let now = am_base::db::now();
    app.cfg
        .update(move |cfg| {
            let b = &mut cfg.claude_api.api_bot;
            match cap_change {
                Some(Some(cap)) => {
                    b.total_cap_usd = Some(cap);
                    b.total_cap_since = Some(now);
                }
                Some(None) => {
                    b.total_cap_usd = None;
                    b.total_cap_since = None;
                }
                None => {}
            }
            if let Some(t) = timeout {
                b.turn_timeout_secs = t;
            }
            if let Some(n) = calls {
                b.max_api_calls_per_turn = n;
            }
            Ok(())
        })
        .await
        .map_err(crate::api::cfg_err)?;
    let after = app.cfg.get().await.claude_api.api_bot.clone();
    let (new_cap, new_since) = (after.total_cap_usd, after.total_cap_since.clone());

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
        "turn_timeout_secs": after.turn_timeout_secs,
        "max_api_calls_per_turn": after.max_api_calls_per_turn,
    }))
}

#[cfg(test)]
mod turn_limit_tests {
    use super::*;

    fn req(v: Value) -> PutCapRequest {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn cap_field_distinguishes_absent_null_and_number() {
        assert_eq!(req(json!({})).cap_usd, None, "沒帶＝不動");
        assert_eq!(req(json!({"cap_usd": null})).cap_usd, Some(None), "null＝清除");
        assert_eq!(req(json!({"cap_usd": 5.0})).cap_usd, Some(Some(5.0)));
    }

    #[tokio::test]
    async fn put_changes_turn_limits_without_touching_the_cap_and_get_shows_them() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        put_claude_api_cap(app.clone(), req(json!({"cap_usd": 7.5}))).await.unwrap();
        let out = put_claude_api_cap(app.clone(), req(json!({"turn_timeout_secs": 2400, "max_api_calls_per_turn": 30}))).await.unwrap();
        assert_eq!(out["turn_timeout_secs"], json!(2400));
        assert_eq!(out["max_api_calls_per_turn"], json!(30));
        assert_eq!(out["cap_usd"], json!(7.5), "沒帶 cap_usd 不動總上限");
        let got = get_claude_api_cap(app.clone()).await.unwrap();
        assert_eq!((got["turn_timeout_secs"].clone(), got["max_api_calls_per_turn"].clone()), (json!(2400), json!(30)));
        // 舊用法：只帶 cap_usd:null 照樣清除。
        let cleared = put_claude_api_cap(app.clone(), req(json!({"cap_usd": null}))).await.unwrap();
        assert_eq!(cleared["cap_usd"], Value::Null);
        assert_eq!(cleared["turn_timeout_secs"], json!(2400));
    }

    #[tokio::test]
    async fn put_rejects_out_of_range_and_below_command_timeout() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        for body in [json!({"turn_timeout_secs": 0}), json!({"turn_timeout_secs": 3601}), json!({"max_api_calls_per_turn": 0}), json!({"max_api_calls_per_turn": 65})] {
            assert!(matches!(put_claude_api_cap(app.clone(), req(body)).await, Err(LcError::BadValue(_))));
        }
        let cmd = app.cfg.get().await.claude_api.api_bot.command_timeout_secs;
        let r = put_claude_api_cap(app.clone(), req(json!({"turn_timeout_secs": cmd - 1}))).await;
        assert!(matches!(r, Err(LcError::BadValue(v)) if v["reason"] == "turn_timeout_below_command_timeout"));
        assert_eq!(app.cfg.get().await.claude_api.api_bot.turn_timeout_secs, am_config::api_bot::default_turn_timeout_secs(), "被拒絕不改設定");
    }
}
