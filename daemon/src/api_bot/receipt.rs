//! Private storage contract only. No production caller or proof inference from checkpoint state.
//! D2 must supply these values only after observing a genuine RunTicket join.
use super::{adapter::AdapterIdentity, protocol::Response};
use sqlx::{Row, SqlitePool};

pub(super) const MAX_TEXT_BYTES: usize = 1024 * 1024;
pub(super) const MAX_BATCH: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum JoinOutcome {
    Completed,
    Failed,
    Cancelled,
}
impl JoinOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Only visible text can enter through the typed constructor; never an opaque response.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct Payload {
    outcome: JoinOutcome,
    text: Option<String>,
}
impl std::fmt::Debug for Payload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Payload")
            .field("outcome", &self.outcome)
            .field("text", &"[redacted]")
            .finish()
    }
}
impl Payload {
    pub(super) fn completed(response: &Response) -> Result<Self, Error> {
        let text = super::runner::final_text(response);
        if text.len() > MAX_TEXT_BYTES {
            return Err(Error::Bounds);
        }
        Ok(Self {
            outcome: JoinOutcome::Completed,
            text: Some(text),
        })
    }
    pub(super) fn ended(outcome: JoinOutcome) -> Result<Self, Error> {
        if outcome == JoinOutcome::Completed {
            return Err(Error::Invalid);
        }
        Ok(Self {
            outcome,
            text: None,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Error {
    Bounds,
    Invalid,
    Conflict,
    Storage,
}
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Saved {
    Inserted,
    Existing,
    Fenced,
}

/// No Debug/Serialize: identity and private final text stay outside generic projections.
pub(super) struct Pending {
    pub(super) identity: AdapterIdentity,
    pub(super) revision: i64,
    pub(super) payload: Payload,
}

/// Atomically validate the full live owner before insertion or an idempotent retry.
/// Errors contain no database error strings, private text or paths.
pub(super) async fn save(
    pool: &SqlitePool,
    identity: &AdapterIdentity,
    revision: i64,
    payload: &Payload,
) -> Result<Saved, Error> {
    if identity.generation <= 0 || revision < 0 {
        return Err(Error::Invalid);
    }
    let mut tx = crate::db::begin_write(pool)
        .await
        .map_err(|_| Error::Storage)?;
    let now = crate::db::now();
    let expiry = crate::db::ts_sql("s.expires_at");
    let current: bool = sqlx::query_scalar(&format!(
        "SELECT EXISTS(
        SELECT 1 FROM api_bot_sessions s
        JOIN bots b ON b.id=s.bot_id
        JOIN runs r ON r.id=s.owner_run_id AND r.bot_id=b.id
        JOIN turns t ON t.id=s.owner_turn_id AND t.run_id=r.id
        JOIN conversations c ON c.id=t.conversation_id AND c.bot_id=b.id
        WHERE s.id=? AND b.id=? AND r.id=? AND t.id=?
          AND s.generation=? AND s.revision=? AND s.retired_at IS NULL AND {expiry}>?
          AND b.kind='claude-api' AND b.deleted_at IS NULL
          AND r.state='running' AND t.status='in_flight' AND t.delivery='ok'
          AND NOT EXISTS(SELECT 1 FROM runs successor WHERE successor.bot_id=b.id
              AND successor.id!=r.id AND successor.state IN ('running','starting','stopping'))
    )",
    ))
    .bind(&identity.session_id)
    .bind(&identity.bot_id)
    .bind(&identity.run_id)
    .bind(&identity.turn_id)
    .bind(identity.generation)
    .bind(revision)
    .bind(&now)
    .fetch_one(&mut *tx)
    .await
    .map_err(|_| Error::Storage)?;
    if !current {
        tx.rollback().await.map_err(|_| Error::Storage)?;
        return Ok(Saved::Fenced);
    }
    let old = sqlx::query("SELECT bot_id,run_id,generation,revision,outcome,final_text FROM api_bot_join_receipts WHERE session_id=? AND turn_id=?")
        .bind(&identity.session_id).bind(&identity.turn_id).fetch_optional(&mut *tx).await.map_err(|_| Error::Storage)?;
    if let Some(row) = old {
        let same = row.get::<String, _>("bot_id") == identity.bot_id
            && row.get::<String, _>("run_id") == identity.run_id
            && row.get::<i64, _>("generation") == identity.generation
            && row.get::<i64, _>("revision") == revision
            && row.get::<String, _>("outcome") == payload.outcome.as_str()
            && row.get::<Option<String>, _>("final_text") == payload.text;
        tx.rollback().await.map_err(|_| Error::Storage)?;
        return if same {
            Ok(Saved::Existing)
        } else {
            Err(Error::Conflict)
        };
    }
    sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,final_text,created_at) VALUES(?,?,?,?,?,?,?,?,?)")
        .bind(&identity.session_id).bind(&identity.bot_id).bind(&identity.run_id).bind(&identity.turn_id)
        .bind(identity.generation).bind(revision).bind(payload.outcome.as_str()).bind(&payload.text).bind(now)
        .execute(&mut *tx).await.map_err(|_| Error::Storage)?;
    tx.commit().await.map_err(|_| Error::Storage)?;
    Ok(Saved::Inserted)
}

/// Read at most 64 private receipts; processing requires a separate exact-owner fence in D2/D3.
pub(super) async fn pending(pool: &SqlitePool, limit: usize) -> Result<Vec<Pending>, Error> {
    if limit == 0 || limit > MAX_BATCH {
        return Err(Error::Bounds);
    }
    let rows = sqlx::query("SELECT session_id,bot_id,run_id,turn_id,generation,revision,outcome,final_text FROM api_bot_join_receipts WHERE state='pending' ORDER BY created_at,session_id,turn_id LIMIT ?")
        .bind(limit as i64).fetch_all(pool).await.map_err(|_| Error::Storage)?;
    rows.into_iter()
        .map(|row| {
            let outcome = match row.get::<String, _>("outcome").as_str() {
                "completed" => JoinOutcome::Completed,
                "failed" => JoinOutcome::Failed,
                "cancelled" => JoinOutcome::Cancelled,
                _ => return Err(Error::Invalid),
            };
            Ok(Pending {
                identity: AdapterIdentity {
                    session_id: row.get("session_id"),
                    bot_id: row.get("bot_id"),
                    run_id: row.get("run_id"),
                    turn_id: row.get("turn_id"),
                    generation: row.get("generation"),
                },
                revision: row.get("revision"),
                payload: Payload {
                    outcome,
                    text: row.get("final_text"),
                },
            })
        })
        .collect()
}

#[derive(Clone, Copy)]
pub(super) enum Disposition {
    Applied,
    Fenced,
}
/// Used inside a future caller's completion transaction, never as proof that completion happened.
pub(super) async fn mark_on(
    tx: &mut sqlx::SqliteConnection,
    item: &Pending,
    disposition: Disposition,
) -> Result<bool, Error> {
    let state = match disposition {
        Disposition::Applied => "applied",
        Disposition::Fenced => "fenced",
    };
    let changed = sqlx::query("UPDATE api_bot_join_receipts SET state=?,processed_at=? WHERE session_id=? AND bot_id=? AND run_id=? AND turn_id=? AND generation=? AND revision=? AND outcome=? AND final_text IS ? AND state='pending'")
        .bind(state).bind(crate::db::now()).bind(&item.identity.session_id).bind(&item.identity.bot_id)
        .bind(&item.identity.run_id).bind(&item.identity.turn_id).bind(item.identity.generation)
        .bind(item.revision).bind(item.payload.outcome.as_str()).bind(&item.payload.text)
        .execute(tx).await.map_err(|_| Error::Storage)?.rows_affected();
    Ok(changed == 1)
}
