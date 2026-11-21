//! OpenAI API 全域餘額（使用者 2026-11-19：放在 Claude 餘額下面）。
//! 花費＝錨點之後的本機帳：`api_bot_attempts`（provider='openai'）加 `claude-api/spend.jsonl` 裡 provider='openai' 的行。
//! 沒有官方餘額端點，所以只能「錨點－本機已確定花費」推算；結果不明又沒估價的 attempt 另列 `uncertain_usd`（預留上限，最壞情況）。
use std::sync::Arc;

use anyhow::Result;
use axum::{extract::State, Json};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::balance_utils::{microusd_to_usd, AttemptSpent};
use crate::lc_error::LcError;
use crate::state::App;

pub fn routes() -> axum::Router<Arc<App>> {
    use axum::routing::{get, put};
    axum::Router::new()
        .route("/openai-api/balance", get(get_balance))
        .route("/openai-api/balance-anchor", put(put_balance_anchor))
}

/// GET /api/openai-api/balance（User-only）。
pub(crate) async fn get_balance(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    Ok(Json(get_openai_api_balance(&app).await?))
}

/// PUT /api/openai-api/balance-anchor（User-only）。
pub(crate) async fn put_balance_anchor(
    State(app): State<Arc<App>>,
    Json(body): Json<PutBalanceAnchorRequest>,
) -> Result<Json<Value>, LcError> {
    Ok(Json(put_openai_api_balance_anchor(&app, body).await?))
}

pub async fn get_openai_api_balance(app: &Arc<App>) -> Result<Value, LcError> {
    let cfg = app.cfg.get().await;
    let enabled = cfg.openai_api.enabled;
    let anchor_usd = cfg.openai_api.balance_anchor_usd;
    let anchor_at_str = cfg.openai_api.balance_anchor_at.clone();
    let as_of = am_base::db::iso_at(Utc::now());

    if !enabled {
        return Ok(json!({ "enabled": false, "configured": false, "as_of": as_of }));
    }

    let (anchor_usd, anchor_at_str) = match (anchor_usd, anchor_at_str) {
        (Some(usd), Some(at)) => (usd, at),
        _ => return Ok(json!({ "enabled": true, "configured": false, "as_of": as_of })),
    };

    let anchor_dt = match crate::claude_api_balance::parse_rfc3339(&anchor_at_str) {
        Ok(dt) => dt,
        Err(e) => {
            return Ok(json!({
                "enabled": true,
                "configured": false,
                "error": format!("invalid balance_anchor_at: {e}"),
                "as_of": as_of,
            }));
        }
    };

    let t_anchor = am_base::db::iso_at(anchor_dt);
    let local_attempts = query_openai_attempts_since(app, &t_anchor)
        .await
        .map_err(|e| LcError::Upstream(format!("failed to query local attempts: {e}")))?;

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
        "enabled": true,
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
         ORDER BY created_at ASC",
    )
    .bind(since_rfc3339)
    .fetch_all(&app.db)
    .await?;

    let mut out: Vec<AttemptSpent> = rows
        .into_iter()
        .map(|(created_at, microusd, uncertain)| AttemptSpent { created_at, microusd, uncertain })
        .collect();

    if let Ok(since) = crate::claude_api_balance::parse_rfc3339(since_rfc3339) {
        out.extend(
            crate::claude_api::read_provider_spend_since(&app.data_dir, since, crate::config::API_PROVIDER_OPENAI)
                .into_iter()
                .map(|(created_at, microusd)| AttemptSpent { created_at, microusd, uncertain: false }),
        );
    }

    Ok(out)
}

#[derive(Debug, Deserialize)]
pub struct PutBalanceAnchorRequest {
    pub usd: f64,
    pub at: Option<String>,
}

pub async fn put_openai_api_balance_anchor(app: &Arc<App>, body: PutBalanceAnchorRequest) -> Result<Value, LcError> {
    if !body.usd.is_finite() || body.usd < 0.0 {
        return Err(LcError::BadValue(json!({"error": "bad_request", "reason": "invalid_usd"})));
    }

    let at_str = match body.at {
        Some(ref at) => {
            crate::claude_api_balance::parse_rfc3339(at)
                .map_err(|e| LcError::BadValue(json!({"error": "bad_request", "reason": e.to_string()})))?;
            at.clone()
        }
        None => am_base::db::now(),
    };

    let usd_val = body.usd;
    let at_val = at_str.clone();
    app.cfg
        .update(move |cfg| {
            cfg.openai_api.balance_anchor_usd = Some(usd_val);
            cfg.openai_api.balance_anchor_at = Some(at_val);
            Ok(())
        })
        .await
        .map_err(crate::api::cfg_err)?;

    Ok(json!({
        "ok": true,
        "anchor": {
            "usd": usd_val,
            "at": at_str,
        }
    }))
}

