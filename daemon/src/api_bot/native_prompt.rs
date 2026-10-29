//! Tracked ownership of a single durable API prompt admission.
use crate::lifecycle::api_bot_port::{ApiNativePromptError as E, ApiTurnKey};
use crate::lifecycle::PromptOut;
use std::{future::Future, sync::Arc};

#[cfg(all(test, feature = "daemon-test-harness"))]
fn pending_lost_acks() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static FAULTS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> = std::sync::OnceLock::new();
    FAULTS.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}
#[cfg(all(test, feature = "daemon-test-harness"))]
pub(super) fn arm_pending_lost_ack_for_test(crid: &str) { assert!(pending_lost_acks().lock().unwrap().insert(crid.to_owned())); }

pub(super) fn prompt<'a>(app: &'a Arc<crate::state::App>, bot: &'a str, text: &'a str, crid: &'a str)
    -> impl Future<Output = Result<PromptOut, E>> + Send + 'a {
    let app = app.clone();
    let bot = bot.to_owned();
    let text = text.to_owned();
    let crid = crid.to_owned();
    let tasks = app.background_tasks.clone();
    Box::pin(async move {
        let task = tasks.spawn(Box::pin(async move { prompt_body(&app, &bot, &text, &crid).await }));
        task.await.unwrap_or(Err(E::Unconfirmed))
    })
}

