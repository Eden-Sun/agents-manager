//! Claude Code 下載好新版之後，只會在 pane 最底下那行印一句
//!
//! ```text
//! ✔ Update installed · Restart to update
//! ```
//!
//! 就沒有別的動靜了——不是事件、不會消失、也不影響回合。使用者要一路點進終端才看得到，等於
//! 沒通知。這支巡邏把那句話讀出來掛到 run 上（`runs.update_notice`），web 才畫得出 header 上
//! 那顆點得下去的徽章（點了就是 `POST /api/bots/{id}/restart`，重啟就是套用更新）。
//!
//! 掛在 run 而不是 bot：等著被套用的更新是**這個 claude process** 的事，重啟後那個 process
//! 沒了，新 run 的欄位本來就是 NULL。
//!
//! `running` 的 claude run 全掃，不像 [`crate::tui_prompts::spawn_survey_watcher`] 只掃停著
//! 的：那句是在回合結束時印的，但使用者下一句話送出去之後 run 就變 `working`，通知還在畫面
//! 上、也還該看得見。

use crate::db;
use crate::state::App;
use crate::tui_prompts::update_notice;
use std::sync::Arc;
use std::time::Duration;

/// 巡邏間隔。更新一天撞不到幾次，晚 30 秒知道沒有差別，但每個 claude pane 都要一次
/// `pane.read`，所以比問卷那支（10 秒）鬆。
const SWEEP: Duration = Duration::from_secs(30);

/// 掃一輪：每個活著的 claude run 讀一次畫面，跟 DB 裡的值不同才寫回去並通知 web。
async fn sweep(app: &Arc<App>) {
    let runs = db::all_active_runs(&app.db).await.unwrap_or_default();
    for run in runs.into_iter().filter(|r| r.state == "running") {
        if !matches!(db::bot(&app.db, &run.bot_id).await, Ok(Some(b)) if b.kind == "claude") {
            continue;
        }
        let Some(pane) = run.pane_id.clone() else { continue };
        let Some(client) = app.herdr_for_run(&run).await else { continue };
        // 讀不到畫面（pane 沒了、主機斷線）就跳過，不要把已經看到的通知清掉。
        let Ok(read) = client.pane_read(&pane, "visible", 80).await else { continue };
        let seen = update_notice(&read.text);
        if seen.as_deref() == run.update_notice.as_deref() {
            continue;
        }
        if seen.is_some() {
            tracing::info!(run = %run.id, bot = %run.bot_id, "claude 有新版等著重啟套用");
        }
        let _ = sqlx::query("UPDATE runs SET update_notice = ? WHERE id = ?")
            .bind(seen.as_deref())
            .bind(&run.id)
            .execute(&app.db)
            .await;
        app.emit_bot_status(&run.bot_id).await;
    }
}

/// 每 [`SWEEP`] 掃一次。
pub fn spawn_update_watcher(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(SWEEP).await;
            sweep(&app).await;
        }
    });
}
