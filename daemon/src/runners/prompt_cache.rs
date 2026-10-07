use std::sync::Arc;

use crate::db;
use crate::state::App;

/// 巡邏每輪對每顆 codex run 叫一次：讀 rollout 新增的部分，最後一筆 `token_count` 變了就推 `bot_status`。
/// 找不到 session／rollout、遠端主機、讀失敗都靜靜跳過（提示而已，不影響任何流程）。
pub async fn refresh_codex(app: &Arc<App>, run: &db::Run) {
    crate::app_ports_r2a8::refresh_codex(app, run).await;
}
