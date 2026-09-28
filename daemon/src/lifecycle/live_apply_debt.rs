//! Durable bookkeeping for a live setting whose TUI readback succeeded.
//!
//! A retry here only writes the observed snapshot to SQLite. It never sends a slash command or
//! picker key, and every write stays bound to the run that produced the readback.

use crate::{launch_rev, state::App};
use sqlx::{FromRow, SqlitePool};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

#[derive(Debug, Clone, FromRow)]
pub(crate) struct RuntimeDebt {
    pub run_id: String,
    pub bot_id: String,
    pub baseline_rev: String,
    pub target_rev: String,
    pub runtime_model: Option<String>,
    pub runtime_effort: Option<String>,
    pub runtime_fast: Option<i64>,
    pub created_at: String,
}

fn memory_debts() -> &'static Mutex<HashMap<String, RuntimeDebt>> {
    static DEBTS: OnceLock<Mutex<HashMap<String, RuntimeDebt>>> = OnceLock::new();
    DEBTS.get_or_init(Default::default)
}

fn workers() -> &'static Mutex<HashSet<String>> {
    static WORKERS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    WORKERS.get_or_init(Default::default)
}

pub(crate) async fn persist_and_commit(app: &Arc<App>, debt: RuntimeDebt) -> Result<(), String> {
    if let Err(error) = store_debt(&app.db, &debt).await {
        memory_debts()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(debt.run_id.clone(), debt.clone());
        schedule_retry(app, &debt.run_id);
        return Err(format!("live_runtime_debt_store_failed: {error}"));
    }

    match commit_debt(&app.db, &debt.run_id).await {
        Ok(Some(_)) | Ok(None) => {
            if let Err(error) = launch_rev::stamp_live_revision(&app.db, &debt.run_id).await {
                tracing::warn!(run_id = %debt.run_id, error = %error, "live runtime was stored; launch revision stamp remains retryable");
                schedule_retry(app, &debt.run_id);
            }
            Ok(())
        }
        Err(error) => {
            schedule_retry(app, &debt.run_id);
            Err(format!("live_runtime_write_failed: {error}"))
        }
    }
}

async fn store_debt(pool: &SqlitePool, debt: &RuntimeDebt) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO live_apply_debts
         (run_id, bot_id, baseline_rev, target_rev, runtime_model, runtime_effort, runtime_fast, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(run_id) DO NOTHING",
    )
    .bind(&debt.run_id)
    .bind(&debt.bot_id)
    .bind(&debt.baseline_rev)
    .bind(&debt.target_rev)
    .bind(&debt.runtime_model)
    .bind(&debt.runtime_effort)
    .bind(debt.runtime_fast)
    .bind(&debt.created_at)
    .execute(pool)
    .await?;
    Ok(())
}

async fn commit_debt(pool: &SqlitePool, run_id: &str) -> Result<Option<RuntimeDebt>, sqlx::Error> {
    // 先讀後寫：deferred 交易升級寫鎖時 SQLite 不跑 busy handler，背景重試與另一個 writer 同時動就直接
    // `database is locked`／BUSY_SNAPSHOT（#723）。BEGIN 就拿寫鎖，撞到只會等 busy_timeout。
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let debt = sqlx::query_as::<_, RuntimeDebt>("SELECT * FROM live_apply_debts WHERE run_id = ?")
        .bind(run_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(debt) = debt else {
        tx.commit().await?;
        return Ok(None);
    };

    let updated = sqlx::query(
        "UPDATE runs SET runtime_model = ?, runtime_effort = ?, runtime_fast = ?,
           live_rev = CASE WHEN launch_rev = ? OR launch_rev = ? THEN ? ELSE NULL END
         WHERE id = ? AND bot_id = ?",
    )
    .bind(&debt.runtime_model)
    .bind(&debt.runtime_effort)
    .bind(debt.runtime_fast)
    .bind(&debt.baseline_rev)
    .bind(&debt.target_rev)
    .bind(&debt.target_rev)
    .bind(&debt.run_id)
    .bind(&debt.bot_id)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(sqlx::Error::RowNotFound);
    }
    sqlx::query("DELETE FROM live_apply_debts WHERE run_id = ?")
        .bind(&debt.run_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(debt))
}

