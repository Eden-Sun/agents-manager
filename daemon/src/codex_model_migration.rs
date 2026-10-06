//! Codex 0.157.0's startup model migration screen requires a person to choose an outcome. The same goes for the
//! other dialogs that sit in front of the composer and want a person's choice: the startup "Update available" menu
//! (default `1. Update now`) and the rate-limit model-switch popup ([`Dialog`]).
//!
//! Keep the dialog open, mark the run blocked (with the reason, in the conversation) when herdr misses the prompt,
//! and hold queued deliveries until the user finishes the choice. This flow never sends keys to the pane.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::db;
use crate::state::App;

const SETTLE: Duration = Duration::from_millis(800);

pub const WAITING_HINT: &str = "Codex 正停在模型升級提示，請到「終端」選 Try new model 或 Use existing model。daemon 不替你選，也不會把訊息打進選單；完成選擇後排隊的訊息會繼續送出。";
/// 啟動時的更新選單（預設選 `1. Update now`）。
pub const UPDATE_WAITING_HINT: &str = "Codex 更新提示等待選擇：啟動時跳出「Update available」選單（預設選 1. Update now）擋在輸入列前面。請到「終端」選 2. Skip（或 3. Skip until next version）；daemon 不替你選更新，也不會把訊息打進選單；完成選擇後排隊的訊息會繼續送出。";
/// 額度快用完時的換模型建議。
pub const RATE_LIMIT_WAITING_HINT: &str = "Codex 正在問要不要為了降低額度消耗切換模型；請到「終端」選擇，daemon 不會替你選，也不會把訊息打進選單；完成選擇後排隊的訊息會繼續送出。";
const CLOSED_NOTE: &str = "Codex 擋住輸入列的提示已關閉，排隊的訊息可繼續送出。";

/// 擋在輸入列前面、要使用者本人選的 codex 畫面。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialog {
    Migration,
    UpdateMenu,
    RateLimitSwitch,
}

impl Dialog {
    /// 畫面上現在開著的是哪一種（都不是＝`None`）。
    pub fn of(screen: &str) -> Option<Self> {
        if crate::tui_prompts::is_codex_model_migration_prompt(screen) {
            Some(Self::Migration)
        } else if crate::codex_update::update_menu_open(screen) {
            Some(Self::UpdateMenu)
        } else if crate::codex_live::rate_limit_switch_prompt_open(screen) {
            Some(Self::RateLimitSwitch)
        } else {
            None
        }
    }

    /// 結構化的原因（`blocked_reason`）：穩定的短代碼＋一句話。
    pub fn reason(self) -> crate::blocked_reason::Reason {
        match self {
            Self::Migration => crate::blocked_reason::Reason { code: "codex_migration", text: "codex 模型升級提示等待選擇".into() },
            Self::UpdateMenu => crate::blocked_reason::Reason { code: "codex_update_menu", text: "codex 更新提示等待選擇".into() },
            Self::RateLimitSwitch => crate::blocked_reason::Reason { code: "rate_limit_switch", text: "codex 額度換模型建議等待選擇".into() },
        }
    }

    /// 告訴使用者「為什麼卡住、要去哪裡處理」的那句話。
    pub fn hint(self) -> &'static str {
        match self {
            Self::Migration => WAITING_HINT,
            Self::UpdateMenu => UPDATE_WAITING_HINT,
            Self::RateLimitSwitch => RATE_LIMIT_WAITING_HINT,
        }
    }
}

struct Episode {
    /// 由這裡補標成 `blocked` 之前的狀態；`None` = herdr 自己判的。
    forced_from: Option<String>,
    /// Only one observer may finish the episode while it awaits a database restore.
    closing: bool,
    /// 開著的是哪一種（`blocked_reason` 靠它）。
    dialog: Dialog,
}

fn open() -> &'static Mutex<HashMap<String, Episode>> {
    static V: OnceLock<Mutex<HashMap<String, Episode>>> = OnceLock::new();
    V.get_or_init(Default::default)
}

/// 不在 `active` 裡的 run（結束了）不留開著的記錄：框開著時 run 就結束的話，沒有人會再讀到「框關了」，記錄只增不減。
pub fn retain_runs(active: &[String]) {
    open().lock().unwrap().retain(|id, _| active.contains(id));
}

/// 這個 run 現在開著的是哪個擋路的 codex 畫面（`blocked_reason` 讀）；沒有＝`None`。
pub fn open_dialog(run_id: &str) -> Option<Dialog> {
    open().lock().unwrap().get(run_id).filter(|e| !e.closing).map(|e| e.dialog)
}

