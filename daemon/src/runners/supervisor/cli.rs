//! App-backed HTTP endpoints for refreshing the installed AGM CLI.

use crate::state::App;
use axum::extract::State;
use axum::Json;
use serde_json::Value;
use std::sync::Arc;

/// `GET /api/supervisor/cli`.
pub async fn get_cli(State(app): State<Arc<App>>) -> Json<Value> {
    Json(crate::supervisor::cli_refresh::status(&app, crate::supervisor::setup::AGM_CLI).await)
}

/// `POST /api/supervisor/cli`：立刻刷新已安裝的 CLI，回傳更新後狀態；`refresh` 是每個角色這次的結果
/// （失敗的 `error` 在這裡看得到，不用去翻 daemon.log）。
pub async fn post_cli_refresh(State(app): State<Arc<App>>) -> Json<Value> {
    let refresh = crate::supervisor::cli_refresh::refresh_with(&app, crate::supervisor::setup::AGM_CLI).await;
    let mut out = crate::supervisor::cli_refresh::status(&app, crate::supervisor::setup::AGM_CLI).await;
    out["refresh"] = Value::Array(refresh);
    Json(out)
}
