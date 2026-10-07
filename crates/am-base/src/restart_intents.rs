//! 對帳用的 restart intent 查詢；復原流程由 `runners::restart_intents` 負責。

/// Reconcile must not heal a restart's committed stop while recovery still owns it.
pub async fn has_open_restart_for_run(
    pool: &sqlx::SqlitePool,
    host: &str,
    bot_id: &str,
    run_id: &str,
) -> anyhow::Result<bool> {
    let payload: Option<String> = sqlx::query_scalar(
        "SELECT payload_json FROM intents \
         WHERE kind='restart' AND subject_id=? AND host=? AND status IN ('pending','running')",
    )
    .bind(bot_id)
    .bind(host)
    .fetch_optional(pool)
    .await?;
    Ok(payload
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|payload| {
            payload
                .get("from_run_id")
                .and_then(serde_json::Value::as_str)
                .map(|id| id == run_id)
        })
        .unwrap_or(false))
}
