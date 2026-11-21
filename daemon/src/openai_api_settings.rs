//! `GET`／`PUT /api/openai-api/settings`：環境設定「OpenAI API」區（使用者 2026-11-18：「要跟 Claude 一樣可以設定」）。
//! 啟用開關（`[openai_api] enabled`）、金鑰狀態與 key_file 路徑、總花費上限與起算時間（`[openai_api.api_bot]`）、
//! 目前 OpenAI 累計花費（`api_bot_attempts` provider='openai' ＋ `spend.jsonl` provider=openai）。
//! **GET 只回 `key_present`／`key_error`，絕不回 key 內容**（錯誤原因是固定句型，不含 key）。UserOnly；PUT 走既有的設定寫入路徑，熱重載。
use std::sync::Arc;

use am_base::lc_error::LcError;
use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::{ConfigFile, API_PROVIDER_OPENAI};
use crate::state::App;

const EPOCH: &str = "1970-01-01T00:00:00.000Z";

/// OpenAI 從 `since`（沒設上限＝全部歷史）起的累計花費（微美元）：`(attempts, spend.jsonl)`。
async fn spent_microusd(app: &Arc<App>, since: &str) -> (i64, i64) {
    let attempts = crate::claude_api_cap::query_total_cap_spent_microusd(app, since, API_PROVIDER_OPENAI).await.unwrap_or(0);
    let since_dt = chrono::DateTime::parse_from_rfc3339(since).map(|d| d.with_timezone(&chrono::Utc)).unwrap_or(chrono::DateTime::UNIX_EPOCH);
    let jsonl = crate::claude_api::read_provider_spend_since(&app.data_dir, since_dt, API_PROVIDER_OPENAI).into_iter().map(|(_, m)| m).sum();
    (attempts, jsonl)
}

async fn settings_json(app: &Arc<App>, cfg: &ConfigFile) -> Value {
    let o = &cfg.openai_api;
    let key = crate::key_file::key_status(&o.key_file);
    let since = o.api_bot.total_cap_since.clone();
    let (attempts, jsonl) = spent_microusd(app, since.as_deref().unwrap_or(EPOCH)).await;
    let usd = |m: i64| m as f64 / 1_000_000.0;
    json!({
        "enabled": o.enabled,
        "key_file": o.key_file,
        "key_present": key.is_ok(),
        "key_error": key.err(),
        "api_bot": {
            "total_cap_usd": o.api_bot.total_cap_usd,
            "total_cap_since": since,
            "max_turn_cost_microusd": o.api_bot.max_turn_cost_microusd,
        },
        "spent_usd": usd(attempts.saturating_add(jsonl)),
        "spent_bots_usd": usd(attempts),
        "spent_uses_usd": usd(jsonl),
    })
}

pub(crate) async fn get_settings(State(app): State<Arc<App>>) -> Json<Value> {
    let cfg = app.cfg.claude_fresh().await;
    Json(settings_json(&app, &cfg).await)
}

/// 沒帶＝不動；`cap_usd`／`total_cap_since` 帶 `null`＝清除。
#[derive(Debug, Deserialize)]
pub(crate) struct PutSettings {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    key_file: Option<String>,
    #[serde(default, deserialize_with = "present")]
    total_cap_usd: Option<Option<f64>>,
    #[serde(default, deserialize_with = "present")]
    total_cap_since: Option<Option<String>>,
    /// 單回合成本上限（微美元，必須大於 0）：`null`＝清除（清了 OpenAI bot 無法啟動）。
    #[serde(default, deserialize_with = "present")]
    max_turn_cost_microusd: Option<Option<u64>>,
}

fn present<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

fn bad(reason: &str) -> LcError {
    LcError::BadValue(json!({"error": "bad_request", "reason": reason}))
}

pub(crate) async fn put_settings(State(app): State<Arc<App>>, Json(body): Json<PutSettings>) -> Result<Json<Value>, LcError> {
    if let Some(Some(cap)) = body.total_cap_usd {
        if !cap.is_finite() || cap < 0.0 {
            return Err(bad("invalid_total_cap_usd"));
        }
    }
    if let Some(Some(since)) = &body.total_cap_since {
        if chrono::DateTime::parse_from_rfc3339(since).is_err() {
            return Err(bad("invalid_total_cap_since"));
        }
    }
    if let Some(path) = &body.key_file {
        let p = path.trim();
        if p.is_empty() || p.len() > 512 || p.chars().any(char::is_control) || !(p.starts_with('/') || p.starts_with("~/")) {
            return Err(bad("invalid_key_file"));
        }
    }
    if let Some(Some(0)) = body.max_turn_cost_microusd {
        return Err(bad("invalid_max_turn_cost_microusd"));
    }
    let now = am_base::db::now();
    let PutSettings { enabled, key_file, total_cap_usd, total_cap_since, max_turn_cost_microusd } = body;
    app.cfg
        .update(move |cfg| {
            let o = &mut cfg.openai_api;
            if let Some(on) = enabled {
                o.enabled = on;
            }
            if let Some(path) = key_file {
                o.key_file = path.trim().to_string();
            }
            match total_cap_usd {
                Some(Some(cap)) => {
                    o.api_bot.total_cap_usd = Some(cap);
                    if total_cap_since.is_none() {
                        o.api_bot.total_cap_since = Some(now.clone());
                    }
                }
                Some(None) => {
                    o.api_bot.total_cap_usd = None;
                    o.api_bot.total_cap_since = None;
                }
                None => {}
            }
            if let Some(since) = total_cap_since {
                o.api_bot.total_cap_since = since;
            }
            if let Some(turn) = max_turn_cost_microusd {
                o.api_bot.max_turn_cost_microusd = turn;
            }
            Ok(())
        })
        .await
        .map_err(crate::api::cfg_err)?;
    let cfg = app.cfg.claude_fresh().await;
    Ok(Json(settings_json(&app, &cfg).await))
}
