use std::sync::Arc;

use crate::db;
use crate::state::App;

/// 巡邏每輪對每顆 codex run 叫一次：讀 rollout 新增的部分，最後一筆 `token_count` 變了就推 `bot_status`。
/// 找不到 session／rollout、遠端主機、讀失敗都靜靜跳過（提示而已，不影響任何流程）。
/// 遠端主機的讀取可能慢（SSH），另外 spawn、同一 run 同時最多一個（#1954 review），巡邏不等它；本機照舊直接讀。
pub async fn refresh_codex(app: &Arc<App>, run: &db::Run) {
    if matches!(db::bot_host(&app.db, &run.bot_id).await.as_deref(), Ok(host) if host != crate::config::LOCAL_HOST) {
        let run_id = run.id.clone();
        let (app, run) = (app.clone(), run.clone());
        crate::prompt_cache::spawn_remote_once(&run_id, Box::pin(async move { crate::app_ports_r2a8::refresh_codex(&app, &run).await }));
        return;
    }
    crate::app_ports_r2a8::refresh_codex(app, run).await;
}
