//! Joined-completion storage core for API bots. The runtime calls this only after a real join.

use crate::state::App;
use anyhow::Result;
use std::sync::Arc;

use super::adapter::AdapterIdentity;
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

#[derive(Debug)]
pub(super) enum StoreError {
    /// The transaction is known not to have committed (or failed before opening).
    Retryable,
    /// Commit or rollback outcome is not known; callers must prove no commit before retrying.
    Ambiguous,
}

pub(super) struct Committed {
    pub(super) message: Option<Box<crate::db::Message>>,
    pub(super) session_revision: i64,
}

/// Storage result. It intentionally has no Debug or Serialize implementation.
pub(super) enum Outcome {
    Committed(Committed),
    Fenced,
}

/// Commit final text only after the caller has observed genuine RunTicket.join completion.
pub(super) async fn commit_after_join(
    app: &Arc<App>,
    key: &ApiTurnKey,
    identity: &AdapterIdentity,
    text: &FinalText,
) -> Result<Outcome, StoreError> {
    let mut tx = crate::db::begin_write(&app.db)
        .await
        .map_err(|_| StoreError::Retryable)?;
    let stored = store_in_transaction(&mut tx, key, identity, text).await;
    let committed = match stored {
        Ok(Some(committed)) => committed,
        Ok(None) => {
            return match tx.rollback().await {
                Ok(()) => Ok(Outcome::Fenced),
                Err(_) => Err(StoreError::Ambiguous),
            };
        }
        Err(_) => {
            return match tx.rollback().await {
                Ok(()) => Err(StoreError::Retryable),
                Err(_) => Err(StoreError::Ambiguous),
            };
        }
    };

    tx.commit().await.map_err(|_| StoreError::Ambiguous)?;
    Ok(Outcome::Committed(committed))
}

/// Settle a genuine failed/cancelled join while this exact owner is still running.
pub(super) async fn fail_after_join(
    app: &Arc<App>,
    key: &ApiTurnKey,
    identity: &AdapterIdentity,
) -> Result<Outcome, StoreError> {
    let mut tx = crate::db::begin_write(&app.db)
        .await
        .map_err(|_| StoreError::Retryable)?;
    let finished_at = crate::db::now();
    let settled: anyhow::Result<Option<Committed>> = async {
        am_base::db::api_bot::close_reserved_unknown_on(
            &mut tx,
            &key.bot_id,
            &key.run_id,
            &key.turn_id,
            &finished_at,
        )
        .await?;
        let outcome = crate::lifecycle::turn_controller::fail_api_bot_on(
            &mut tx,
            &key.bot_id,
            &key.run_id,
            &key.turn_id,
        )
        .await?;
        if outcome != crate::lifecycle::turn_controller::Outcome::Applied {
            return Ok(None);
        }
        let session_revision = session_revision_on(&mut tx, identity).await?;
        Ok(Some(Committed {
            message: None,
            session_revision,
        }))
    }
    .await;

    let committed = match settled {
        Ok(Some(committed)) => committed,
        Ok(None) => {
            return match tx.rollback().await {
                Ok(()) => Ok(Outcome::Fenced),
                Err(_) => Err(StoreError::Ambiguous),
            };
        }
        Err(_) => {
            return match tx.rollback().await {
                Ok(()) => Err(StoreError::Retryable),
                Err(_) => Err(StoreError::Ambiguous),
            };
        }
    };

    tx.commit().await.map_err(|_| StoreError::Ambiguous)?;
    Ok(Outcome::Committed(committed))
}

/// A commit error is retryable only when a fresh exact read proves this transaction did not
/// complete. Never infer zero cost ownership from the attempt row count.
pub(super) async fn exact_turn_is_uncommitted(app: &Arc<App>, key: &ApiTurnKey) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS(
            SELECT 1
              FROM bots b
              JOIN runs r ON r.bot_id=b.id
              JOIN turns t ON t.run_id=r.id
              JOIN conversations c ON c.id=t.conversation_id
             WHERE b.id=? AND b.kind='claude-api' AND b.deleted_at IS NULL
               AND r.id=? AND r.state='running'
               AND t.id=? AND t.run_id=r.id AND t.status='in_flight'
               AND c.bot_id=b.id
               AND NOT EXISTS (SELECT 1 FROM messages m WHERE m.turn_id=t.id AND m.role='assistant')
        )",
    )
    .bind(&key.bot_id)
    .bind(&key.run_id)
    .bind(&key.turn_id)
    .fetch_one(&app.db)
    .await
    .unwrap_or(false)
}

/// Events are emitted only after commit and after the runtime publishes the returned exact cursor.
pub(super) async fn emit_after_commit(app: &Arc<App>, key: &ApiTurnKey, committed: Committed) {
    if let Some(message) = committed.message {
        crate::lifecycle::emit_message_added(app, &key.bot_id, *message).await;
    }
    crate::lifecycle::emit_turn(app, &key.turn_id).await;
}

async fn store_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    key: &ApiTurnKey,
    identity: &AdapterIdentity,
    text: &FinalText,
) -> Result<Option<Committed>> {
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
    // Zero rows are valid when no reservation remains; owner and turn CAS are independent.
    am_base::db::api_bot::close_reserved_unknown_on(
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

    let message = crate::lifecycle::insert_message_tx(
        tx,
        &conversation_id,
        Some(&key.turn_id),
        "assistant",
        &text.0,
        API_BOT_COMPLETION_SOURCE,
        false,
        None,
    )
    .await?;
    let session_revision = session_revision_on(tx, identity).await?;
    Ok(Some(Committed {
        message: Some(Box::new(message)),
        session_revision,
    }))
}

async fn session_revision_on(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    identity: &AdapterIdentity,
) -> Result<i64> {
    let revision: Option<i64> = sqlx::query_scalar(
        "SELECT revision FROM api_bot_sessions
          WHERE id=? AND bot_id=? AND owner_run_id=? AND owner_turn_id=? AND generation=?
            AND retired_at IS NULL",
    )
    .bind(&identity.session_id)
    .bind(&identity.bot_id)
    .bind(&identity.run_id)
    .bind(&identity.turn_id)
    .bind(identity.generation)
    .fetch_optional(&mut **tx)
    .await?;
    revision.ok_or_else(|| anyhow::anyhow!("API-bot completion has no exact session cursor"))
}
