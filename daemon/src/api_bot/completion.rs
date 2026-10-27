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
    pub(super) fn receipt_payload(&self) -> Result<super::receipt::Payload, super::receipt::Error> {
        super::receipt::Payload::visible(&self.0)
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
    let settled = fail_in_transaction(&mut tx, key, identity).await;

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

async fn fail_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    key: &ApiTurnKey,
    identity: &AdapterIdentity,
) -> Result<Option<Committed>> {
    am_base::db::api_bot::close_reserved_unknown_on(
        tx,
        &key.bot_id,
        &key.run_id,
        &key.turn_id,
        &crate::db::now(),
    )
    .await?;
    if crate::lifecycle::turn_controller::fail_api_bot_on(
        tx,
        &key.bot_id,
        &key.run_id,
        &key.turn_id,
    )
    .await?
        != crate::lifecycle::turn_controller::Outcome::Applied
    {
        return Ok(None);
    }
    Ok(Some(Committed {
        message: None,
        session_revision: session_revision_on(tx, identity).await?,
    }))
}

/// Called only after true join; pending receipt must be durable before settlement starts.
pub(super) async fn store_durable_after_join(
    app: &Arc<App>,
    key: &ApiTurnKey,
    identity: &AdapterIdentity,
    payload: &super::receipt::Payload,
) -> Result<Outcome, StoreError> {
    use super::receipt::{self, Pending, Saved, State};
    if key.bot_id != identity.bot_id
        || key.run_id != identity.run_id
        || key.turn_id != identity.turn_id
    {
        return Ok(Outcome::Fenced);
    }
    let item = if let Some(stored) = receipt::load(&app.db, identity)
        .await
        .map_err(|_| StoreError::Retryable)?
    {
        if stored.item.payload != *payload {
            return Err(StoreError::Ambiguous);
        }
        match stored.state {
            State::Fenced => return Ok(Outcome::Fenced),
            State::Applied => {
                return match probe_after_join(app, identity, payload).await? {
                    Probe::Applied(c) => Ok(Outcome::Committed(c)),
                    _ => Err(StoreError::Ambiguous),
                }
            }
            State::Pending => stored.item,
        }
    } else {
        let revision = exact_revision(&app.db, identity)
            .await?
            .ok_or(StoreError::Retryable)?;
        let item = Pending {
            identity: identity.clone(),
            revision,
            payload: payload.clone(),
        };
        match receipt::save(&app.db, identity, revision, payload)
            .await
            .map_err(|_| StoreError::Ambiguous)?
        {
            Saved::Fenced => return Ok(Outcome::Fenced),
            Saved::Inserted | Saved::Existing => item,
        }
    };
    consume_receipt(app, key, &item).await
}

/// No side effect can precede both the exact receipt and live-owner checks.
pub(super) async fn consume_receipt(
    app: &Arc<App>,
    key: &ApiTurnKey,
    item: &super::receipt::Pending,
) -> Result<Outcome, StoreError> {
    use super::receipt::{self, Disposition, JoinOutcome, State};
    if key.bot_id != item.identity.bot_id
        || key.run_id != item.identity.run_id
        || key.turn_id != item.identity.turn_id
    {
        return Ok(Outcome::Fenced);
    }
    let mut tx = crate::db::begin_write(&app.db)
        .await
        .map_err(|_| StoreError::Retryable)?;
    if !receipt::matches_on(&mut tx, item, State::Pending)
        .await
        .map_err(|_| StoreError::Retryable)?
    {
        tx.rollback().await.map_err(|_| StoreError::Ambiguous)?;
        return Err(StoreError::Ambiguous);
    }
    if !receipt::owner_on(&mut tx, &item.identity, item.revision, "in_flight")
        .await
        .map_err(|_| StoreError::Retryable)?
    {
        tx.rollback().await.map_err(|_| StoreError::Ambiguous)?;
        receipt::fence_stale(&app.db, item)
            .await
            .map_err(|_| StoreError::Retryable)?;
        return Ok(Outcome::Fenced);
    }
    // A prior assistant is a conflict even if another writer left the turn in flight.
    // Check inside the write transaction so a retry cannot create a second assistant.
    if assistant_count(&mut tx, &key.turn_id).await? != 0 {
        tx.rollback().await.map_err(|_| StoreError::Ambiguous)?;
        return Err(StoreError::Ambiguous);
    }
    let stored: Result<Option<Committed>> = async {
        let committed = if item.payload.outcome() == JoinOutcome::Completed {
            let text = FinalText(
                item.payload
                    .text()
                    .ok_or_else(|| anyhow::anyhow!("invalid receipt payload"))?
                    .to_owned(),
            );
            store_in_transaction(&mut tx, key, &item.identity, &text).await?
        } else {
            fail_in_transaction(&mut tx, key, &item.identity).await?
        };
        let Some(committed) = committed else {
            anyhow::bail!("joined receipt owner CAS missed");
        };
        if !receipt::mark_on(&mut tx, item, Disposition::Applied)
            .await
            .map_err(|_| anyhow::anyhow!("receipt CAS failed"))?
        {
            anyhow::bail!("joined receipt application CAS missed");
        }
        Ok(Some(committed))
    }
    .await;
    match stored {
        Ok(Some(committed)) => {
            tx.commit().await.map_err(|_| StoreError::Ambiguous)?;
            Ok(Outcome::Committed(committed))
        }
        _ => match tx.rollback().await {
            Ok(()) => Err(StoreError::Retryable),
            Err(_) => Err(StoreError::Ambiguous),
        },
    }
}

