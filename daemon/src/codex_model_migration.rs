//! Codex 0.157.0's startup model migration screen requires a person to choose an outcome.
//!
//! Keep the migration screen open, mark the run blocked when herdr misses the prompt, and hold
//! queued deliveries until the user finishes the choice. This flow never sends keys to the pane.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::db;
use crate::state::App;

const SETTLE: Duration = Duration::from_millis(800);

pub const WAITING_HINT: &str = "Codex 正停在模型升級提示，請到「終端」選 Try new model 或 Use existing model。daemon 不替你選，也不會把訊息打進選單；完成選擇後排隊的訊息會繼續送出。";
const CLOSED_NOTE: &str = "Codex 模型升級提示已關閉，排隊的訊息可繼續送出。";

struct Episode {
    /// 由這裡補標成 `blocked` 之前的狀態；`None` = herdr 自己判的。
    forced_from: Option<String>,
}

fn open() -> &'static Mutex<HashMap<String, Episode>> {
    static V: OnceLock<Mutex<HashMap<String, Episode>>> = OnceLock::new();
    V.get_or_init(Default::default)
}

pub fn is_open(run_id: &str) -> bool {
    open().lock().unwrap().contains_key(run_id)
}

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

pub(crate) async fn observe_screen(app: &Arc<App>, run: &db::Run, screen: &str) {
    if crate::tui_prompts::is_codex_model_migration_prompt(screen) {
        notify_once(app, run).await;
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
            crate::child_alerts::on_child_blocked(app, run);
        }
        return;
    }

    let Some(episode) = open().lock().unwrap().remove(&run.id) else {
        return;
    };
    tracing::info!(run = %run.id, bot = %run.bot_id, "Codex 模型升級提示已關閉");
    if let Ok(conversation) = db::conversation_id(&app.db, &run.bot_id).await {
        let _ = crate::lifecycle::insert_message(
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
    let mut restored = false;
    if let Some(previous) = episode.forced_from {
        restored =
            sqlx::query("UPDATE runs SET agent_status=? WHERE id=? AND agent_status='blocked'")
                .bind(previous)
                .bind(&run.id)
                .execute(&app.db)
                .await
                .map(|result| result.rows_affected() == 1)
                .unwrap_or(false);
    }
    if restored {
        app.emit_bot_status(&run.bot_id).await;
    }
    crate::child_alerts::forget(&run.bot_id);
    crate::lifecycle::schedule_flush_queued(app, &run.bot_id);
}

async fn notify_once(app: &Arc<App>, run: &db::Run) {
    {
        let mut episodes = open().lock().unwrap();
        if episodes.contains_key(&run.id) {
            return;
        }
        episodes.insert(run.id.clone(), Episode { forced_from: None });
    }
    tracing::warn!(run = %run.id, bot = %run.bot_id, "Codex model migration prompt is waiting for a user choice");
    match db::conversation_id(&app.db, &run.bot_id).await {
        Ok(conversation) => {
            let _ = crate::lifecycle::insert_message(
                app,
                &conversation,
                None,
                "system",
                WAITING_HINT,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing as tt;

    async fn run_of(app: &Arc<App>, run_id: &str) -> db::Run {
        sqlx::query_as::<_, db::Run>("SELECT * FROM runs WHERE id=?")
            .bind(run_id)
            .fetch_one(&app.db)
            .await
            .unwrap()
    }

    async fn system_messages(app: &Arc<App>, bot_id: &str) -> Vec<String> {
        let conversation = db::conversation_id(&app.db, bot_id).await.unwrap();
        sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id=? AND role='system' ORDER BY created_at, rowid")
            .bind(conversation)
            .fetch_all(&app.db)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn migration_screen_blocks_without_sending_keys_and_releases_after_user_choice() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "codex-migration").await;
        sqlx::query("UPDATE bots SET kind='codex' WHERE id=?")
            .bind(&bot.id)
            .execute(&env.app.db)
            .await
            .unwrap();
        let run_id = tt::fake_run(&env.app, &bot.id).await;
        let run = run_of(&env.app, &run_id).await;
        let migration = include_str!("lifecycle/fixtures/codex-0.157-model-migration.txt");

        observe_screen(&env.app, &run, migration).await;
        assert_eq!(run_of(&env.app, &run_id).await.agent_status, "blocked");
        assert!(is_open(&run_id));
        let notices = system_messages(&env.app, &bot.id).await;
        assert_eq!(notices, [WAITING_HINT]);
        assert!(
            env.herdr.calls_to("pane.send_keys").is_empty(),
            "the daemon must leave the user's choice untouched"
        );

        observe_screen(
            &env.app,
            &run_of(&env.app, &run_id).await,
            "› Ask Codex to do anything\n",
        )
        .await;
        assert_eq!(run_of(&env.app, &run_id).await.agent_status, "idle");
        assert!(!is_open(&run_id));
        assert_eq!(env.herdr.calls_to("pane.send_keys").len(), 0);
        let messages = system_messages(&env.app, &bot.id).await;
        assert_eq!(messages, [WAITING_HINT, CLOSED_NOTE]);
    }
}