#[cfg(test)]
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
            crate::child_alerts::on_child_blocked(app, run);
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
        let _ = crate::models::app_ports_p13::insert_message(
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
    crate::models::app_ports_p13::child_alerts_forget(&run.bot_id);
    crate::models::app_ports_p13::schedule_flush_queued(app, &run.bot_id);
}

async fn notify_once(app: &Arc<App>, run: &db::Run, dialog: Dialog) {
    {
        let mut episodes = open().lock().unwrap();
        if episodes.contains_key(&run.id) {
            return;
        }
        episodes.insert(run.id.clone(), Episode { forced_from: None, closing: false, dialog });
    }
    tracing::warn!(run = %run.id, bot = %run.bot_id, ?dialog, "Codex dialog is waiting for a user choice");
    match db::conversation_id(&app.db, &run.bot_id).await {
        Ok(conversation) => {
            let _ = crate::models::app_ports_p13::insert_message(
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

#[cfg(test)]
mod retain_tests {
    use super::*;

    #[test]
    fn an_episode_of_a_run_that_ended_is_dropped() {
        for id in ["cmm-gone", "cmm-kept"] {
            open().lock().unwrap().insert(
                id.to_string(),
                Episode { forced_from: None, closing: false, dialog: Dialog::Migration },
            );
        }
        retain_runs(&["cmm-kept".to_string()]);
        assert!(!is_open("cmm-gone") && is_open("cmm-kept"));
        retain_runs(&[]);
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

    /// 啟動時的更新選單與 rate-limit 切換選單一樣擋住輸入列、要使用者本人選：herdr 判成 idle 時要補標 blocked、
    /// 在對話裡講原因（網頁看得到、知道去 pane 處理）、一個鍵都不按；選單關掉自動回到原本的狀態、補一句「已關閉」並放行排隊的訊息。
    #[tokio::test]
    async fn other_startup_dialogs_block_with_their_own_reason_and_release_when_closed() {
        for (name, screen, reason_has) in [
            ("update", include_str!("lifecycle/fixtures/codex-0.155-update-menu.txt"), "更新"),
            ("rate", include_str!("lifecycle/fixtures/codex-0.157-rate-limit-switch.txt"), "切換模型"),
        ] {
            let env = tt::env().await;
            let app = env.app.clone();
            let bot = tt::claude_bot(&app, &env.project_id, &format!("codex-dialog-{name}")).await;
            sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
            let run_id = tt::fake_run(&app, &bot.id).await;
            let run = run_of(&app, &run_id).await;
            assert_eq!(run.agent_status, "idle", "前提：herdr 判成 idle");

            observe_screen(&app, &run, screen).await;
            assert_eq!(run_of(&app, &run_id).await.agent_status, "blocked", "{name}：要補標 blocked");
            assert!(is_open(&run_id), "{name}");
            let notes = system_messages(&app, &bot.id).await;
            assert_eq!(notes.len(), 1, "{name}：講一次原因就好：{notes:?}");
            assert!(notes[0].contains(reason_has) && notes[0].contains("終端"), "{name}：原因要講清楚、說去終端處理：{}", notes[0]);
            observe_screen(&app, &run_of(&app, &run_id).await, screen).await;
            assert_eq!(system_messages(&app, &bot.id).await.len(), 1, "{name}：同一個選單不重複講");
            assert!(env.herdr.calls_to("pane.send_keys").is_empty(), "{name}：不替使用者選");
            assert!(env.herdr.calls_to("pane.send_text").is_empty(), "{name}");

            observe_screen(&app, &run_of(&app, &run_id).await, "› Ask Codex to do anything\n").await;
            assert_eq!(run_of(&app, &run_id).await.agent_status, "idle", "{name}：關掉後回到原本的狀態");
            assert!(!is_open(&run_id), "{name}");
            assert_eq!(system_messages(&app, &bot.id).await.last().map(String::as_str), Some(CLOSED_NOTE), "{name}");
        }
    }

    #[tokio::test]
    async fn a_newer_herdr_status_supersedes_the_synthetic_block_before_queue_release() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "codex-migration-superseded").await;
        sqlx::query("UPDATE bots SET kind='codex' WHERE id=?")
            .bind(&bot.id)
            .execute(&app.db)
            .await
            .unwrap();
        let run_id = tt::fake_run(&app, &bot.id).await;
        let migration = include_str!("lifecycle/fixtures/codex-0.157-model-migration.txt");
        observe_screen(&app, &run_of(&app, &run_id).await, migration).await;
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?")
            .bind(&run_id)
            .execute(&app.db)
            .await
            .unwrap();
        crate::models::app_ports_p13::take_scheduled_flush_count(&bot.id);

        observe_screen(&app, &run_of(&app, &run_id).await, "› Ask Codex to do anything\n").await;

        assert_eq!(run_of(&app, &run_id).await.agent_status, "working", "do not overwrite herdr's newer state");
        assert!(!is_open(&run_id), "the newer status authoritatively superseded our marker");
        assert_eq!(system_messages(&app, &bot.id).await, [WAITING_HINT, CLOSED_NOTE]);
        assert_eq!(crate::models::app_ports_p13::take_scheduled_flush_count(&bot.id), 1);
    }

    #[tokio::test]
    async fn an_ended_run_releases_the_synthetic_blocked_episode() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "codex-migration-ended").await;
        sqlx::query("UPDATE bots SET kind='codex' WHERE id=?")
            .bind(&bot.id)
            .execute(&app.db)
            .await
            .unwrap();
        let run_id = tt::fake_run(&app, &bot.id).await;
        let migration = include_str!("lifecycle/fixtures/codex-0.157-model-migration.txt");
        observe_screen(&app, &run_of(&app, &run_id).await, migration).await;
        sqlx::query("UPDATE runs SET state='exited' WHERE id=?")
            .bind(&run_id)
            .execute(&app.db)
            .await
            .unwrap();
        crate::models::app_ports_p13::take_scheduled_flush_count(&bot.id);

        observe_screen(&app, &run_of(&app, &run_id).await, "› Ask Codex to do anything\n").await;

        let run = run_of(&app, &run_id).await;
        assert_eq!(run.state, "exited");
        assert_eq!(run.agent_status, "blocked", "don't rewrite an ended run");
        assert!(!is_open(&run_id), "the ended run authoritatively resolves the restore debt");
        assert_eq!(system_messages(&app, &bot.id).await, [WAITING_HINT, CLOSED_NOTE]);
        assert_eq!(crate::models::app_ports_p13::take_scheduled_flush_count(&bot.id), 1);
    }

    /// The restore marker is owed until SQLite commits it. A later patrol must retry before it
    /// writes the close note or wakes the prompt queue.
    #[tokio::test]
    async fn a_failed_restore_keeps_the_episode_until_a_later_patrol_succeeds() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "codex-migration-restore").await;
        sqlx::query("UPDATE bots SET kind='codex' WHERE id=?")
            .bind(&bot.id)
            .execute(&app.db)
            .await
            .unwrap();
        let run_id = tt::fake_run(&app, &bot.id).await;
        let migration = include_str!("lifecycle/fixtures/codex-0.157-model-migration.txt");
        observe_screen(&app, &run_of(&app, &run_id).await, migration).await;
        let conversation = db::conversation_id(&app.db, &bot.id).await.unwrap();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at)
             VALUES (?,?,'web','queued','pending','next prompt',?)",
        )
        .bind(db::ulid())
        .bind(&conversation)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        crate::models::app_ports_p13::take_scheduled_flush_count(&bot.id);
        sqlx::query(
            "CREATE TRIGGER refuse_codex_migration_restore BEFORE UPDATE OF agent_status ON runs
             WHEN OLD.agent_status='blocked' AND NEW.agent_status='idle' BEGIN SELECT RAISE(ABORT, 'database is locked'); END",
        )
        .execute(&app.db)
        .await
        .unwrap();

        observe_screen(&app, &run_of(&app, &run_id).await, "› Ask Codex to do anything\n").await;
        assert_eq!(run_of(&app, &run_id).await.agent_status, "blocked", "restore did not commit");
        assert!(is_open(&run_id), "the patrol needs the retained episode to retry");
        assert_eq!(system_messages(&app, &bot.id).await, [WAITING_HINT]);
        assert_eq!(crate::models::app_ports_p13::take_scheduled_flush_count(&bot.id), 0, "don't wake the queue yet");

        sqlx::query("DROP TRIGGER refuse_codex_migration_restore")
            .execute(&app.db)
            .await
            .unwrap();
        observe_screen(&app, &run_of(&app, &run_id).await, "› Ask Codex to do anything\n").await;
        assert_eq!(run_of(&app, &run_id).await.agent_status, "idle");
        assert!(!is_open(&run_id));
        assert_eq!(system_messages(&app, &bot.id).await, [WAITING_HINT, CLOSED_NOTE]);
        assert_eq!(crate::models::app_ports_p13::take_scheduled_flush_count(&bot.id), 1);

        observe_screen(&app, &run_of(&app, &run_id).await, "› Ask Codex to do anything\n").await;
        assert_eq!(system_messages(&app, &bot.id).await, [WAITING_HINT, CLOSED_NOTE], "closure note is written once");
        assert_eq!(crate::models::app_ports_p13::take_scheduled_flush_count(&bot.id), 0, "resolved episode wakes the queue once");
    }
}