async fn exact_revision(
    pool: &sqlx::SqlitePool,
    identity: &AdapterIdentity,
) -> Result<Option<i64>, StoreError> {
    sqlx::query_scalar("SELECT revision FROM api_bot_sessions WHERE id=? AND bot_id=? AND owner_run_id=? AND owner_turn_id=? AND generation=? AND retired_at IS NULL")
        .bind(&identity.session_id).bind(&identity.bot_id).bind(&identity.run_id).bind(&identity.turn_id).bind(identity.generation)
        .fetch_optional(pool).await.map_err(|_| StoreError::Retryable)
}

/// Read-only evidence; no Debug/Serialize for the private receipt/message values.
pub(super) enum Probe {
    Applied(Committed),
    Pending,
    Stale(super::receipt::Pending),
    Unconfirmed,
}

pub(super) async fn probe_after_join(
    app: &Arc<App>,
    identity: &AdapterIdentity,
    payload: &super::receipt::Payload,
) -> Result<Probe, StoreError> {
    use super::receipt::{self, JoinOutcome, Pending, State};
    let stored = receipt::load(&app.db, identity)
        .await
        .map_err(|_| StoreError::Retryable)?;
    let mut tx = app.db.begin().await.map_err(|_| StoreError::Retryable)?;
    let Some(stored) = stored else {
        // A missing receipt alone is never proof. Check the exact live owner and zero assistants.
        let revision: Option<i64> = sqlx::query_scalar("SELECT revision FROM api_bot_sessions WHERE id=? AND bot_id=? AND owner_run_id=? AND owner_turn_id=? AND generation=? AND retired_at IS NULL")
            .bind(&identity.session_id).bind(&identity.bot_id).bind(&identity.run_id).bind(&identity.turn_id).bind(identity.generation)
            .fetch_optional(&mut *tx).await.map_err(|_| StoreError::Retryable)?;
        let Some(revision) = revision else {
            return Ok(Probe::Unconfirmed);
        };
        let item = Pending {
            identity: identity.clone(),
            revision,
            payload: payload.clone(),
        };
        let absent: bool = sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM api_bot_join_receipts WHERE session_id=? AND turn_id=?)")
            .bind(&identity.session_id).bind(&identity.turn_id).fetch_one(&mut *tx).await.map_err(|_| StoreError::Retryable)?;
        let empty = assistant_count(&mut tx, &identity.turn_id).await? == 0;
        return if absent
            && empty
            && receipt::owner_on(&mut tx, identity, item.revision, "in_flight")
                .await
                .map_err(|_| StoreError::Retryable)?
        {
            Ok(Probe::Pending)
        } else {
            Ok(Probe::Unconfirmed)
        };
    };
    let item = stored.item;
    if item.payload != *payload {
        return Ok(Probe::Unconfirmed);
    }
    match stored.state {
        State::Fenced => Ok(Probe::Unconfirmed),
        State::Pending => {
            if !receipt::matches_on(&mut tx, &item, State::Pending)
                .await
                .map_err(|_| StoreError::Retryable)?
            {
                return Ok(Probe::Unconfirmed);
            }
            if !receipt::owner_on(&mut tx, identity, item.revision, "in_flight")
                .await
                .map_err(|_| StoreError::Retryable)?
            {
                return Ok(Probe::Stale(item));
            }
            if assistant_count(&mut tx, &identity.turn_id).await? == 0 {
                Ok(Probe::Pending)
            } else {
                Ok(Probe::Unconfirmed)
            }
        }
        State::Applied => {
            let status = if payload.outcome() == JoinOutcome::Completed {
                "completed"
            } else {
                "failed"
            };
            if !receipt::matches_on(&mut tx, &item, State::Applied)
                .await
                .map_err(|_| StoreError::Retryable)?
                || !receipt::owner_on(&mut tx, identity, item.revision, status)
                    .await
                    .map_err(|_| StoreError::Retryable)?
            {
                return Ok(Probe::Unconfirmed);
            }
            let count = assistant_count(&mut tx, &identity.turn_id).await?;
            let message = if payload.outcome() == JoinOutcome::Completed {
                if count != 1 {
                    return Ok(Probe::Unconfirmed);
                }
                let message: Option<crate::db::Message> = sqlx::query_as("SELECT m.*,m.rowid AS seq FROM messages m JOIN turns t ON t.id=m.turn_id JOIN conversations c ON c.id=t.conversation_id WHERE t.id=? AND m.conversation_id=c.id AND c.bot_id=? AND m.role='assistant' AND m.source='transcript' AND m.incomplete=0 AND m.rewound_at IS NULL AND m.content=?")
                    .bind(&identity.turn_id).bind(&identity.bot_id).bind(payload.text()).fetch_optional(&mut *tx).await.map_err(|_| StoreError::Retryable)?;
                let Some(message) = message else {
                    return Ok(Probe::Unconfirmed);
                };
                Some(Box::new(message))
            } else {
                if count != 0 {
                    return Ok(Probe::Unconfirmed);
                }
                None
            };
            Ok(Probe::Applied(Committed {
                message,
                session_revision: item.revision,
            }))
        }
    }
}

async fn assistant_count(conn: &mut sqlx::SqliteConnection, turn: &str) -> Result<i64, StoreError> {
    sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'")
        .bind(turn)
        .fetch_one(conn)
        .await
        .map_err(|_| StoreError::Retryable)
}
