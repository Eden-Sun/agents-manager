//! Private API backend repository. No runtime, key access, HTTP, export or sharing entry points.
//! Callers own the daemon lifecycle; every mutation here fences bot/run/turn/generation/revision.
use anyhow::{ensure, Context, Result};
use serde_json::Value;
use sqlx::{FromRow, SqliteConnection, SqlitePool};

pub const RETENTION_SECS: i64 = 7 * 24 * 3600;
pub const MAX_CHECKPOINT_BYTES: usize = 8 * 1024 * 1024;

/// Deliberately no derived Debug/Serialize: a checkpoint contains full private conversation data.
pub struct Checkpoint(String);
impl std::fmt::Debug for Checkpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Checkpoint(<private>)")
    }
}
impl Checkpoint {
    pub fn new(value: &Value) -> Result<Self> {
        let object = value.as_object().context("checkpoint must be an object")?;
        ensure!(
            object.keys().all(|k| matches!(
                k.as_str(),
                "messages" | "responses" | "api_pending" | "tool_pending" | "completed_tool_ids"
            )),
            "unsupported checkpoint field"
        );
        ensure!(
            value["messages"].is_array()
                && value["responses"].is_array()
                && value["api_pending"].is_boolean()
                && (value["tool_pending"].is_null() || value["tool_pending"].is_string())
                && value["completed_tool_ids"].is_array(),
            "invalid checkpoint shape"
        );
        let encoded = serde_json::to_string(value)?;
        ensure!(
            encoded.len() <= MAX_CHECKPOINT_BYTES,
            "checkpoint size limit exceeded"
        );
        Ok(Self(encoded))
    }
    pub fn value(&self) -> Value {
        serde_json::from_str(&self.0).expect("validated checkpoint")
    }
    fn api_pending(&self) -> bool {
        self.value()["api_pending"] == true
    }
    fn pending(&self) -> bool {
        let v = self.value();
        v["api_pending"] == true || !v["tool_pending"].is_null()
    }
}
#[derive(Clone, Debug)]
pub struct Fence<'a> {
    pub session: &'a str,
    pub bot: &'a str,
    pub run: &'a str,
    pub turn: &'a str,
    pub generation: i64,
    pub revision: i64,
}
#[derive(FromRow)]
struct Stored {
    owner_run_id: Option<String>,
    owner_turn_id: Option<String>,
    generation: i64,
    revision: i64,
    checkpoint_json: String,
}

async fn owner(conn: &mut SqliteConnection, f: &Fence<'_>) -> Result<()> {
    let valid: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM bots b JOIN runs r ON r.bot_id=b.id JOIN turns t ON t.run_id=r.id JOIN conversations c ON c.id=t.conversation_id WHERE b.id=? AND b.kind='claude-api' AND b.deleted_at IS NULL AND r.id=? AND r.state='running' AND t.id=? AND t.status='in_flight' AND c.bot_id=b.id)")
        .bind(f.bot).bind(f.run).bind(f.turn).fetch_one(conn).await?;
    ensure!(
        valid && f.generation > 0 && f.revision >= 0,
        "stale API bot owner"
    );
    Ok(())
}
async fn current(conn: &mut SqliteConnection, f: &Fence<'_>, at: &str) -> Result<Stored> {
    owner(conn, f).await?;
    let row: Stored = sqlx::query_as("SELECT owner_run_id,owner_turn_id,generation,revision,checkpoint_json FROM api_bot_sessions WHERE id=? AND bot_id=? AND retired_at IS NULL AND expires_at>?")
        .bind(f.session).bind(f.bot).bind(at).fetch_optional(conn).await?.context("API bot session unavailable")?;
    ensure!(
        row.owner_run_id.as_deref() == Some(f.run)
            && row.owner_turn_id.as_deref() == Some(f.turn)
            && row.generation == f.generation,
        "stale API bot session fence"
    );
    Ok(row)
}
async fn cas(
    conn: &mut SqliteConnection,
    f: &Fence<'_>,
    checkpoint: &Checkpoint,
    at: &str,
) -> Result<i64> {
    let next = f
        .revision
        .checked_add(1)
        .context("checkpoint revision overflow")?;
    let changed=sqlx::query("UPDATE api_bot_sessions SET checkpoint_json=?,revision=?,updated_at=?,expires_at=? WHERE id=? AND bot_id=? AND owner_run_id=? AND owner_turn_id=? AND generation=? AND revision=? AND retired_at IS NULL AND expires_at>?")
        .bind(&checkpoint.0).bind(next).bind(at).bind(super::iso_in(RETENTION_SECS)).bind(f.session).bind(f.bot).bind(f.run).bind(f.turn).bind(f.generation).bind(f.revision).bind(at).execute(conn).await?.rows_affected();
    ensure!(changed == 1, "stale checkpoint revision");
    Ok(next)
}

