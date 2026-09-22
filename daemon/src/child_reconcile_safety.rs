//! Durable guards for child retirement during explicit restore/restart workflows.

use anyhow::Result;
use sqlx::SqlitePool;

pub const RESTORE_GRACE_SECS: i64 = 10 * 60;

async fn note(pool: &SqlitePool, bot_id: &str, kind: &str, body: &str) -> Result<()> {
    sqlx::query("INSERT INTO supervisor_notes (id, supervisor_id, kind, body, version, created_at) VALUES (?,?,?,?,1,?)")
        .bind(crate::db::ulid())
        .bind(bot_id)
        .bind(kind)
        .bind(body)
        .bind(crate::db::now())
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn hold_after_name_taken(pool: &SqlitePool, bot_id: &str, agent: &str) -> Result<()> {
    note(
        pool,
        bot_id,
        "child_retirement_hold",
        &format!("agent_name_taken: herdr refused to start `{agent}`; ownership is uncertain"),
    )
    .await
}

pub async fn clear_after_successful_restart(pool: &SqlitePool, bot_id: &str) -> Result<()> {
    note(
        pool,
        bot_id,
        "child_retirement_hold",
        "cleared: a later in-pane restart succeeded",
    )
    .await
}

pub async fn record_retirement_grace(pool: &SqlitePool, bot_id: &str) -> Result<String> {
    let until = crate::db::iso_in(RESTORE_GRACE_SECS);
    note(pool, bot_id, "child_retirement_grace", &until).await?;
    Ok(until)
}

pub async fn clear_retirement_grace(pool: &SqlitePool, bot_id: &str) -> Result<()> {
    note(pool, bot_id, "child_retirement_grace", &crate::db::now()).await
}

/// A `None` result means normal retirement rules may proceed. Read errors are returned so callers
/// can fail closed and defer reconciliation.
pub async fn retirement_block(pool: &SqlitePool, bot_id: &str) -> Result<Option<String>> {
    let hold: Option<String> = sqlx::query_scalar(
        "SELECT body FROM supervisor_notes WHERE supervisor_id=? AND kind='child_retirement_hold' ORDER BY created_at DESC, rowid DESC LIMIT 1",
    )
    .bind(bot_id)
    .fetch_optional(pool)
    .await?;
    if let Some(reason) = hold.filter(|s| !s.starts_with("cleared:")) {
        return Ok(Some(reason));
    }

    let grace: Option<String> = sqlx::query_scalar(
        "SELECT body FROM supervisor_notes WHERE supervisor_id=? AND kind='child_retirement_grace' ORDER BY created_at DESC, rowid DESC LIMIT 1",
    )
    .bind(bot_id)
    .fetch_optional(pool)
    .await?;
    if let Some(until) = grace {
        if crate::db::cmp_ts(&until, &crate::db::now()).is_gt() {
            return Ok(Some(format!(
                "child retirement grace period ends at {until}"
            )));
        }
    }

    Ok(None)
}
