//! 使用者時區（`[agents] user_time_zone`）：`GET`／`PUT /api/settings/time-zone`。
//! 寫進 config.toml 的 `[agents]`；新啟動的 bot 讀到新值（已在跑的 bot 不會重讀）。

use std::sync::Arc;

use axum::{extract::State, Extension, Json};
use serde_json::{json, Value};

use crate::{api::RequestPrincipal, lifecycle::LcError, state::App};

fn body(time_zone: &str) -> Value {
    let offset = am_base::user_time_zone::utc_offset_label(time_zone, chrono::Utc::now()).unwrap_or_default();
    json!({"time_zone": time_zone, "utc_offset": offset})
}

pub(crate) async fn get(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let agents = app.cfg.agents_fresh().await;
    Ok(Json(body(agents.time_zone())))
}

pub(crate) async fn put(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<RequestPrincipal>,
    Json(input): Json<Value>,
) -> Result<Json<Value>, LcError> {
    if principal != RequestPrincipal::User {
        return Err(LcError::Forbidden(json!({"error":"forbidden", "reason":"user_only"})));
    }
    let tz = input.get("time_zone").and_then(Value::as_str).map(str::trim).unwrap_or_default().to_string();
    if !am_base::user_time_zone::is_valid_time_zone(&tz) {
        return Err(LcError::BadValue(json!({"error":"bad_time_zone"})));
    }
    app.cfg
        .update(move |cfg| {
            cfg.agents.user_time_zone = Some(tz);
            Ok(())
        })
        .await
        .map_err(crate::api::cfg_err)?;
    get(State(app)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn time_zone_round_trips_and_rejects_unknown_names() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let Json(before) = get(State(app.clone())).await.unwrap();
        assert_eq!(before["time_zone"], "Asia/Taipei");
        assert_eq!(before["utc_offset"], "+8");

        let bad = put(State(app.clone()), Extension(RequestPrincipal::User), Json(json!({"time_zone":"Mars/Olympus"}))).await;
        assert!(matches!(bad, Err(LcError::BadValue(_))));
        let missing = put(State(app.clone()), Extension(RequestPrincipal::User), Json(json!({}))).await;
        assert!(matches!(missing, Err(LcError::BadValue(_))));
        let bot = put(State(app.clone()), Extension(RequestPrincipal::Bot("test".into())), Json(json!({"time_zone":"America/New_York"}))).await;
        assert!(matches!(bot, Err(LcError::Forbidden(_))));

        let Json(after) = put(State(app.clone()), Extension(RequestPrincipal::User), Json(json!({"time_zone":"America/New_York"}))).await.unwrap();
        assert_eq!(after["time_zone"], "America/New_York");
        assert_eq!(after["utc_offset"], "-5");
        let Json(read) = get(State(app)).await.unwrap();
        assert_eq!(read["time_zone"], "America/New_York");
    }
}
