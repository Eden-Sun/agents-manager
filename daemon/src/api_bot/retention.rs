//! Online retention: a deletion proof is not a runner join or completion proof.
use std::{future::Future, sync::Arc, time::Duration};
use am_base::db::api_bot::{retention_candidates, retention_delete_on};

pub(super) type RetentionCandidate = (String,String,Option<String>,Option<String>,i64,i64,String,Option<String>,Option<String>);
use crate::lifecycle::api_bot_port::ApiRunKey;

pub(super) struct RetentionDeletable;

pub(super) fn prove<'a>(conn: &'a mut sqlx::SqliteConnection, c: &'a RetentionCandidate) -> impl Future<Output = Result<RetentionDeletable, &'static str>> + Send + 'a {
    Box::pin(prove_body(conn, c))
}

async fn prove_body(conn: &mut sqlx::SqliteConnection, c: &RetentionCandidate) -> Result<RetentionDeletable, &'static str> {
    let has_receipt: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM api_bot_join_receipts WHERE session_id=?)").bind(&c.1).fetch_one(&mut *conn).await.map_err(|_| "api_retention_storage")?;
    if !has_receipt { return Err("api_retention_no_receipt"); }
    let Some(owner_run) = c.2.as_deref() else { return Err("api_retention_inconsistent"); };
    let Some(owner_turn) = c.3.as_deref() else { return Err("api_retention_inconsistent"); };
    let mut cursor: Option<String> = None;
    let mut count = 0usize;
    let mut owner_seen = false;
    let mut revisions = std::collections::HashSet::new();
    loop {
        let rows: Vec<(String,i64,i64,String,String,String,String,bool)> = sqlx::query_as(
            "SELECT j.turn_id,j.revision,j.generation,j.bot_id,j.run_id,j.outcome,j.state, EXISTS(SELECT 1 FROM runs r JOIN turns t ON t.run_id=r.id JOIN conversations c ON c.id=t.conversation_id WHERE r.id=j.run_id AND r.bot_id=j.bot_id AND r.state IN ('stopped','exited') AND t.id=j.turn_id AND c.bot_id=j.bot_id AND t.delivery='ok' AND ((j.outcome='completed' AND t.status='completed' AND (SELECT count(*) FROM messages m WHERE m.turn_id=t.id AND m.role='assistant')=1 AND EXISTS(SELECT 1 FROM messages m WHERE m.turn_id=t.id AND m.conversation_id=c.id AND m.role='assistant' AND m.source='transcript' AND m.incomplete=0 AND m.rewound_at IS NULL AND m.content=j.final_text)) OR (j.outcome IN ('failed','cancelled') AND t.status='failed' AND (SELECT count(*) FROM messages m WHERE m.turn_id=t.id AND m.role='assistant')=0))) FROM api_bot_join_receipts j WHERE j.session_id=? AND (? IS NULL OR j.turn_id>?) ORDER BY j.turn_id LIMIT 64")
            .bind(&c.1).bind(cursor.as_deref()).bind(cursor.as_deref()).fetch_all(&mut *conn).await.map_err(|_| "api_retention_storage")?;
        if rows.is_empty() { break; }
        for (turn, revision, generation, bot, run, _outcome, state, valid) in &rows {
            count += 1;
            if count > 4096 { return Err("api_retention_evidence_bound"); }
            if !revisions.insert(*revision) { return Err("api_retention_inconsistent"); }
            if state == "pending" { return Err("api_retention_pending"); }
            if state == "fenced" { return Err("api_retention_fenced"); }
            if state != "applied" || !valid || bot != &c.0 { return Err("api_retention_inconsistent"); }
            if turn == owner_turn {
                if owner_seen || run != owner_run || *generation != c.4 || *revision != c.5 { return Err("api_retention_inconsistent"); }
                owner_seen = true;
            } else if *revision >= c.5 {
                return Err("api_retention_inconsistent");
            }
        }
        cursor = rows.last().map(|row| row.0.clone());
        if rows.len() < 64 { break; }
    }
    if count == 0 { return Err("api_retention_no_receipt"); }
    if !owner_seen { return Err("api_retention_inconsistent"); }
    Ok(RetentionDeletable)
}

pub(super) fn scan_once(app: &Arc<crate::state::App>) -> impl Future<Output = anyhow::Result<()>> + Send + '_ {
    Box::pin(scan_body(app))
}
async fn scan_body(app: &Arc<crate::state::App>) -> anyhow::Result<()> {
    if !app.api_startup_gate.ready() || app.shutdown.is_cancelled() { return Ok(()); }
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM bots WHERE kind='claude-api')").fetch_one(&app.db).await?;
    if !exists { return Ok(()); }
    let at = crate::db::now();
    let mut after: Option<(String,String)> = None;
    loop {
        let page = retention_candidates(&app.db, after.as_ref().map(|v| (v.0.as_str(),v.1.as_str())), &at).await?;
        if page.is_empty() { break; }
        after = page.last().map(|c| (c.0.clone(), c.1.clone()));
        for c in &page {
            if app.shutdown.is_cancelled() { return Ok(()); }
            let Some(run) = c.2.as_ref() else {
                let has_receipt: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM api_bot_join_receipts WHERE session_id=?)").bind(&c.1).fetch_one(&app.db).await?;
                let reason = if has_receipt { "api_retention_inconsistent" } else { "api_retention_no_receipt" };
                tracing::info!(reason, "API retention deferred");
                continue;
            };
            let key = ApiRunKey { bot_id: c.0.clone(), run_id: run.clone() };
            let lock = crate::capabilities::BotLocks::bot_lock(app.as_ref(), &c.0).await;
            let _lifecycle_lock = tokio::select! {
                biased;
                _ = app.shutdown.cancelled() => return Ok(()),
                guard = lock.lock_owned() => guard,
            };
            let _guard = match app.api_bot_runtime.retention_guard(app, &key).await {
                Ok(guard) => guard,
                Err(_) => { tracing::info!(reason="api_retention_busy", "API retention deferred"); continue; }
            };
            #[cfg(all(test, feature = "daemon-test-harness"))]
            crate::lifecycle::race_point::hit("api_retention_before_tx", &key.run_id).await;
            let mut tx = crate::db::begin_write(&app.db).await?;
            if let Err(reason) = prove(&mut tx, c).await {
                tx.rollback().await?;
                tracing::info!(reason, "API retention deferred");
                continue;
            }
            if !retention_delete_on(&mut tx, c, &at).await? {
                tx.rollback().await?;
                tracing::info!(reason="api_retention_inconsistent", "API retention deferred");
                continue;
            }
            tx.commit().await?;
        }
        if page.len() < 64 { break; }
    }
    Ok(())
}

pub(crate) fn spawn_retention_ticker(app: Arc<crate::state::App>) {
    crate::background_loop::spawn_periodic(&app, "API retention", Duration::from_secs(3600), Duration::from_secs(3600), |app| async move {
        if scan_once(&app).await.is_err() { tracing::warn!(reason="api_retention_storage", "API retention deferred"); }
    });
}

pub(crate) fn schedule_retention_after_delete(app: &Arc<crate::state::App>) {
    let app = app.clone();
    let tasks = app.background_tasks.clone();
    tasks.spawn(Box::pin(async move {
        if scan_once(&app).await.is_err() { tracing::warn!(reason="api_retention_storage", "API retention deferred"); }
    }));
}