pub async fn create(pool: &SqlitePool, f: &Fence<'_>, checkpoint: &Checkpoint) -> Result<()> {
    ensure!(
        f.revision == 0 && !checkpoint.pending(),
        "initial checkpoint must be settled at revision zero"
    );
    let mut tx = super::begin_write(pool).await?;
    owner(&mut tx, f).await?;
    let at = super::now();
    sqlx::query("INSERT INTO api_bot_sessions(id,bot_id,owner_run_id,owner_turn_id,generation,checkpoint_json,created_at,updated_at,expires_at) VALUES(?,?,?,?,?,?,?,?,?)")
        .bind(f.session).bind(f.bot).bind(f.run).bind(f.turn).bind(f.generation).bind(&checkpoint.0).bind(&at).bind(&at).bind(super::iso_in(RETENTION_SECS)).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
/// Never query this table from generic state/message/export/share projections.
pub async fn load(pool: &SqlitePool, f: &Fence<'_>) -> Result<Checkpoint> {
    let mut tx = super::begin_write(pool).await?;
    let row = current(&mut tx, f, &super::now()).await?;
    tx.commit().await?;
    ensure!(row.revision == f.revision, "stale checkpoint revision");
    Checkpoint::new(
        &serde_json::from_str(&row.checkpoint_json).context("invalid stored checkpoint")?,
    )
}
pub async fn save(pool: &SqlitePool, f: &Fence<'_>, checkpoint: &Checkpoint) -> Result<i64> {
    let mut tx = super::begin_write(pool).await?;
    let at = super::now();
    let row = current(&mut tx, f, &at).await?;
    let before = Checkpoint::new(&serde_json::from_str(&row.checkpoint_json)?)?;
    ensure!(
        before.api_pending() == checkpoint.api_pending(),
        "HTTP marker changes require attempt accounting"
    );
    let revision = cas(&mut tx, f, checkpoint, &at).await?;
    tx.commit().await?;
    Ok(revision)
}

#[derive(Clone, Debug)]
pub struct Attempt<'a> {
    pub id: &'a str,
    pub ordinal: i64,
    pub request_hash: &'a str,
    pub speed: &'a str,
    pub reserved_microusd: i64,
    pub pricing_version: &'a str,
}
#[derive(FromRow)]
struct Existing {
    session_id: Option<String>,
    run_id: Option<String>,
    turn_id: Option<String>,
    generation: i64,
    ordinal: i64,
    request_hash: String,
    requested_speed: String,
    reserved_microusd: i64,
    pricing_version: String,
    state: String,
    actual_speed: Option<String>,
    usage_json: Option<String>,
    estimated_microusd: Option<i64>,
}
async fn attempt(conn: &mut SqliteConnection, id: &str) -> Result<Option<Existing>> {
    Ok(sqlx::query_as("SELECT session_id,run_id,turn_id,generation,ordinal,request_hash,requested_speed,reserved_microusd,pricing_version,state,actual_speed,usage_json,estimated_microusd FROM api_bot_attempts WHERE id=?").bind(id).fetch_optional(conn).await?)
}
fn same_attempt(row: &Existing, f: &Fence<'_>, a: &Attempt<'_>) -> bool {
    row.session_id.as_deref() == Some(f.session)
        && row.run_id.as_deref() == Some(f.run)
        && row.turn_id.as_deref() == Some(f.turn)
        && row.generation == f.generation
        && row.ordinal == a.ordinal
        && row.request_hash == a.request_hash
        && row.requested_speed == a.speed
        && row.reserved_microusd == a.reserved_microusd
        && row.pricing_version == a.pricing_version
}
#[derive(Debug, PartialEq, Eq)]
pub enum Reservation {
    Fresh(i64),
    Existing { revision: i64, state: String },
}
/// An existing attempt is NEVER permission to send again, including failed/unknown attempts.
pub async fn reserve(
    pool: &SqlitePool,
    f: &Fence<'_>,
    a: &Attempt<'_>,
    checkpoint: &Checkpoint,
    max_turn_microusd: i64,
    max_per_hour: i64,
) -> Result<Reservation> {
    ensure!(
        !a.id.is_empty()
            && a.ordinal >= 0
            && a.request_hash.len() == 64
            && a.request_hash.bytes().all(|b| b.is_ascii_hexdigit())
            && matches!(a.speed, "fast" | "standard")
            && a.reserved_microusd > 0
            && !a.pricing_version.is_empty()
            && max_turn_microusd > 0
            && max_per_hour > 0,
        "invalid attempt limits"
    );
    ensure!(
        checkpoint.api_pending() && checkpoint.value()["tool_pending"].is_null(),
        "reservation requires HTTP-only pending checkpoint"
    );
    let mut tx = super::begin_write(pool).await?;
    let at = super::now();
    let row = current(&mut tx, f, &at).await?;
    if let Some(existing) = attempt(&mut tx, a.id).await? {
        ensure!(same_attempt(&existing, f, a), "attempt id conflict");
        tx.commit().await?;
        return Ok(Reservation::Existing {
            revision: row.revision,
            state: existing.state,
        });
    }
    let before = Checkpoint::new(&serde_json::from_str(&row.checkpoint_json)?)?;
    ensure!(!before.pending(), "unresolved checkpoint cannot send again");
    let spent:i64=sqlx::query_scalar("SELECT COALESCE(SUM(COALESCE(estimated_microusd,reserved_microusd)),0) FROM api_bot_attempts WHERE turn_id=?").bind(f.turn).fetch_one(&mut *tx).await?;
    ensure!(
        spent
            .checked_add(a.reserved_microusd)
            .is_some_and(|n| n <= max_turn_microusd),
        "API bot turn budget exhausted"
    );
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM api_bot_attempts WHERE created_at>=?")
            .bind(super::iso_in(-3600))
            .fetch_one(&mut *tx)
            .await?;
    ensure!(count < max_per_hour, "API bot request rate exhausted");
    let revision = cas(&mut tx, f, checkpoint, &at).await?;
    sqlx::query("INSERT INTO api_bot_attempts(id,session_id,bot_id,run_id,turn_id,generation,ordinal,request_hash,requested_speed,state,reserved_microusd,pricing_version,created_at) VALUES(?,?,?,?,?,?,?,?,?,'reserved',?,?,?)")
        .bind(a.id).bind(f.session).bind(f.bot).bind(f.run).bind(f.turn).bind(f.generation).bind(a.ordinal).bind(a.request_hash).bind(a.speed).bind(a.reserved_microusd).bind(a.pricing_version).bind(&at).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Reservation::Fresh(revision))
}
/// Keep vendor usage extensions, but reject credential-bearing objects. Full responses,
/// prompts, auth/config objects are never accepted as accounting data.
fn usage_json(usage: Option<&Value>) -> Result<Option<String>> {
    let Some(value) = usage else { return Ok(None) };
    let object = value.as_object().context("invalid usage")?;
    fn safe(value: &Value) -> bool {
        match value {
            Value::Object(map) => map.iter().all(|(k, v)| {
                !matches!(
                    k.to_ascii_lowercase().as_str(),
                    "key"
                        | "api_key"
                        | "key_file"
                        | "authorization"
                        | "x-api-key"
                        | "access_token"
                        | "token"
                        | "content"
                        | "messages"
                        | "prompt"
                ) && safe(v)
            }),
            Value::Array(values) => values.iter().all(safe),
            Value::String(text) => !text.contains("sk-ant-"),
            _ => true,
        }
    }
    ensure!(safe(value), "usage contains non-accounting data");
    for key in [
        "input_tokens",
        "output_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
    ] {
        if let Some(n) = object.get(key) {
            ensure!(n.as_u64().is_some(), "invalid usage token count");
        }
    }
    let encoded = serde_json::to_string(value)?;
    ensure!(encoded.len() <= 16 * 1024, "usage size limit exceeded");
    Ok(Some(encoded))
}
pub struct Receipt<'a> {
    pub state: &'a str,
    pub actual_speed: Option<&'a str>,
    pub usage: Option<&'a Value>,
    pub estimated_microusd: Option<i64>,
}
pub async fn record(
    pool: &SqlitePool,
    f: &Fence<'_>,
    id: &str,
    receipt: &Receipt<'_>,
    checkpoint: &Checkpoint,
) -> Result<i64> {
    ensure!(
        matches!(receipt.state, "recorded" | "failed" | "unknown")
            && receipt
                .actual_speed
                .is_none_or(|s| matches!(s, "fast" | "standard"))
            && receipt.estimated_microusd.is_none_or(|n| n >= 0),
        "invalid receipt"
    );
    ensure!(
        checkpoint.api_pending() == (receipt.state == "unknown"),
        "unknown HTTP remains pending; known response must settle marker"
    );
    if receipt.state == "unknown" {
        ensure!(
            receipt.estimated_microusd.is_none(),
            "unknown usage cannot be zero cost"
        );
    }
    if let Some(speed) = receipt
        .usage
        .and_then(|u| u.get("speed"))
        .and_then(Value::as_str)
    {
        ensure!(
            Some(speed) == receipt.actual_speed,
            "usage speed and receipt disagree"
        );
    }
    let usage = usage_json(receipt.usage)?;
    let mut tx = super::begin_write(pool).await?;
    let at = super::now();
    let row = current(&mut tx, f, &at).await?;
    let existing = attempt(&mut tx, id).await?.context("attempt missing")?;
    ensure!(
        existing.session_id.as_deref() == Some(f.session)
            && existing.run_id.as_deref() == Some(f.run)
            && existing.turn_id.as_deref() == Some(f.turn)
            && existing.generation == f.generation,
        "attempt owner conflict"
    );
    if existing.state != "reserved" {
        ensure!(
            existing.state == receipt.state
                && existing.actual_speed.as_deref() == receipt.actual_speed
                && existing.usage_json == usage
                && existing.estimated_microusd == receipt.estimated_microusd,
            "receipt conflict"
        );
        tx.commit().await?;
        return Ok(row.revision);
    }
    let before = Checkpoint::new(&serde_json::from_str(&row.checkpoint_json)?)?;
    ensure!(before.api_pending(), "missing pending HTTP checkpoint");
    let revision = cas(&mut tx, f, checkpoint, &at).await?;
    sqlx::query("UPDATE api_bot_attempts SET state=?,actual_speed=?,usage_json=?,estimated_microusd=?,finished_at=? WHERE id=? AND state='reserved'")
        .bind(receipt.state).bind(receipt.actual_speed).bind(usage).bind(receipt.estimated_microusd).bind(&at).bind(id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(revision)
}
/// No replay: recovery marks unconfirmed reservations unknown and preserves their cost hold.
pub async fn recover_unknown(pool: &SqlitePool, run_id: &str) -> Result<u64> {
    Ok(sqlx::query("UPDATE api_bot_attempts SET state='unknown',estimated_microusd=NULL,finished_at=? WHERE state='reserved' AND run_id=? AND NOT EXISTS(SELECT 1 FROM runs WHERE id=? AND state IN ('running','starting'))")
        .bind(super::now()).bind(run_id).bind(run_id).execute(pool).await?.rows_affected())
}
/// Close every still-reserved attempt for one exact live API-bot turn inside its caller's
/// transaction. A zero-row result means only that there was no matching reservation; callers
/// must independently prove the exact live owner and must not treat this count as that proof.
pub async fn close_reserved_unknown_on(
    conn: &mut SqliteConnection,
    bot_id: &str,
    run_id: &str,
    turn_id: &str,
    finished_at: &str,
) -> Result<u64> {
    Ok(sqlx::query(
        "UPDATE api_bot_attempts
            SET state='unknown', estimated_microusd=NULL, finished_at=?
          WHERE bot_id=? AND run_id=? AND turn_id=? AND state='reserved'
            AND EXISTS (
                SELECT 1
                  FROM bots b
                  JOIN runs r ON r.bot_id=b.id
                  JOIN turns t ON t.run_id=r.id
                  JOIN conversations c ON c.id=t.conversation_id
                 WHERE b.id=? AND b.kind='claude-api' AND b.deleted_at IS NULL
                   AND r.id=? AND r.state='running'
                   AND t.id=? AND t.run_id=r.id AND t.status='in_flight'
                   AND c.bot_id=b.id
            )",
    )
    .bind(finished_at)
    .bind(bot_id)
    .bind(run_id)
    .bind(turn_id)
    .bind(bot_id)
    .bind(run_id)
    .bind(turn_id)
    .execute(conn)
    .await?
    .rows_affected())
}
async fn prune_in(conn: &mut SqliteConnection, at: &str) -> Result<u64> {
    sqlx::query("UPDATE api_bot_attempts SET state='unknown',estimated_microusd=NULL,finished_at=? WHERE state='reserved' AND (session_id IS NULL OR session_id IN(SELECT s.id FROM api_bot_sessions s JOIN bots b ON b.id=s.bot_id WHERE s.expires_at<=? OR b.deleted_at IS NOT NULL))")
        .bind(at).bind(at).execute(&mut *conn).await?;
    Ok(sqlx::query("DELETE FROM api_bot_sessions WHERE expires_at<=? OR bot_id IN(SELECT id FROM bots WHERE deleted_at IS NOT NULL)").bind(at).execute(conn).await?.rows_affected())
}
/// C must schedule this at boot/hourly and on delete; B adds no production task.
pub async fn prune(pool: &SqlitePool) -> Result<u64> {
    let mut tx = super::begin_write(pool).await?;
    let n = prune_in(&mut tx, &super::now()).await?;
    tx.commit().await?;
    Ok(n)
}

/// Explicit handover only after a settled checkpoint, never a recovery replay.
pub async fn rebind(
    pool: &SqlitePool,
    old: &Fence<'_>,
    new_run: &str,
    new_turn: &str,
    new_generation: i64,
) -> Result<i64> {
    ensure!(
        new_generation >= old.generation && (new_run == old.run || new_generation > old.generation),
        "generation must advance for a new run"
    );
    let mut tx = super::begin_write(pool).await?;
    let at = super::now();
    let next = Fence {
        session: old.session,
        bot: old.bot,
        run: new_run,
        turn: new_turn,
        generation: new_generation,
        revision: old.revision,
    };
    owner(&mut tx, &next).await?;
    let row:Stored=sqlx::query_as("SELECT owner_run_id,owner_turn_id,generation,revision,checkpoint_json FROM api_bot_sessions WHERE id=? AND bot_id=? AND retired_at IS NULL AND expires_at>?")
        .bind(old.session).bind(old.bot).bind(&at).fetch_optional(&mut *tx).await?.context("session unavailable")?;
    ensure!(
        row.generation == old.generation
            && row.revision == old.revision
            && row.owner_run_id.as_deref() == Some(old.run)
            && row.owner_turn_id.as_deref() == Some(old.turn),
        "stale handover fence"
    );
    let checkpoint = Checkpoint::new(&serde_json::from_str(&row.checkpoint_json)?)?;
    ensure!(
        !checkpoint.pending(),
        "unresolved session cannot be handed over"
    );
    let revision = old.revision.checked_add(1).context("revision overflow")?;
    sqlx::query("UPDATE api_bot_sessions SET owner_run_id=?,owner_turn_id=?,generation=?,revision=? WHERE id=?")
        .bind(new_run).bind(new_turn).bind(new_generation).bind(revision).bind(old.session).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(revision)
}
/// Explicit fresh-chat retirement erases the private body, while preserving accounting.
pub async fn retire(pool: &SqlitePool, f: &Fence<'_>) -> Result<()> {
    let revision = f.revision.checked_add(1).context("revision overflow")?;
    let mut tx = super::begin_write(pool).await?;
    let at = super::now();
    let changed=sqlx::query("UPDATE api_bot_sessions SET retired_at=?,checkpoint_json='{}',revision=? WHERE id=? AND bot_id=? AND owner_run_id=? AND owner_turn_id=? AND generation=? AND revision=? AND retired_at IS NULL")
        .bind(&at).bind(revision).bind(f.session).bind(f.bot).bind(f.run).bind(f.turn).bind(f.generation).bind(f.revision).execute(&mut *tx).await?.rows_affected();
    ensure!(changed == 1, "stale retirement fence");
    sqlx::query("UPDATE api_bot_attempts SET state='unknown',estimated_microusd=NULL,finished_at=? WHERE session_id=? AND state='reserved'")
        .bind(&at).bind(f.session).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    async fn pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query("PRAGMA foreign_keys=ON")
            .execute(&pool)
            .await
            .unwrap();
        super::super::migrate_with(&pool, &[]).await.unwrap();
        sqlx::query("INSERT INTO projects(id,path,label,created_at)VALUES('p','/test','p',?)")
            .bind(super::super::now())
            .execute(&pool)
            .await
            .unwrap();
        for id in ["a", "b"] {
            sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at)VALUES(?,'p',?,'claude-api','fixture',?)").bind(id).bind(id).bind(super::super::now()).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO runs(id,bot_id,state,started_at)VALUES(?,?,'running',?)")
                .bind(format!("r{id}"))
                .bind(id)
                .bind(super::super::now())
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query("INSERT INTO conversations(id,bot_id,created_at)VALUES(?,?,?)")
                .bind(format!("c{id}"))
                .bind(id)
                .bind(super::super::now())
                .execute(&pool)
                .await
                .unwrap();
            for i in [1, 2] {
                sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,created_at)VALUES(?,?,?,'web',?,?)")
                    .bind(format!("t{id}{i}")).bind(format!("c{id}")).bind(format!("r{id}")).bind(if i == 1 { "in_flight" } else { "queued" }).bind(super::super::now()).execute(&pool).await.unwrap();
            }
        }
        pool
    }
    async fn advance_turn(pool: &SqlitePool) {
        sqlx::query("UPDATE turns SET status='completed',completed_at=? WHERE id='ta1'")
            .bind(super::super::now()).execute(pool).await.unwrap();
        sqlx::query("UPDATE turns SET status='in_flight' WHERE id='ta2'")
            .execute(pool).await.unwrap();
    }
    fn fence() -> Fence<'static> {
        Fence {
            session: "s",
            bot: "a",
            run: "ra",
            turn: "ta1",
            generation: 1,
            revision: 0,
        }
    }
    fn checkpoint(pending: bool) -> Checkpoint {
        Checkpoint::new(&json!({"messages":[{"role":"user","content":"PRIVATE-CHECKPOINT-ONLY"}],"responses":[{"content":[{"type":"thinking","thinking":"","signature":"opaque-original"}]}],"api_pending":pending,"tool_pending":null,"completed_tool_ids":[]})).unwrap()
    }
    fn attempt_data() -> Attempt<'static> {
        Attempt {
            id: "attempt-a",
            ordinal: 0,
            request_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            speed: "fast",
            reserved_microusd: 100,
            pricing_version: "fixture-rates-v1",
        }
    }
    async fn count(pool: &SqlitePool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*)FROM api_bot_attempts")
            .fetch_one(pool)
            .await
            .unwrap()
    }
    #[tokio::test]
    async fn full_checkpoint_roundtrips_but_debug_and_accounting_never_include_it() {
        let pool = pool().await;
        let f = fence();
        let c = checkpoint(false);
        create(&pool, &f, &c).await.unwrap();
        assert_eq!(load(&pool, &f).await.unwrap().value(), c.value());
        assert!(!format!("{c:?}").contains("PRIVATE"));
        let f = Fence {
            revision: save(&pool, &f, &c).await.unwrap(),
            ..f
        };
        assert_eq!(
            load(&pool, &f).await.unwrap().value()["responses"][0]["content"][0]["signature"],
            "opaque-original"
        );
        assert!(Checkpoint::new(&json!({"key_file":"should-not-persist"})).is_err());
        assert_eq!(count(&pool).await, 0);
    }
    #[tokio::test]
    async fn old_generation_revision_and_other_bot_run_turn_reject_without_writes() {
        let pool = pool().await;
        let f = fence();
        create(&pool, &f, &checkpoint(false)).await.unwrap();
        for wrong in [
            Fence {
                bot: "b",
                ..f.clone()
            },
            Fence {
                run: "rb",
                ..f.clone()
            },
            Fence {
                turn: "tb1",
                ..f.clone()
            },
            Fence {
                generation: 2,
                ..f.clone()
            },
            Fence {
                revision: 7,
                ..f.clone()
            },
        ] {
            assert!(save(&pool, &wrong, &checkpoint(false)).await.is_err());
            assert!(
                reserve(&pool, &wrong, &attempt_data(), &checkpoint(true), 1000, 10)
                    .await
                    .is_err()
            );
        }
        assert_eq!(count(&pool).await, 0);
        assert_eq!(
            load(&pool, &f).await.unwrap().value(),
            checkpoint(false).value()
        );
    }
    #[tokio::test]
    async fn duplicate_attempt_is_not_permission_to_send_and_receipt_counts_once() {
        let pool = pool().await;
        let f = fence();
        create(&pool, &f, &checkpoint(false)).await.unwrap();
        let a = attempt_data();
        assert_eq!(
            reserve(&pool, &f, &a, &checkpoint(true), 1000, 10)
                .await
                .unwrap(),
            Reservation::Fresh(1)
        );
        assert_eq!(
            reserve(&pool, &f, &a, &checkpoint(true), 1000, 10)
                .await
                .unwrap(),
            Reservation::Existing {
                revision: 1,
                state: "reserved".into()
            }
        );
        let wrong = Attempt {
            request_hash: "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            ..a.clone()
        };
        assert!(reserve(&pool, &f, &wrong, &checkpoint(true), 1000, 10)
            .await
            .is_err());
        let f = Fence { revision: 1, ..f };
        let usage = json!({"input_tokens":5,"output_tokens":7,"speed":"fast","vendor_extension":{"count":3}});
        let receipt = Receipt {
            state: "recorded",
            actual_speed: Some("fast"),
            usage: Some(&usage),
            estimated_microusd: Some(42),
        };
        assert_eq!(
            record(&pool, &f, a.id, &receipt, &checkpoint(false))
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            record(&pool, &f, a.id, &receipt, &checkpoint(false))
                .await
                .unwrap(),
            2
        );
        assert_eq!(count(&pool).await, 1);
        let (cost, stored): (Option<i64>, String) =
            sqlx::query_as("SELECT estimated_microusd,usage_json FROM api_bot_attempts")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(cost, Some(42));
        assert_eq!(serde_json::from_str::<Value>(&stored).unwrap(), usage);
        assert!(!stored.contains("PRIVATE"));
    }
    #[tokio::test]
    async fn budget_rate_and_unknown_reservations_remain_conservative() {
        let pool = pool().await;
        let f = fence();
        create(&pool, &f, &checkpoint(false)).await.unwrap();
        let a = attempt_data();
        assert!(reserve(&pool, &f, &a, &checkpoint(true), 99, 10)
            .await
            .is_err());
        assert_eq!(count(&pool).await, 0);
        assert!(reserve(&pool, &f, &a, &checkpoint(true), 100, 0)
            .await
            .is_err());
        reserve(&pool, &f, &a, &checkpoint(true), 100, 1)
            .await
            .unwrap();
        let f = Fence { revision: 1, ..f };
        let unknown = Receipt {
            state: "unknown",
            actual_speed: None,
            usage: None,
            estimated_microusd: None,
        };
        assert!(record(&pool, &f, a.id, &unknown, &checkpoint(false))
            .await
            .is_err());
        let revision = record(&pool, &f, a.id, &unknown, &checkpoint(true))
            .await
            .unwrap();
        let f = Fence { revision, ..f };
        assert!(reserve(
            &pool,
            &f,
            &Attempt {
                id: "next",
                ordinal: 1,
                ..a.clone()
            },
            &checkpoint(true),
            1000,
            10
        )
        .await
        .is_err());
        advance_turn(&pool).await;
        assert!(rebind(&pool, &f, "ra", "ta2", 1).await.is_err());
        let row: (Option<i64>, i64) =
            sqlx::query_as("SELECT estimated_microusd,reserved_microusd FROM api_bot_attempts")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row, (None, 100));
    }
    #[tokio::test]
    async fn only_retired_runs_are_recovered_and_pending_tool_never_hands_over() {
        let pool = pool().await;
        let f = fence();
        create(&pool, &f, &checkpoint(false)).await.unwrap();
        reserve(&pool, &f, &attempt_data(), &checkpoint(true), 1000, 10)
            .await
            .unwrap();
        assert_eq!(recover_unknown(&pool, "ra").await.unwrap(), 0);
        sqlx::query("UPDATE runs SET state='stopped'WHERE id='ra'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(recover_unknown(&pool, "ra").await.unwrap(), 1);
        assert_eq!(recover_unknown(&pool, "ra").await.unwrap(), 0);
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT state FROM api_bot_attempts")
                .fetch_one(&pool)
                .await
                .unwrap(),
            "unknown"
        );
    }
    #[tokio::test]
    async fn joined_turn_closeout_changes_all_exact_live_reservations_only() {
        let pool = pool().await;
        let f = fence();
        create(&pool, &f, &checkpoint(false)).await.unwrap();
        reserve(&pool, &f, &attempt_data(), &checkpoint(true), 1000, 10)
            .await
            .unwrap();
        let at = "2026-10-26T12:00:00.000Z";
        sqlx::query("INSERT INTO api_bot_attempts(id,session_id,bot_id,run_id,turn_id,generation,ordinal,request_hash,requested_speed,state,reserved_microusd,pricing_version,created_at) VALUES('reserved-b','s','a','ra','ta1',1,1,'0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef','standard','reserved',250,'fixture-v1',?)")
            .bind(super::super::now())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO api_bot_attempts(id,session_id,bot_id,run_id,turn_id,generation,ordinal,request_hash,requested_speed,actual_speed,state,usage_json,reserved_microusd,estimated_microusd,pricing_version,created_at,finished_at) VALUES('recorded','s','a','ra','ta1',1,2,'1123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef','fast','standard','recorded','{\"input_tokens\":7,\"output_tokens\":2}',300,43,'fixture-v1',?,'2026-10-25T12:00:00.000Z')")
            .bind(super::super::now())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO api_bot_attempts(id,session_id,bot_id,run_id,turn_id,generation,ordinal,request_hash,requested_speed,actual_speed,state,usage_json,reserved_microusd,estimated_microusd,pricing_version,created_at,finished_at) VALUES('failed','s','a','ra','ta1',1,3,'3123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef','standard','standard','failed','{\"input_tokens\":3}',320,18,'fixture-v1',?,'2026-10-24T12:00:00.000Z')")
            .bind(super::super::now())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO api_bot_attempts(id,session_id,bot_id,run_id,turn_id,generation,ordinal,request_hash,requested_speed,actual_speed,state,usage_json,reserved_microusd,estimated_microusd,pricing_version,created_at,finished_at) VALUES('unknown','s','a','ra','ta1',1,4,'4123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef','fast','fast','unknown','{\"input_tokens\":4}',340,NULL,'fixture-v1',?,'2026-10-23T12:00:00.000Z')")
            .bind(super::super::now())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO api_bot_attempts(id,session_id,bot_id,run_id,turn_id,generation,ordinal,request_hash,requested_speed,state,reserved_microusd,pricing_version,created_at) VALUES('detached',NULL,NULL,NULL,NULL,1,3,'2123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef','fast','reserved',90,'fixture-v1',?)")
            .bind(super::super::now())
            .execute(&pool)
            .await
            .unwrap();

        let recorded_before: (String, Option<String>, String, Option<String>, i64, Option<i64>, Option<String>) =
            sqlx::query_as("SELECT state,actual_speed,usage_json,finished_at,reserved_microusd,estimated_microusd,pricing_version FROM api_bot_attempts WHERE id='recorded'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let terminal_before: Vec<(String, String, Option<String>, Option<String>, i64, Option<i64>, Option<String>)> =
            sqlx::query_as("SELECT id,state,actual_speed,usage_json,reserved_microusd,estimated_microusd,finished_at FROM api_bot_attempts WHERE id IN ('recorded','failed','unknown') ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        for (bot, run, turn) in [
            ("b", "ra", "ta1"),
            ("a", "rb", "ta1"),
            ("a", "ra", "ta2"),
        ] {
            let mut tx = super::super::begin_write(&pool).await.unwrap();
            assert_eq!(
                close_reserved_unknown_on(&mut tx, bot, run, turn, at)
                    .await
                    .unwrap(),
                0
            );
            tx.rollback().await.unwrap();
        }

        let mut tx = super::super::begin_write(&pool).await.unwrap();
        assert_eq!(
            close_reserved_unknown_on(&mut tx, "a", "ra", "ta1", at)
                .await
                .unwrap(),
            2,
            "all exact reserved attempts close; count is not an owner proof"
        );
        tx.commit().await.unwrap();

        let reserved_rows: Vec<(String, String, Option<i64>, i64, Option<String>)> =
            sqlx::query_as("SELECT id,state,estimated_microusd,reserved_microusd,finished_at FROM api_bot_attempts WHERE id IN ('attempt-a','reserved-b') ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(reserved_rows.len(), 2);
        for (id, state, estimated, reserved, finished) in reserved_rows {
            assert_eq!((state.as_str(), estimated, reserved, finished.as_deref()), ("unknown", None, if id == "attempt-a" { 100 } else { 250 }, Some(at)));
        }
        let recorded_after: (String, Option<String>, String, Option<String>, i64, Option<i64>, Option<String>) =
            sqlx::query_as("SELECT state,actual_speed,usage_json,finished_at,reserved_microusd,estimated_microusd,pricing_version FROM api_bot_attempts WHERE id='recorded'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(recorded_after, recorded_before, "recorded usage and cost stay byte-for-byte unchanged");
        let terminal_after: Vec<(String, String, Option<String>, Option<String>, i64, Option<i64>, Option<String>)> =
            sqlx::query_as("SELECT id,state,actual_speed,usage_json,reserved_microusd,estimated_microusd,finished_at FROM api_bot_attempts WHERE id IN ('recorded','failed','unknown') ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(terminal_after, terminal_before, "recorded/failed/unknown rows are not rewritten");
        let detached: (Option<String>, Option<String>, Option<String>, String, i64, Option<i64>, Option<String>) =
            sqlx::query_as("SELECT bot_id,run_id,turn_id,state,reserved_microusd,estimated_microusd,finished_at FROM api_bot_attempts WHERE id='detached'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(detached, (None, None, None, "reserved".into(), 90, None, None));
    }
    #[tokio::test]
    async fn successful_handover_fences_old_turn_and_explicit_retirement_erases_content() {
        let pool = pool().await;
        let f = fence();
        create(&pool, &f, &checkpoint(false)).await.unwrap();
        advance_turn(&pool).await;
        assert_eq!(rebind(&pool, &f, "ra", "ta2", 1).await.unwrap(), 1);
        assert!(load(&pool, &f).await.is_err());
        let f = Fence {
            turn: "ta2",
            revision: 1,
            ..f
        };
        assert!(load(&pool, &f).await.is_ok());
        retire(&pool, &f).await.unwrap();
        assert!(load(&pool, &f).await.is_err());
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT checkpoint_json FROM api_bot_sessions")
                .fetch_one(&pool)
                .await
                .unwrap(),
            "{}"
        );
    }
    #[tokio::test]
    async fn expiry_and_soft_delete_purge_private_state_but_retain_unknown_accounting() {
        for soft_delete in [false, true] {
            let pool = pool().await;
            let f = fence();
            create(&pool, &f, &checkpoint(false)).await.unwrap();
            reserve(&pool, &f, &attempt_data(), &checkpoint(true), 1000, 10)
                .await
                .unwrap();
            if soft_delete {
                sqlx::query("UPDATE bots SET deleted_at=?WHERE id='a'")
                    .bind(super::super::now())
                    .execute(&pool)
                    .await
                    .unwrap();
            } else {
                sqlx::query("UPDATE api_bot_sessions SET expires_at=?")
                    .bind(super::super::iso_in(-1))
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            assert!(load(
                &pool,
                &Fence {
                    revision: 1,
                    ..f.clone()
                }
            )
            .await
            .is_err());
            assert_eq!(prune(&pool).await.unwrap(), 1);
            assert_eq!(prune(&pool).await.unwrap(), 0);
            assert_eq!(
                sqlx::query_scalar::<_, i64>("SELECT COUNT(*)FROM api_bot_sessions")
                    .fetch_one(&pool)
                    .await
                    .unwrap(),
                0
            );
            let row:(Option<String>,String,Option<i64>,i64)=sqlx::query_as("SELECT session_id,state,estimated_microusd,reserved_microusd FROM api_bot_attempts").fetch_one(&pool).await.unwrap();
            assert_eq!(row, (None, "unknown".into(), None, 100));
        }
    }
    #[tokio::test]
    async fn hard_bot_purge_cascades_private_data_but_preserves_billing_and_rejects_secrets() {
        let pool = pool().await;
        let f = fence();
        create(&pool, &f, &checkpoint(false)).await.unwrap();
        let a = attempt_data();
        reserve(&pool, &f, &a, &checkpoint(true), 1000, 10)
            .await
            .unwrap();
        let f = Fence { revision: 1, ..f };
        let bad = json!({"key_file":"private","input_tokens":1});
        assert!(record(
            &pool,
            &f,
            a.id,
            &Receipt {
                state: "recorded",
                actual_speed: None,
                usage: Some(&bad),
                estimated_microusd: Some(0)
            },
            &checkpoint(false)
        )
        .await
        .is_err());
        record(
            &pool,
            &f,
            a.id,
            &Receipt {
                state: "recorded",
                actual_speed: None,
                usage: Some(&json!({"input_tokens":1})),
                estimated_microusd: Some(12),
            },
            &checkpoint(false),
        )
        .await
        .unwrap();
        sqlx::query("DELETE FROM turns WHERE conversation_id='ca'")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM conversations WHERE id='ca'")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM runs WHERE id='ra'")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM bots WHERE id='a'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*)FROM api_bot_sessions")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        let row: (Option<String>, Option<String>, Option<i64>) =
            sqlx::query_as("SELECT bot_id,session_id,estimated_microusd FROM api_bot_attempts")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row, (None, None, Some(12)));
    }
}

/// Restart abandonment retains the reservation when the request outcome is unknown.
/// Caller must prove the exact abandoned run in the same write transaction.
pub async fn abandon_run_reserved_on(conn: &mut SqliteConnection, bot: &str, run: &str, at: &str) -> Result<u64> {
    Ok(sqlx::query("UPDATE api_bot_attempts SET state='unknown', estimated_microusd=NULL, finished_at=? WHERE bot_id=? AND run_id=? AND state='reserved'")
        .bind(at).bind(bot).bind(run).execute(conn).await?.rows_affected())
}
