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
        notify_once(app, run, dialog).await;
        if run.agent_status == "blocked" {
            return;
        }

        let previous = run.agent_status.clone();
        let marked =
            sqlx::query("UPDATE runs SET agent_status='blocked' WHERE id=? AND agent_status=?")
                .bind(&run.id)
                .bind(&previous)
                .execute(&app.db)
                .await
                .map(|result| result.rows_affected() == 1)
                .unwrap_or(false);
        if marked {
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
        match sqlx::query("UPDATE runs SET agent_status=? WHERE id=? AND state='running' AND agent_status='blocked'")
            .bind(previous)
            .bind(&run.id)
            .execute(&app.db)
            .await
        {
            Ok(result) if result.rows_affected() == 1 => (true, true),
            Ok(_) => match sqlx::query_as::<_, (String, String)>("SELECT state, agent_status FROM runs WHERE id=?")
                .bind(&run.id)
                .fetch_optional(&app.db)
                .await
            {
                Ok(Some((state, status))) => (state != "running" || status != "blocked", false),
                Ok(None) => (true, false),
                Err(error) => {
                    tracing::warn!(run = %run.id, error = ?error, "could not verify synthetic Codex migration blocked status");
                    (false, false)
                }
            },
            Err(error) => {
                tracing::warn!(run = %run.id, error = ?error, "could not restore synthetic Codex migration blocked status");
                (false, false)
            }
        }
    } else {
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

async fn notify_once(app: &(impl crate::capabilities::Db + crate::capabilities::Emit), run: &db::Run, dialog: Dialog) {
    {
        let mut episodes = open().lock().unwrap();
        if episodes.contains_key(&run.id) {
            return;
        }
        episodes.insert(run.id.clone(), Episode { forced_from: None, closing: false, dialog });
    }
    tracing::warn!(run = %run.id, bot = %run.bot_id, ?dialog, "Codex dialog is waiting for a user choice");
    match db::conversation_id(app.db(), &run.bot_id).await {
        Ok(conversation) => {
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
}
