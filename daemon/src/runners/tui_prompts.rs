//! `tui_prompts` 背景巡邏 runner。

use crate::db;
use crate::state::App;
use crate::tui_prompts::dismiss_if_survey;
use std::sync::Arc;
use std::time::Duration;

const SWEEP: Duration = Duration::from_secs(10);

/// `idle` 也掃：問卷在回合結束後插入，herdr 可能只判成 idle，下一句話就會打進問卷裡。
pub fn spawn_survey_watcher(app: Arc<App>) {
    crate::background_loop::spawn_periodic(&app, "survey watcher", SWEEP, SWEEP, |app| async move {
        let runs = db::all_active_runs(&app.db).await.unwrap_or_default();
        crate::session_paused::forget_ended(&app, &runs).await;
        crate::dangerous_rm::forget_ended(&app, &runs).await;
        for run in runs {
            if run.agent_status == "blocked" || run.agent_status == "idle" || crate::dangerous_rm::is_open(&run.id) {
                dismiss_if_survey(&app, &run).await;
                // 防誤刪框：herdr 判成 idle 時的安全網，也負責框關掉之後的收尾（不按任何鍵）。
                crate::runners::dangerous_rm::observe(&app, &run).await;
            }
            // Session paused 選單：herdr 判 idle 時補標 blocked、選單關掉時還原（不按任何鍵）。
            if run.agent_status == "idle" || crate::session_paused::is_forced(&run.id) {
                crate::runners::session_paused::observe(&app, &run).await;
            }
            // Codex 在 starting／working 狀態也可能停在啟動遷移框；只查畫面，不替使用者選。
            crate::runners::codex_model_migration::observe(&app, &run).await;
            // claude 停在一般權限確認選單：事件那一刻漏掉、或選單換了一種工具，這裡每輪重讀一次（只看、不按鍵）。
            if run.agent_status == "blocked" {
                crate::blocked_reason::observe(&app, &run).await;
            }
        }
    });
}