fn prompt_body<'a>(app: &'a Arc<crate::state::App>, bot: &'a str, text: &'a str, crid: &'a str) -> impl Future<Output = Result<PromptOut, E>> + Send + 'a {
    Box::pin(prompt_body_inner(app, bot, text, crid))
}
async fn prompt_body_inner(app: &Arc<crate::state::App>, bot: &str, text: &str, crid: &str) -> Result<PromptOut, E> {
    use crate::lifecycle::send_now::ports::MaintenancePort;
    if !app.api_startup_gate.ready() || app.shutdown.is_cancelled() || text.trim().is_empty() || crid.trim().is_empty() { return Err(E::Unconfirmed); }
    let _lock = super::explicit_retirement::ApiOwnedBotLock::acquire(app, bot).await.map_err(|_| E::Unconfirmed)?;
    if app.api_bot_runtime.explicit_retire_quarantined(bot) { return Err(E::Unconfirmed); }
    let maintenance_open = matches!(app.window_held().await, Ok(None));
    let now = crate::db::now();
    let mut tx = crate::db::begin_write(&app.db).await.map_err(|_| E::Unconfirmed)?;
    let valid: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM bots b JOIN projects p ON p.id=b.project_id WHERE b.id=? AND b.kind='claude-api' AND b.deleted_at IS NULL AND p.deleted_at IS NULL AND p.host='local' AND p.handed_off_to IS NULL)")
        .bind(bot).fetch_one(&mut *tx).await.map_err(|_| E::Unconfirmed)?;
    if !valid { return Err(E::Unconfirmed); }
    let conversation: Option<String> = sqlx::query_scalar("SELECT id FROM conversations WHERE bot_id=?").bind(bot).fetch_optional(&mut *tx).await.map_err(|_| E::Unconfirmed)?;
    let conversation = match conversation {
        Some(id) => id,
        None => {
            let id = crate::db::ulid();
            sqlx::query("INSERT INTO conversations(id,bot_id,created_at) VALUES(?,?,?)").bind(&id).bind(bot).bind(&now).execute(&mut *tx).await.map_err(|_| E::Unconfirmed)?;
            id
        }
    };
    let existing: Option<(String,String,Option<String>,Option<String>,String)> = sqlx::query_as("SELECT id,delivery,prompt_text,run_id,status FROM turns WHERE conversation_id=? AND client_request_id=?").bind(&conversation).bind(crid).fetch_optional(&mut *tx).await.map_err(|_| E::Unconfirmed)?;
    if let Some((turn, delivery, prompt_text, old_run, status)) = existing {
        let rows: Vec<(String,String,String)> = sqlx::query_as("SELECT id,content,source FROM messages WHERE turn_id=? AND role='user' LIMIT 2").bind(&turn).fetch_all(&mut *tx).await.map_err(|_| E::Unconfirmed)?;
        if rows.len()!=1 { return Err(E::Unconfirmed); }
        if rows[0].1 != text { return Err(E::Unconfirmed); }
        if prompt_text.as_deref() != Some(text) || rows[0].2 != "web" { return Err(E::Unconfirmed); }
        if delivery != "ok" { return Err(E::Unconfirmed); }
        if status != "completed" { return Err(E::Unconfirmed); }
        let old_run = old_run.ok_or(E::Unconfirmed)?;
        let belongs: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runs WHERE id=? AND bot_id=?)").bind(&old_run).bind(bot).fetch_one(&mut *tx).await.map_err(|_| E::Unconfirmed)?;
        if !belongs { return Err(E::Unconfirmed); }
        let assistants: Vec<(String,i64,Option<String>,String)> = sqlx::query_as("SELECT source,incomplete,rewound_at,conversation_id FROM messages WHERE turn_id=? AND role='assistant' LIMIT 2")
            .bind(&turn).fetch_all(&mut *tx).await.map_err(|_| E::Unconfirmed)?;
        if assistants.len() != 1 || assistants[0].0 != "transcript" || assistants[0].1 != 0 || assistants[0].2.is_some() || assistants[0].3 != conversation { return Err(E::Unconfirmed); }
        let message_id = rows[0].0.clone();
        tx.rollback().await.map_err(|_| E::Unconfirmed)?;
        return Ok(PromptOut { turn_id: turn, message_id, delivery, send_now: None });
    }
    if !maintenance_open { return Err(E::Unconfirmed); }
    let run: Option<String> = sqlx::query_scalar("SELECT r.id FROM runs r JOIN bots b ON b.id=r.bot_id JOIN projects p ON p.id=b.project_id WHERE b.id=? AND b.kind='claude-api' AND b.deleted_at IS NULL AND p.deleted_at IS NULL AND p.host='local' AND p.handed_off_to IS NULL AND r.state='running'")
        .bind(bot).fetch_optional(&mut *tx).await.map_err(|_| E::Unconfirmed)?;
    let run = run.ok_or(E::Unconfirmed)?;
    let turn = crate::db::ulid();
    let message = crate::db::ulid();
    let changed = sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,client_request_id,created_at,prompt_text,auto_resend) SELECT ?,?,?,'web','in_flight','pending',?,?,?,0 WHERE EXISTS(SELECT 1 FROM bots b JOIN projects p ON p.id=b.project_id JOIN runs r ON r.bot_id=b.id JOIN conversations c ON c.bot_id=b.id WHERE b.id=? AND b.kind='claude-api' AND b.deleted_at IS NULL AND p.deleted_at IS NULL AND p.host='local' AND p.handed_off_to IS NULL AND r.id=? AND r.state='running' AND c.id=?)")
        .bind(&turn).bind(&conversation).bind(&run).bind(crid).bind(&now).bind(text).bind(bot).bind(&run).bind(&conversation)
        .execute(&mut *tx).await.map_err(|_| E::Unconfirmed)?;
    if changed.rows_affected()!=1 { return Err(E::Unconfirmed); }
    sqlx::query("INSERT INTO messages(id,conversation_id,turn_id,role,content,source,created_at) VALUES(?,?,?,'user',?,'web',?)")
        .bind(&message).bind(&conversation).bind(&turn).bind(text).bind(&now).execute(&mut *tx).await.map_err(|_| E::Unconfirmed)?;
    if app.shutdown.is_cancelled() { tx.rollback().await.map_err(|_| E::Unconfirmed)?; return Err(E::Unconfirmed); }
    let mut committed = tx.commit().await.is_ok();
    #[cfg(all(test, feature = "daemon-test-harness"))]
    if committed && pending_lost_acks().lock().unwrap().remove(crid) { committed = false; }
    if !committed {
        let _ = sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM turns t JOIN conversations c ON c.id=t.conversation_id JOIN messages m ON m.turn_id=t.id AND m.conversation_id=c.id WHERE t.id=? AND t.run_id=? AND t.client_request_id=? AND c.bot_id=? AND m.id=? AND m.role='user' AND m.content=?)")
            .bind(&turn).bind(&run).bind(crid).bind(bot).bind(&message).bind(text).fetch_one(&app.db).await;
        return Err(E::Unconfirmed);
    }
    #[cfg(all(test, feature = "daemon-test-harness"))]
    crate::lifecycle::race_point::hit("api_native_prompt_after_pending_commit", crid).await;
    if !matches!(app.window_held().await, Ok(None)) || app.shutdown.is_cancelled() { return Err(E::Unconfirmed); }
    let key = ApiTurnKey { bot_id: bot.to_owned(), run_id: run, turn_id: turn.clone() };
    crate::lifecycle::api_bot_port::ApiBotPort::api_turn_spawn_pending(app, &key).await.map_err(|_| E::Unconfirmed)?;
    crate::lifecycle::prompt::publish_prompt_message(app, bot, &message).await;
    Ok(PromptOut { turn_id: turn, message_id: message, delivery: "ok".into(), send_now: None })
}
