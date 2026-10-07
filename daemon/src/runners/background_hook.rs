use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;

use crate::db;
use crate::state::App;
use crate::background_hook::{parse, Snapshot};

/// claude 的 Stop hook 進來：有 `background_tasks` 就記下並更新 `background_jobs` 的數字（變了推 `bot_status`）。
/// 沒有這個鍵（舊版 claude）什麼都不做，數字留給畫面判斷。
pub async fn on_stop(app: &Arc<App>, run: &db::Run, payload: &Value) {
    let Some(reported) = parse(payload) else { return };
    let snap = Snapshot { at: Instant::now(), reported, services: None };
    let (n, had_shells) = (snap.jobs(), snap.reported.shells() > 0);
    let at = snap.at;
    let changed = {
        let mut hook = app.background_hook.lock().unwrap_or_else(|e| e.into_inner());
        let mut counts = app.background_jobs.lock().unwrap_or_else(|e| e.into_inner());
        let counted = crate::background_jobs::record(&mut counts, &run.id, n);
        let before = hook.get(&run.id).map(|s| s.reported.clone());
        let after = Some(snap.reported.clone());
        hook.insert(run.id.clone(), snap);
        counted || before != after
    };
    if changed {
        tracing::info!(run = %run.id, bot = %run.bot_id, background_jobs = n, source = "hook", "background jobs changed");
        app.emit_bot_status(&run.bot_id).await;
    }
    if had_shells {
        // 常駐服務要看行程樹（本機 ps、遠端 ssh，最多 10 秒）：不卡住 hook 的處理，查到再回頭修正數字。
        let (app, run) = (app.clone(), run.clone());
        tokio::spawn(async move { refine_services(&app, &run, at).await });
    }
}

async fn refine_services(app: &(impl crate::background_hook::HookSnapshots + crate::background_jobs::JobCounts + crate::capabilities::BotStatusEmit + crate::capabilities::Db + crate::capabilities::HerdrRoutes + crate::hosts::HostsAccess), run: &db::Run, at: Instant) {
    let Some(pane) = run.pane_id.as_deref() else { return };
    let Some(client) = app.herdr_for_run(run).await else { return };
    let services = crate::background_jobs::services(app, run, &client, pane).await;
    let n = {
        let mut hook = app.background_hook().lock().unwrap_or_else(|e| e.into_inner());
        // 這段時間裡來了新的 Stop（或被丟掉）：這份查詢結果不屬於它了。
        let Some(snap) = hook.get_mut(&run.id).filter(|s| s.at == at) else { return };
        snap.services = Some(services);
        snap.jobs()
    };
    let changed = crate::background_jobs::record(&mut app.background_jobs().lock().unwrap_or_else(|e| e.into_inner()), &run.id, n);
    if changed {
        tracing::info!(run = %run.id, bot = %run.bot_id, background_jobs = n, services, "background jobs changed (resident services deducted)");
        app.emit_bot_status(&run.bot_id).await;
    }
}
