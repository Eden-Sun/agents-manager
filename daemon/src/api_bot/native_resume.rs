//! Durable continuity evidence only. This is neither join nor completion proof.
use super::{adapter::AdapterIdentity, runtime::startup_context_settled};
use crate::lifecycle::api_bot_port::ApiBotError;
use std::future::Future;

pub(super) struct ResumeHandoffProof {
    pub(super) identity: AdapterIdentity,
    pub(super) revision: i64,
}

pub(super) fn prove<'a>(pool: &'a sqlx::SqlitePool, bot: &'a str, new_run: &'a str, old_run: &'a str)
    -> impl Future<Output = Result<ResumeHandoffProof, ApiBotError>> + Send + 'a {
    Box::pin(prove_body(pool, bot, new_run, old_run))
}

async fn prove_body(pool: &sqlx::SqlitePool, bot: &str, new_run: &str, old_run: &str) -> Result<ResumeHandoffProof, ApiBotError> {
    let mut tx = crate::db::begin_write(pool).await.map_err(|_| ApiBotError::Unconfirmed)?;
    let proof = prove_on(&mut tx, bot, old_run, ProofMode::RuntimeBeforeRebind(new_run)).await?;
    tx.commit().await.map_err(|_| ApiBotError::Unconfirmed)?;
    Ok(proof)
}

pub(super) enum ProofMode<'a> { StartBeforeRun, RuntimeBeforeRebind(&'a str) }

pub(super) fn prove_on<'a>(conn: &'a mut sqlx::SqliteConnection, bot: &'a str, old_run: &'a str, mode: ProofMode<'a>) -> impl Future<Output = Result<ResumeHandoffProof, ApiBotError>> + Send + 'a {
    Box::pin(prove_on_inner(conn, bot, old_run, mode))
}
async fn prove_on_inner(conn: &mut sqlx::SqliteConnection, bot: &str, old_run: &str, mode: ProofMode<'_>) -> Result<ResumeHandoffProof, ApiBotError> {
    use am_base::db::api_bot::{Checkpoint, MAX_CHECKPOINT_BYTES};
    let expiry = crate::db::ts_sql("s.expires_at");
    let sql = format!("SELECT s.id,s.owner_turn_id,s.generation,s.revision,s.format_version, CASE WHEN length(CAST(s.checkpoint_json AS BLOB))<=? THEN s.checkpoint_json END FROM api_bot_sessions s JOIN bots b ON b.id=s.bot_id JOIN projects p ON p.id=b.project_id JOIN runs old ON old.id=s.owner_run_id AND old.bot_id=b.id JOIN turns t ON t.id=s.owner_turn_id AND t.run_id=old.id JOIN conversations c ON c.id=t.conversation_id AND c.bot_id=b.id JOIN api_bot_join_receipts j ON j.session_id=s.id AND j.bot_id=b.id AND j.run_id=old.id AND j.turn_id=t.id AND j.generation=s.generation AND j.revision=s.revision WHERE b.id=? AND b.kind='claude-api' AND b.deleted_at IS NULL AND p.deleted_at IS NULL AND p.host='local' AND p.handed_off_to IS NULL AND s.retired_at IS NULL AND {expiry}>? AND old.id=? AND old.state IN ('stopped','exited') AND t.status='completed' AND t.delivery='ok' AND j.state='applied' AND j.outcome='completed' AND j.final_text IS NOT NULL AND (SELECT count(*) FROM messages m WHERE m.turn_id=t.id AND m.role='assistant')=1 AND EXISTS(SELECT 1 FROM messages m WHERE m.turn_id=t.id AND m.conversation_id=c.id AND m.role='assistant' AND m.source='transcript' AND m.incomplete=0 AND m.rewound_at IS NULL AND m.content=j.final_text) AND NOT EXISTS(SELECT 1 FROM api_bot_join_receipts x WHERE x.session_id=s.id AND (x.state!='applied' OR (x.turn_id!=t.id AND x.revision>=s.revision))) AND ((? IS NULL AND NOT EXISTS(SELECT 1 FROM runs n WHERE n.bot_id=b.id AND n.state IN ('starting','running','stopping'))) OR (? IS NOT NULL AND EXISTS(SELECT 1 FROM runs n WHERE n.id=? AND n.bot_id=b.id AND n.state='running') AND NOT EXISTS(SELECT 1 FROM runs n WHERE n.bot_id=b.id AND n.id!=? AND n.state IN ('starting','running','stopping'))))");
    let new_run = match mode { ProofMode::StartBeforeRun => None, ProofMode::RuntimeBeforeRebind(run) => Some(run) };
    let row: Option<(String,String,i64,i64,i64,Option<String>)> = sqlx::query_as(&sql)
        .bind(MAX_CHECKPOINT_BYTES as i64).bind(bot).bind(crate::db::now()).bind(old_run).bind(new_run).bind(new_run).bind(new_run).bind(new_run)
        .fetch_optional(&mut *conn).await.map_err(|_| ApiBotError::Unconfirmed)?;
    let Some((session, turn, generation, revision, 1, Some(raw))) = row else { return Err(ApiBotError::Unconfirmed); };
    let value: serde_json::Value = serde_json::from_str(&raw).map_err(|_| ApiBotError::Unconfirmed)?;
    let checkpoint = Checkpoint::new(&value).map_err(|_| ApiBotError::Unconfirmed)?;
    let state: super::runner::State = serde_json::from_value(checkpoint.value()).map_err(|_| ApiBotError::Unconfirmed)?;
    if !startup_context_settled(&state) { return Err(ApiBotError::Unconfirmed); }
    Ok(ResumeHandoffProof { identity: AdapterIdentity { session_id: session, bot_id: bot.to_owned(), run_id: old_run.to_owned(), turn_id: turn, generation }, revision })
}

