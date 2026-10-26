//! Inert joined-completion storage core for API bots.
//!
//! This module has no production caller. A future caller may deliver final text only after a
//! genuine successful RunTicket.join; this storage core does not accept a joined boolean or add a
//! port to encode that lifecycle evidence.

use crate::state::App;
use anyhow::{anyhow, Result};
use std::sync::Arc;

use super::protocol::Response;
use crate::lifecycle::api_bot_port::ApiTurnKey;

const API_BOT_COMPLETION_SOURCE: &str = "transcript";

/// Final user-visible text. It intentionally has no Debug or Serialize implementation.
pub(super) struct FinalText(String);

impl FinalText {
    pub(super) fn from_response(response: &Response) -> Self {
        Self(super::runner::final_text(response))
    }
}

/// Storage result. It intentionally has no Debug or Serialize implementation.
pub(super) enum Outcome {
    Committed,
    Fenced,
}

/// Store a completion only when the caller has already observed a genuine successful join.
///
/// There is deliberately no production call site in this change. Callers must pass only the
/// final response after RunTicket.join, never from the runner task before its JoinHandle exits.
pub(super) async fn commit_after_join(
    app: &Arc<App>,
    key: &ApiTurnKey,
    text: FinalText,
) -> Result<Outcome> {
    let mut tx = crate::db::begin_write(&app.db).await?;
    let stored = store_in_transaction(&mut tx, key, &text).await;
    let message = match stored {
        Ok(Some(message)) => message,
        Ok(None) => {
            tx.rollback().await?;
            return Ok(Outcome::Fenced);
        }
        Err(error) => {
            let rollback = tx.rollback().await;
            return match rollback {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(anyhow!(
                    "API-bot completion failed ({error}) and transaction rollback failed ({rollback_error})"
                )),
            };
        }
    };

    tx.commit().await?;
    crate::lifecycle::emit_message_added(app, &key.bot_id, message).await;
    crate::lifecycle::emit_turn(app, &key.turn_id).await;
    Ok(Outcome::Committed)
}

async fn store_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    key: &ApiTurnKey,
    text: &FinalText,
) -> Result<Option<crate::db::Message>> {
    let conversation_id = sqlx::query_scalar::<_, String>(
        "SELECT c.id
           FROM bots b
           JOIN runs r ON r.bot_id=b.id
           JOIN turns t ON t.run_id=r.id
           JOIN conversations c ON c.id=t.conversation_id
          WHERE b.id=? AND b.kind='claude-api' AND b.deleted_at IS NULL
            AND r.id=? AND r.state='running'
            AND t.id=? AND t.run_id=r.id AND t.status='in_flight'
            AND c.bot_id=b.id",
    )
    .bind(&key.bot_id)
    .bind(&key.run_id)
    .bind(&key.turn_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(conversation_id) = conversation_id else {
        return Ok(None);
    };

    let finished_at = crate::db::now();
    // Zero rows are valid when no reservation remains; the exact owner query above and the turn
    // CAS below independently establish ownership and completion.
    let _closed_attempts = am_base::db::api_bot::close_reserved_unknown_on(
        tx,
        &key.bot_id,
        &key.run_id,
        &key.turn_id,
        &finished_at,
    )
    .await?;

    let outcome = crate::lifecycle::turn_controller::complete_api_bot_on(
        tx,
        &key.bot_id,
        &key.run_id,
        &key.turn_id,
    )
    .await?;
    if outcome != crate::lifecycle::turn_controller::Outcome::Applied {
        return Ok(None);
    }

    Ok(Some(
        crate::lifecycle::insert_message_tx(
            tx,
            &conversation_id,
            Some(&key.turn_id),
            "assistant",
            &text.0,
            API_BOT_COMPLETION_SOURCE,
            false,
            None,
        )
        .await?,
    ))
}