/// One DB-only retry step. It is also used by deterministic tests to release an injected SQL fault
/// without waiting on the background backoff loop.
pub(crate) async fn retry_once(app: &Arc<App>, run_id: &str) -> Result<bool, sqlx::Error> {
    let memory = memory_debts()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(run_id)
        .cloned();
    if let Some(debt) = memory {
        store_debt(&app.db, &debt).await?;
        memory_debts()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(run_id);
    }

    let committed = commit_debt(&app.db, run_id).await?;
    if let Some(committed) = committed {
        launch_rev::stamp_live_revision(&app.db, run_id).await?;
        app.emit_bot_status(&committed.bot_id).await;
        return Ok(true);
    }
    let stamped = launch_rev::stamp_live_revision(&app.db, run_id).await?;
    if stamped {
        if let Ok(Some(run)) = crate::db::run(&app.db, run_id).await {
            app.emit_bot_status(&run.bot_id).await;
        }
    }
    Ok(stamped)
}

fn schedule_retry(app: &Arc<App>, run_id: &str) {
    let should_start = workers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(run_id.to_string());
    if !should_start {
        return;
    }
    let (app, run_id) = (app.clone(), run_id.to_string());
    tokio::spawn(async move {
        let mut delay = Duration::from_millis(250);
        loop {
            tokio::time::sleep(delay).await;
            match retry_once(&app, &run_id).await {
                Ok(_) => break,
                // 原本那個 run 列不在了：絕不轉寫到別的 run，停手（SPEC §4.4a 第 5 點）。
                Err(sqlx::Error::RowNotFound) => {
                    tracing::warn!(run_id, "live-apply bookkeeping target run is gone; giving up");
                    break;
                }
                Err(error) => {
                    tracing::warn!(run_id, error = %error, "live-apply DB-only retry is still pending");
                    delay = (delay * 2).min(Duration::from_secs(30));
                }
            }
        }
        workers()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&run_id);
    });
}

/// Called during daemon startup so debts survive a process restart.
pub(crate) async fn recover(app: &Arc<App>) {
    match sqlx::query_scalar::<_, String>(
        "SELECT run_id FROM live_apply_debts UNION SELECT id FROM runs WHERE live_rev IS NOT NULL",
    )
    .fetch_all(&app.db)
    .await
    {
        Ok(run_ids) => {
            for run_id in run_ids {
                schedule_retry(app, &run_id);
            }
        }
        Err(error) => {
            tracing::warn!(error = %error, "could not enumerate live-apply bookkeeping debt at startup")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing as tt;

    /// #723：補寫帳時另一個 writer 正拿著寫入鎖（背景重試與手動重試同時跑就是這樣）。先讀後寫的 deferred 交易升級寫鎖時
    /// SQLite 不跑 busy handler，直接回 `database is locked`（對方剛 commit 則是 517 BUSY_SNAPSHOT）；要等對方放手再寫。
    #[tokio::test]
    async fn a_retry_waits_for_a_concurrent_writer_instead_of_failing_as_locked() {
        let e = tt::env().await;
        let bot = tt::claude_bot(&e.app, &e.project_id, "debt-lock").await;
        let run_id = tt::fake_run(&e.app, &bot.id).await;
        let rev = launch_rev::of(&bot);
        let debt = RuntimeDebt {
            run_id: run_id.clone(),
            bot_id: bot.id.clone(),
            baseline_rev: rev.clone(),
            target_rev: rev,
            runtime_model: Some("claude-opus-5-5".into()),
            runtime_effort: None,
            runtime_fast: None,
            created_at: crate::db::now(),
        };
        store_debt(&e.app.db, &debt).await.unwrap();

        let mut writer = e.app.db.begin_with("BEGIN IMMEDIATE").await.unwrap();
        sqlx::query("UPDATE runs SET agent_status = 'working' WHERE id = ?")
            .bind(&run_id)
            .execute(&mut *writer)
            .await
            .unwrap();
        let (app, id) = (e.app.clone(), run_id.clone());
        let retry = tokio::spawn(async move { retry_once(&app, &id).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        writer.commit().await.unwrap();
        retry.await.unwrap().expect("a busy writer only delays the retry; it is not a failure");

        let runtime: Option<String> = sqlx::query_scalar("SELECT runtime_model FROM runs WHERE id = ?")
            .bind(&run_id)
            .fetch_one(&e.app.db)
            .await
            .unwrap();
        assert_eq!(runtime.as_deref(), Some("claude-opus-5-5"));
    }
}
