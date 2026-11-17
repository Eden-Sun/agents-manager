//! 背景工作跑超過一小時（#2049）：跨過門檻時對該 bot 送一次 daemon 通知，提醒它確認還需不需要。
//! 只提醒、不替 bot 停任何工作；每段背景只送一次（歸零後新的一段才會再送）。
use std::{sync::Arc, time::Duration};

use crate::{db, lifecycle, state::App};

const POLL: Duration = Duration::from_secs(60);

pub fn spawn(app: Arc<App>) {
    crate::background_loop::spawn_restartable(&app, "background stuck notice", {
        let app = app.clone();
        move || {
            let app = app.clone();
            async move {
                loop {
                    tokio::select! {
                        biased;
                        _ = app.shutdown.cancelled() => return,
                        _ = tokio::time::sleep(POLL) => {}
                    }
                    notify_due(&app).await;
                }
            }
        }
    });
}

async fn notify_due(app: &Arc<App>) {
    // 鎖只在這一行持有，不跨 await。
    let due = crate::background_jobs::take_newly_stuck_now(&mut app.background_jobs.lock().unwrap_or_else(|e| e.into_inner()));
    for (run_id, since, n) in due {
        let bot_id = match db::run(&app.db, &run_id).await {
            Ok(Some(run)) => run.bot_id,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(%error, run = %run_id, "background stuck notice: run lookup failed");
                continue;
            }
        };
        tracing::info!(run = %run_id, bot = %bot_id, n, since, "background work stuck over 1h; notifying bot");
        app.emit_bot_status(&bot_id).await;
        let text = format!(
            "[daemon 通知，不是新請求] 你有 {n} 個背景工作已持續超過 60 分鐘。請確認還需不需要：不需要就停掉（Claude 用 TaskStop；其他 CLI 用自己記下的 PID kill）；要長駐的請改放 herdr shell pane。AG Man 不會替你停，這一段只提醒這一次。"
        );
        if let Err(error) = lifecycle::prompt_from_api_queue_if_busy(
            app,
            &bot_id,
            &text,
            &format!("bg-stuck:{run_id}:{since}"),
            &[],
            lifecycle::RelaySrc::daemon_notice(Some("daemon")),
            false,
            None,
        )
        .await
        {
            tracing::warn!(error = ?error, run = %run_id, bot = %bot_id, "background stuck notice: queue failed");
        }
    }
}
