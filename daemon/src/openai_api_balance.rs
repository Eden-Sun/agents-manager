use std::sync::Arc;
use anyhow::Result;
use chrono::{Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::lc_error::LcError;
use crate::state::App;
use crate::balance_utils::{AttemptSpent, microusd_to_usd};

pub fn routes() -> axum::Router<Arc<App>> {
    use axum::routing::get;
    axum::Router::new()
        .route("/openai-api/balance", get(get_openai_api_balance))
        .route("/openai-api/balance-anchor", axum::routing::put(put_openai_api_balance_anchor))
}

pub async fn get_openai_api_balance(
    app: Arc<App>,
) -> Result<Value, LcError> {
    let cfg = app.cfg.get().await;
    let anchor_usd = cfg.openai_api.balance_anchor_usd;
    let anchor_at_str = cfg.openai_api.balance_anchor_at.clone();

    let now = Utc::now();
    let as_of = am_base::db::iso_at(now);

    let (anchor_usd, anchor_at_str) = match (anchor_usd, anchor_at_str) {
        (Some(usd), Some(at)) => (usd, at),
        _ => {
            return Ok(json!({
                "configured": false,
                "as_of": as_of,
            }));
        }
    };

    let anchor_dt = match crate::claude_api_balance::parse_rfc3339(&anchor_at_str) {
        Ok(dt) => dt,
        Err(e) => {
            return Ok(json!({
                "configured": false,
                "error": format!("invalid balance_anchor_at: {e}"),
                "as_of": as_of,
            }));
        }
    };

    // 抓取本機帳（從 T 開始）
    let t_anchor = am_base::db::iso_at(anchor_dt);
    
    // 查詢 DB + spend.jsonl
    let local_attempts = match query_openai_attempts_since(&app, &t_anchor).await {
        Ok(atts) => atts,
        Err(e) => {
            return Err(LcError::Upstream(format!("failed to query local attempts: {e}")));
        }
    };

    let mut total_spent = 0.0;
    let mut uncertain_usd = 0.0;
    for att in local_attempts {
        if att.uncertain {
            uncertain_usd += microusd_to_usd(att.microusd);
        } else {
            total_spent += microusd_to_usd(att.microusd);
        }
    }
    
    let remaining_usd = anchor_usd - total_spent;

    Ok(json!({
        "configured": true,
        "anchor": {
            "usd": anchor_usd,
            "at": anchor_at_str,
        },
        "spent_usd": total_spent,
        "remaining_usd": remaining_usd,
        "uncertain_usd": uncertain_usd,
        "remaining_floor_usd": remaining_usd - uncertain_usd,
        "as_of": as_of,
    }))
}

pub async fn query_openai_attempts_since(app: &Arc<App>, since_rfc3339: &str) -> Result<Vec<AttemptSpent>> {
    let rows: Vec<(String, i64, bool)> = sqlx::query_as(
        "SELECT created_at,
                COALESCE(CASE WHEN state='recorded' THEN estimated_microusd
                              WHEN state='unknown' THEN COALESCE(estimated_microusd, reserved_microusd)
                              ELSE 0 END, 0),
                (state='unknown' AND estimated_microusd IS NULL)
         FROM api_bot_attempts
         WHERE provider = 'openai' AND created_at >= ? AND state IN ('recorded', 'unknown')
         ORDER BY created_at ASC"
    )
    .bind(since_rfc3339)
    .fetch_all(&app.db)
    .await?;

    let mut out: Vec<AttemptSpent> = rows.into_iter().map(|(created_at, microusd, uncertain)| AttemptSpent { created_at, microusd, uncertain }).collect();
    
    // spend.jsonl (provider='openai')
    if let Ok(since) = crate::claude_api_balance::parse_rfc3339(since_rfc3339) {
        out.extend(crate::claude_api::read_provider_spend_since(&app.data_dir, since, crate::config::API_PROVIDER_OPENAI).into_iter().map(|(created_at, microusd)| AttemptSpent { created_at, microusd, uncertain: false }));
    }
    
    Ok(out)
}

#[derive(Debug, Deserialize)]
pub struct PutBalanceAnchorRequest {
    pub usd: f64,
    pub at: Option<String>,
}

pub async fn put_openai_api_balance_anchor(
    app: Arc<App>,
    body: PutBalanceAnchorRequest,
) -> Result<Value, LcError> {
    if body.usd < 0.0 || !body.usd.is_finite() {
        return Err(LcError::BadValue(json!({"error": "bad_request", "reason": "invalid_usd"})));
    }

    let at_str = match body.at {
        Some(ref at) => {
            crate::claude_api_balance::parse_rfc3339(at).map_err(|e| LcError::BadValue(json!({"error": "bad_request", "reason": e.to_string()})))?;
            at.clone()
        }
        None => am_base::db::now(),
    };

    let usd_val = body.usd;
    let at_val = at_str.clone();

    app.cfg.update(move |cfg| {
        cfg.openai_api.balance_anchor_usd = Some(usd_val);
        cfg.openai_api.balance_anchor_at = Some(at_val);
        Ok(())
    }).await.map_err(crate::api::cfg_err)?;

    Ok(json!({
        "ok": true,
        "anchor": {
            "usd": usd_val,
            "at": at_str,
        }
    }))
}
