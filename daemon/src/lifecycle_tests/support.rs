//! lifecycle 測試共用的 App 端輔助（需要 daemon 的 App／supervisor，留在 daemon）。
use crate::db;
use crate::state::App;
use std::sync::Arc;

pub async fn bot_stopped_reported(app: &Arc<App>, bot_id: &str) -> bool {
    let t = crate::supervisor::incidents::Thresholds {
        host_disconnected_secs: 120,
        bot_stopped_secs: 300,
        assignment_stalled_secs: 7200,
        notify_max_attempts: 5,
        approval_stalled_secs: 1800,
        classify_failures: crate::supervisor::incidents::CLASSIFY_FAILURE_LIMIT,
        spool_fold_stuck_rounds: crate::supervisor::incidents::SPOOL_FOLD_STUCK_LIMIT,
    };
    crate::supervisor::incidents::observe(app, &t).await.seen.iter().any(|o| o.kind == "bot_stopped" && o.resource == bot_id)
}

pub async fn watch_run_pane(app: &Arc<App>, run_id: &str) -> (String, String, String) {
    let run = db::run(&app.db, run_id).await.unwrap().unwrap();
    let host = db::bot_host(&app.db, &run.bot_id).await.unwrap();
    let session = app.session_for_run(&run).await.unwrap();
    let key = (host, session, run.pane_id.clone().unwrap());
    app.pane_watchers.lock().await.insert(key.clone(), tokio::spawn(std::future::pending::<()>()));
    key
}

pub async fn watched(app: &Arc<App>, key: &(String, String, String)) -> bool {
    app.pane_watchers.lock().await.contains_key(key)
}
