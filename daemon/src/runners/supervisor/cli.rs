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

/// `POST /api/supervisor/cli`：立刻刷新已安裝的 CLI，回傳更新後狀態。
pub async fn post_cli_refresh(State(app): State<Arc<App>>) -> Json<Value> {
    crate::supervisor::cli_refresh::refresh_with(&app, crate::supervisor::setup::AGM_CLI).await;
    Json(crate::supervisor::cli_refresh::status(&app, crate::supervisor::setup::AGM_CLI).await)
}