#[cfg(all(test, feature = "daemon-test-harness"))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resume_proof_requires_applied_and_settled_checkpoint_in_both_modes() {
        let env = crate::testing::env().await;
        let bot=crate::db::ulid(); let old=crate::db::ulid(); let new=crate::db::ulid(); let turn=crate::db::ulid(); let conv=crate::db::ulid(); let session=crate::db::ulid(); let message=crate::db::ulid();
        let now=crate::db::now();
        let checkpoint = r#"{"messages":[],"responses":[],"api_pending":false,"tool_pending":null,"completed_tool_ids":[]}"#;
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES(?,?,?,'claude-api','fixture',?)").bind(&bot).bind(&env.project_id).bind(&bot).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES(?,?,'exited',?)").bind(&old).bind(&bot).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO conversations(id,bot_id,created_at) VALUES(?,?,?)").bind(&conv).bind(&bot).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES(?,?,?,'web','completed','ok',?)").bind(&turn).bind(&conv).bind(&old).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO api_bot_sessions(id,bot_id,owner_run_id,owner_turn_id,generation,revision,checkpoint_json,created_at,updated_at,expires_at) VALUES(?,?,?,?,1,1,?,?,?,?)").bind(&session).bind(&bot).bind(&old).bind(&turn).bind(checkpoint).bind(&now).bind(&now).bind(crate::db::iso_in(3600)).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,final_text,state,created_at,processed_at) VALUES(?,?,?,?,1,1,'completed','fixture','applied',?,?)").bind(&session).bind(&bot).bind(&old).bind(&turn).bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO messages(id,conversation_id,turn_id,role,content,source,created_at) VALUES(?,?,?,'assistant','fixture','transcript',?)").bind(&message).bind(&conv).bind(&turn).bind(&now).execute(&env.app.db).await.unwrap();
        let mut tx = crate::db::begin_write(&env.app.db).await.unwrap();
        assert!(prove_on(&mut tx, &bot, &old, ProofMode::StartBeforeRun).await.is_ok());
        tx.rollback().await.unwrap();
        sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES(?,?,'running',?)").bind(&new).bind(&bot).bind(&now).execute(&env.app.db).await.unwrap();
        assert!(prove(&env.app.db, &bot, &new, &old).await.is_ok());
        sqlx::query("UPDATE api_bot_sessions SET checkpoint_json=? WHERE id=?").bind(r#"{"messages":[],"responses":[],"api_pending":true,"tool_pending":null,"completed_tool_ids":[]}"#).bind(&session).execute(&env.app.db).await.unwrap();
        assert!(prove(&env.app.db, &bot, &new, &old).await.is_err());
        sqlx::query("UPDATE api_bot_sessions SET checkpoint_json=? WHERE id=?").bind(r#"{"messages":[],"responses":[],"api_pending":false,"tool_pending":"fixture","completed_tool_ids":[]}"#).bind(&session).execute(&env.app.db).await.unwrap();
        assert!(prove(&env.app.db, &bot, &new, &old).await.is_err());
        sqlx::query("UPDATE api_bot_sessions SET checkpoint_json=? WHERE id=?").bind(checkpoint).bind(&session).execute(&env.app.db).await.unwrap();
        sqlx::query("UPDATE api_bot_join_receipts SET state='fenced' WHERE session_id=?").bind(&session).execute(&env.app.db).await.unwrap();
        assert!(prove(&env.app.db, &bot, &new, &old).await.is_err());
    }
}
