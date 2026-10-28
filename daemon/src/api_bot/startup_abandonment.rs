//! Restart abandonment is not a join receipt and never replays side effects.
use super::{adapter::AdapterIdentity, runtime::StartupError, startup_recovery::Error};
use std::sync::Arc;

pub(super) async fn abandon_all(app: &Arc<crate::state::App>) -> Result<(), Error> {
    let mut after: Option<(String, String)> = None;
    loop {
        let runs: Vec<(String, String)> = sqlx::query_as("SELECT b.id,r.id FROM runs r JOIN bots b ON b.id=r.bot_id WHERE b.kind='claude-api' AND r.state IN ('starting','running','stopping') AND (? IS NULL OR (b.id,r.id)>(?,?)) ORDER BY b.id,r.id LIMIT 64")
            .bind(after.as_ref().map(|v| &v.0)).bind(after.as_ref().map(|v| &v.0)).bind(after.as_ref().map(|v| &v.1))
            .fetch_all(&app.db).await.map_err(|_| Error::Storage)?;
        if runs.is_empty() {
            break;
        }
        after = runs.last().cloned();
        for (bot, run) in runs {
            // The guard proves startup exclusivity for this bot/run; dummy session/turn are never
            // used as DB evidence, and the absence of a live registry is not join proof.
            let identity = AdapterIdentity {
                bot_id: bot.clone(),
                run_id: run.clone(),
                session_id: String::new(),
                turn_id: String::new(),
                generation: 0,
            };
            let _guard = app
                .api_bot_runtime
                .startup_guard(app, &identity)
                .await
                .map_err(|e| match e {
                    StartupError::Busy => Error::Unresolved,
                    StartupError::NotStartup => Error::NotStartup,
                    StartupError::Storage => Error::Storage,
                })?;
            let mut tx = crate::db::begin_write(&app.db)
                .await
                .map_err(|_| Error::Storage)?;
            let valid: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runs r JOIN bots b ON b.id=r.bot_id WHERE b.id=? AND b.kind='claude-api' AND r.id=? AND r.state IN ('starting','running','stopping') AND NOT EXISTS(SELECT 1 FROM runs x WHERE x.bot_id=b.id AND x.id!=r.id AND x.state IN ('starting','running','stopping'))) AND NOT EXISTS(SELECT 1 FROM api_bot_join_receipts WHERE bot_id=? AND run_id=? AND state='pending') AND NOT EXISTS(SELECT 1 FROM turns t LEFT JOIN conversations c ON c.id=t.conversation_id WHERE t.run_id=? AND t.status IN ('in_flight','queued') AND (c.bot_id IS NOT ? OR EXISTS(SELECT 1 FROM messages m WHERE m.turn_id=t.id AND m.role='assistant'))) ")
            .bind(&bot).bind(&run).bind(&bot).bind(&run).bind(&run).bind(&bot)
            .fetch_one(&mut *tx).await.map_err(|_| Error::Storage)?;
            if !valid {
                return Err(Error::Unresolved);
            }
            let now = crate::db::now();
            am_base::db::api_bot::abandon_run_reserved_on(&mut tx, &bot, &run, &now)
                .await
                .map_err(|_| Error::Storage)?;
            crate::lifecycle::turn_controller::abandon_api_run_turns_on(&mut tx, &bot, &run)
                .await
                .map_err(|_| Error::Storage)?;
            let unsettled: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM turns WHERE run_id=? AND status IN ('in_flight','queued')) OR EXISTS(SELECT 1 FROM api_bot_attempts WHERE bot_id=? AND run_id=? AND state='reserved')")
                .bind(&run).bind(&bot).bind(&run).fetch_one(&mut *tx).await.map_err(|_| Error::Storage)?;
            if unsettled {
                return Err(Error::Unresolved);
            }
            let changed = sqlx::query("UPDATE runs SET state='exited', ended_at=COALESCE(ended_at,?), exit_reason='api_restart_abandoned' WHERE id=? AND bot_id=? AND state IN ('starting','running','stopping')")
            .bind(&now).bind(&run).bind(&bot).execute(&mut *tx).await.map_err(|_| Error::Storage)?.rows_affected();
            if changed != 1 {
                return Err(Error::Unresolved);
            }
            tx.commit().await.map_err(|_| Error::Storage)?;
        }
    }
    // A keyset page becoming empty does not prove that no earlier key appeared.
    let remains: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runs r JOIN bots b ON b.id=r.bot_id WHERE b.kind='claude-api' AND r.state IN ('starting','running','stopping'))")
        .fetch_one(&app.db).await.map_err(|_| Error::Storage)?;
    if remains {
        return Err(Error::Unresolved);
    }
    Ok(())
}
