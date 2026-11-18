use std::sync::Arc;
use std::time::Duration;

use crate::db;
use crate::state::App;
use crate::codex_model_migration::{open, Dialog, Episode, CLOSED_NOTE};

const SETTLE: Duration = Duration::from_millis(800);

pub fn on_blocked(app: &Arc<App>, run: &db::Run) {
    let (app, run) = (app.clone(), run.clone());
    tokio::spawn(async move {
        tokio::time::sleep(SETTLE).await;
        observe(&app, &run).await;
    });
}

pub async fn observe(app: &Arc<App>, run: &db::Run) {
    if !matches!(db::bot(&app.db, &run.bot_id).await, Ok(Some(bot)) if bot.kind == "codex") {
        return;
    }
    let Some(pane) = run.pane_id.as_deref().filter(|p| !p.trim().is_empty()) else {
        return;
    };
    let Some(client) = app.herdr_for_run(run).await else {
        return;
    };
    let Ok(read) = client.pane_read(pane, "visible", 80).await else {
        return;
    };
    observe_screen(app, run, &read.text).await;
}

pub async fn observe_screen(app: &Arc<App>, run: &db::Run, screen: &str) {
    if let Some(dialog) = Dialog::of(screen) {
        if run.agent_status == "blocked" {
            if db::compare_and_set_running_run_agent_status(&app.db, run, "blocked", "blocked").await.unwrap_or(false) {
                let changed = notify_once(app, run, dialog).await;
                if changed {
                    // herdr 不會把 hook 審核框判成 blocked（現場五顆都是 idle）：run 已經是 blocked 而記憶體裡沒有 episode，
                    // 就是 daemon 重啟前補標的。記回「原本是 idle」，框關掉才還原得回去，不會一直掛著 blocked。
                    if dialog == Dialog::HookReview {
                        if let Some(episode) = open().lock().unwrap().get_mut(&run.id) {
                            episode.forced_from.get_or_insert_with(|| "idle".to_string());
                        }
                    }
                    app.emit_bot_status(&run.bot_id).await;
                }
            }
            // 框換成另一種時 blocked_reason 跟著變，網頁要重讀一次。
            return;
        }

        let previous = run.agent_status.clone();
        let marked = db::compare_and_set_running_run_agent_status(&app.db, run, &previous, "blocked").await.unwrap_or(false);
        if marked {
            notify_once(app, run, dialog).await;
            if let Some(episode) = open().lock().unwrap().get_mut(&run.id) {
                episode.forced_from.get_or_insert(previous);
            }
            app.emit_bot_status(&run.bot_id).await;
            crate::runners::child_alerts::on_child_blocked(app, run);
        }
        return;
    }

    let forced_from = {
        let mut episodes = open().lock().unwrap();
        let Some(episode) = episodes.get_mut(&run.id) else {
            return;
        };
        if episode.closing {
            return;
        }
        episode.closing = true;
        episode.forced_from.clone()
    };
    let (resolved, status_changed) = if let Some(previous) = &forced_from {
        match db::compare_and_set_running_run_agent_status(&app.db, run, "blocked", previous).await {
            Ok(true) => (true, true),
            Ok(false) => match db::is_current_running_run(&app.db, run).await {
                Ok(true) => (true, false),
                Ok(false) => {
                    open().lock().unwrap().remove(&run.id);
                    return;
                }
                Err(error) => {
                    tracing::warn!(run = %run.id, error = ?error, "could not verify current Codex migration run");
                    (false, false)
                }
            },
            Err(error) => {
                tracing::warn!(run = %run.id, error = ?error, "could not restore synthetic Codex migration blocked status");
                (false, false)
            }
        }
    } else {
        if !db::is_current_running_run(&app.db, run).await.unwrap_or(false) {
            open().lock().unwrap().remove(&run.id);
            return;
        }
        (true, false)
    };
    if !resolved {
        if let Some(episode) = open().lock().unwrap().get_mut(&run.id) {
            episode.closing = false;
        }
        return;
    }
    open().lock().unwrap().remove(&run.id);
    tracing::info!(run = %run.id, bot = %run.bot_id, "Codex 擋住輸入列的提示已關閉");
    if let Ok(conversation) = db::conversation_id(&app.db, &run.bot_id).await {
        let _ = crate::app_ports_p13::insert_message(
            app,
            &conversation,
            None,
            "system",
            CLOSED_NOTE,
            "system",
            false,
            None,
        )
        .await;
    }
    if status_changed {
        app.emit_bot_status(&run.bot_id).await;
    }
    crate::app_ports_p13::child_alerts_forget(&run.bot_id);
    crate::app_ports_p13::schedule_flush_queued(app, &run.bot_id);
}

/// 回傳是否新開或換了一種框（要講提示、原因要重推）；同一種框再看到就不動。
async fn notify_once(app: &(impl crate::capabilities::Db + crate::capabilities::Emit), run: &db::Run, dialog: Dialog) -> bool {
    {
        let mut episodes = open().lock().unwrap();
        match episodes.get_mut(&run.id) {
            Some(e) if e.closing || e.dialog == dialog => return false,
            // 換了一種框：原因跟著換、提示再講一次。
            Some(e) => e.dialog = dialog,
            None => {
                episodes.insert(run.id.clone(), Episode { forced_from: None, closing: false, dialog });
            }
        }
    }
    tracing::warn!(run = %run.id, bot = %run.bot_id, ?dialog, "Codex dialog is waiting for a user choice");
    match db::conversation_id(app.db(), &run.bot_id).await {
        Ok(conversation) => {
            // episode 只在記憶體：daemon 重啟（部署）後同一個框會再被看到一次。聊天室最後一則已經是這句提示就不重貼
            // （director 2026-11-18 兩小時內因部署被貼了 8 次）；中間有別的訊息才再講。
            let last: Result<Option<(String, String)>, _> =
                sqlx::query_as("SELECT role, content FROM messages WHERE conversation_id = ? ORDER BY created_at DESC, rowid DESC LIMIT 1")
                    .bind(&conversation)
                    .fetch_optional(app.db())
                    .await;
            if matches!(&last, Ok(Some((role, content))) if role == "system" && content == dialog.hint()) {
                return true;
            }
            let _ = crate::app_ports_p13::insert_message(
                app,
                &conversation,
                None,
                "system",
                dialog.hint(),
                "system",
                false,
                None,
            )
            .await;
        }
        Err(error) => {
            tracing::warn!(run = %run.id, error = ?error, "could not post the Codex model migration notice")
        }
    }
    true
}
