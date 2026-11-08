//! 排程的 DB 操作（schedules＋schedule_occurrences）。
//!
//! 時間戳一律用 `db::now()`（毫秒 RFC3339 UTC），字串比較等於時間比較。
//! occurrence 的「未結束」＝`pending`／`queued`。`queued` 等綁定的 turn 進終態才收斂：
//! completed＋delivery ok → done；delivery unknown（不論 completed_fallback 或 failed）→ unknown（待確認；同交易把排程暫停，不自動重跑）；
//! failed → failed；turn 不見了 → cancelled。`unknown` 不算未結束，但 UI 要顯示待確認。
//! 所有讀後寫都用 [`db::begin_write`]（BEGIN IMMEDIATE），避免 deferred 升鎖遇 SQLITE_BUSY。

use anyhow::{Context, Result};
use serde::Serialize;
use sqlx::{Sqlite, SqlitePool, Transaction};

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Schedule {
    pub id: String,
    pub project_id: String,
    pub bot_id: String,
    pub name: String,
    pub prompt: String,
    pub mode: String,
    pub tz: String,
    pub spec_json: String,
    pub next_due: Option<String>,
    pub enabled: i64,
    pub generation: i64,
    pub notify: String,
    /// 跑完就清空對話（0／1）：這顆排程 done 之後，綁定的 Bot 開新對話。
    pub clear_after: i64,
    pub retry_count: i64,
    pub retry_interval_mins: i64,
    pub retry_deadline_mins: i64,
    #[serde(skip)]
    pub create_crid: Option<String>,
    #[serde(skip)]
    pub create_fp: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub deleted_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Occurrence {
    pub id: String,
    pub schedule_id: String,
    pub source: String,
    pub scheduled_for: String,
    pub generation: i64,
    pub bot_id: String,
    pub prompt: String,
    /// 觸發它的時段標籤（一天多時段才有；空白＝沒有時段）。
    pub slot_label: Option<String>,
    pub client_request_id: Option<String>,
    pub status: String,
    pub waiting_reason: Option<String>,
    pub merged_count: i64,
    pub conversation_id: Option<String>,
    pub turn_id: Option<String>,
    pub error: Option<String>,
    pub outcome: Option<String>,
    pub outcome_summary: Option<String>,
    pub reply_message_id: Option<String>,
    pub attempt_no: i64,
    pub next_retry_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// 狀態轉移（終態）：給通知用。`notify` 決定要不要對外通知。
#[derive(Debug, Clone)]
pub struct Transition {
    pub schedule_id: String,
    pub schedule_name: String,
    pub project_id: String,
    pub occurrence_id: String,
    pub status: String,
    pub notify: String,
    pub error: Option<String>,
    /// 綁定的 Bot（done 之後跑「跑完就清空對話」用）。
    pub bot_id: String,
    /// 排程勾了「跑完就清空對話」。
    pub clear_after: bool,
    pub outcome: String,
    pub attempt_no: i64,
}

/// 這個終態要不要通知（依排程的 notify 設定）。
pub fn wants_notice(notify: &str, outcome: &str) -> bool {
    match notify {
        "all" => matches!(outcome, "success" | "failed" | "refused" | "unknown"),
        "failure" => matches!(outcome, "failed" | "refused" | "unknown"),
        _ => false,
    }
}

pub async fn insert_schedule_tx(tx: &mut Transaction<'_, Sqlite>, s: &Schedule) -> Result<()> {
    sqlx::query(
        "INSERT INTO schedules (id, project_id, bot_id, name, prompt, mode, tz, spec_json, next_due, enabled, generation, notify, clear_after, retry_count, retry_interval_mins, retry_deadline_mins, create_crid, create_fp, created_at, updated_at, deleted_at)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,NULL)",
    )
    .bind(&s.id)
    .bind(&s.project_id)
    .bind(&s.bot_id)
    .bind(&s.name)
    .bind(&s.prompt)
    .bind(&s.mode)
    .bind(&s.tz)
    .bind(&s.spec_json)
    .bind(&s.next_due)
    .bind(s.enabled)
    .bind(s.generation)
    .bind(&s.notify)
    .bind(s.clear_after)
    .bind(s.retry_count)
    .bind(s.retry_interval_mins)
    .bind(s.retry_deadline_mins)
    .bind(&s.create_crid)
    .bind(&s.create_fp)
    .bind(&s.created_at)
    .bind(&s.updated_at)
    .execute(&mut **tx)
    .await
    .context("insert schedule")?;
    Ok(())
}

pub async fn get_schedule(pool: &SqlitePool, id: &str) -> Result<Option<Schedule>> {
    Ok(sqlx::query_as::<_, Schedule>("SELECT * FROM schedules WHERE id = ?").bind(id).fetch_optional(pool).await?)
}

pub async fn get_schedule_tx(tx: &mut Transaction<'_, Sqlite>, id: &str) -> Result<Option<Schedule>> {
    Ok(sqlx::query_as::<_, Schedule>("SELECT * FROM schedules WHERE id = ?").bind(id).fetch_optional(&mut **tx).await?)
}

/// 建立的冪等：同一個專案內同一個 client_request_id。
pub async fn find_by_create_key_tx(tx: &mut Transaction<'_, Sqlite>, project_id: &str, crid: &str) -> Result<Option<Schedule>> {
    Ok(sqlx::query_as::<_, Schedule>("SELECT * FROM schedules WHERE project_id = ? AND create_crid = ?")
        .bind(project_id)
        .bind(crid)
        .fetch_optional(&mut **tx)
        .await?)
}

/// 專案 menu 用：未軟刪的排程，依建立時間排。
pub async fn list_schedules(pool: &SqlitePool, project_id: &str) -> Result<Vec<Schedule>> {
    Ok(sqlx::query_as::<_, Schedule>("SELECT * FROM schedules WHERE project_id = ? AND deleted_at IS NULL ORDER BY created_at, id")
        .bind(project_id)
        .fetch_all(pool)
        .await?)
}

/// runner 用：到期、啟用、未軟刪、還有下一次的排程。
pub async fn list_due(pool: &SqlitePool, now: &str) -> Result<Vec<Schedule>> {
    Ok(sqlx::query_as::<_, Schedule>(
        "SELECT * FROM schedules WHERE enabled = 1 AND deleted_at IS NULL AND next_due IS NOT NULL AND next_due <= ? ORDER BY next_due, id",
    )
    .bind(now)
    .fetch_all(pool)
    .await?)
}

/// 目前未結束的 occurrence（同一排程最多一筆）。
pub async fn open_occurrence_tx(tx: &mut Transaction<'_, Sqlite>, schedule_id: &str) -> Result<Option<Occurrence>> {
    Ok(sqlx::query_as::<_, Occurrence>("SELECT * FROM schedule_occurrences WHERE schedule_id = ? AND status IN ('pending','queued') LIMIT 1")
        .bind(schedule_id)
        .fetch_optional(&mut **tx)
        .await?)
}

pub async fn open_occurrence(pool: &SqlitePool, schedule_id: &str) -> Result<Option<Occurrence>> {
    Ok(sqlx::query_as::<_, Occurrence>("SELECT * FROM schedule_occurrences WHERE schedule_id = ? AND status IN ('pending','queued') LIMIT 1")
        .bind(schedule_id)
        .fetch_optional(pool)
        .await?)
}

pub async fn open_occurrences(pool: &SqlitePool) -> Result<Vec<Occurrence>> {
    Ok(sqlx::query_as::<_, Occurrence>("SELECT * FROM schedule_occurrences WHERE status IN ('pending','queued') ORDER BY created_at, id")
        .fetch_all(pool)
        .await?)
}

pub async fn get_occurrence(pool: &SqlitePool, id: &str) -> Result<Option<Occurrence>> {
    Ok(sqlx::query_as::<_, Occurrence>("SELECT * FROM schedule_occurrences WHERE id = ?").bind(id).fetch_optional(pool).await?)
}

pub async fn get_occurrence_tx(tx: &mut Transaction<'_, Sqlite>, id: &str) -> Result<Option<Occurrence>> {
    Ok(sqlx::query_as::<_, Occurrence>("SELECT * FROM schedule_occurrences WHERE id = ?").bind(id).fetch_optional(&mut **tx).await?)
}

pub async fn insert_occurrence_tx(tx: &mut Transaction<'_, Sqlite>, o: &Occurrence) -> Result<()> {
    sqlx::query(
        "INSERT INTO schedule_occurrences (id, schedule_id, source, scheduled_for, generation, bot_id, prompt, slot_label, client_request_id, status, merged_count, created_at, updated_at)
         VALUES (?,?,?,?,?,?,?,?,?,'pending',0,?,?)",
    )
    .bind(&o.id)
    .bind(&o.schedule_id)
    .bind(&o.source)
    .bind(&o.scheduled_for)
    .bind(o.generation)
    .bind(&o.bot_id)
    .bind(&o.prompt)
    .bind(&o.slot_label)
    .bind(&o.client_request_id)
    .bind(&o.created_at)
    .bind(&o.updated_at)
    .execute(&mut **tx)
    .await
    .context("insert schedule occurrence")?;
    Ok(())
}

/// 到期時 occurrence 還沒結束：不另開新的，只記一次合併。
pub async fn merge_into_open_tx(tx: &mut Transaction<'_, Sqlite>, occurrence_id: &str, now: &str) -> Result<()> {
    sqlx::query("UPDATE schedule_occurrences SET merged_count = merged_count + 1, updated_at = ? WHERE id = ?")
        .bind(now)
        .bind(occurrence_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub async fn set_next_due_tx(tx: &mut Transaction<'_, Sqlite>, schedule_id: &str, next: Option<&str>, now: &str) -> Result<()> {
    sqlx::query("UPDATE schedules SET next_due = ?, updated_at = ? WHERE id = ?")
        .bind(next)
        .bind(now)
        .bind(schedule_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// 綁定 turn：與 turn／message 寫入同一個交易（呼叫端負責 begin／commit）。
pub async fn bind_queued_tx(tx: &mut Transaction<'_, Sqlite>, occurrence_id: &str, conversation_id: &str, turn_id: &str, now: &str) -> Result<()> {
    let done = sqlx::query(
        "UPDATE schedule_occurrences SET status = 'queued', conversation_id = ?, turn_id = ?, waiting_reason = NULL, next_retry_at = NULL, outcome = NULL, outcome_summary = NULL, reply_message_id = NULL, error = NULL, updated_at = ?
          WHERE id = ? AND status = 'pending'",
    )
    .bind(conversation_id)
    .bind(turn_id)
    .bind(now)
    .bind(occurrence_id)
    .execute(&mut **tx)
    .await?;
    anyhow::ensure!(done.rows_affected() == 1, "occurrence {occurrence_id} 已不是 pending，不能綁定 turn");
    Ok(())
}

/// 記下等待原因（pending 時）。回傳是否真的改了（只有改了才需要推 WS）。
pub async fn set_waiting_reason(pool: &SqlitePool, occurrence_id: &str, reason: Option<&str>, now: &str) -> Result<bool> {
    let done = sqlx::query("UPDATE schedule_occurrences SET waiting_reason = ?, updated_at = ? WHERE id = ? AND status = 'pending' AND waiting_reason IS NOT ?")
        .bind(reason)
        .bind(now)
        .bind(occurrence_id)
        .bind(reason)
        .execute(pool)
        .await?;
    Ok(done.rows_affected() == 1)
}

/// 把 pending／queued 的 occurrence 標成終態（例如 Bot 不合格、取消）。只動未結束的。
pub async fn finish_open_tx(tx: &mut Transaction<'_, Sqlite>, occurrence_id: &str, status: &str, why: &str, now: &str) -> Result<()> {
    sqlx::query("UPDATE schedule_occurrences SET status = ?, error = ?, waiting_reason = NULL, outcome = CASE WHEN ? = 'failed' THEN 'failed' ELSE 'unknown' END, outcome_summary = ?, updated_at = ? WHERE id = ? AND status IN ('pending','queued')")
        .bind(status)
        .bind(why)
        .bind(status)
        .bind(super::outcome::summary(why))
        .bind(now)
        .bind(occurrence_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// 終態同步：queued 的 occurrence 依綁定 turn 的狀態收斂（見檔頭）。回傳這次剛發生的終態轉移（每筆只一次，靠 notified 去重）。
#[cfg(test)]
pub async fn settle_queued(pool: &SqlitePool, now: &str) -> Result<Vec<Transition>> {
    settle_with_model(pool, now, &super::outcome::Rules).await
}

/// Classify outside the write transaction; the exact turn/attempt is revalidated before settling.
pub(super) fn settle_with_model<'a>(pool: &'a SqlitePool, now: &'a str, model: &'a impl super::outcome::Model) -> impl std::future::Future<Output = Result<Vec<Transition>>> + Send + 'a {
    Box::pin(settle_body(pool, now, model))
}

async fn settle_body(pool: &SqlitePool, now: &str, model: &impl super::outcome::Model) -> Result<Vec<Transition>> {
    let rows = sqlx::query_as::<_, Occurrence>("SELECT * FROM schedule_occurrences WHERE status = 'queued'").fetch_all(pool).await?;
    let mut out = Vec::new();
    for occ in rows {
        let turn: Option<(String, String)> = match &occ.turn_id {
            Some(id) => sqlx::query_as("SELECT status, delivery FROM turns WHERE id = ?").bind(id).fetch_optional(pool).await?,
            None => None,
        };
        let (next, error) = match turn.as_ref().map(|t| (t.0.as_str(), t.1.as_str())) {
            None => ("cancelled", Some("綁定的 turn 已不存在")),
            Some(("completed" | "completed_fallback", "ok")) => ("done", None),
            Some(("completed" | "completed_fallback", _)) | Some(("failed", "unknown")) => ("unknown", Some("送出結果不明，請人工確認；不會自動重送")),
            Some(("failed", _)) => ("failed", Some("回合失敗或已撤回")),
            _ => continue,
        };
        let reply: Option<(String, String)> = sqlx::query_as("SELECT id, content FROM messages WHERE turn_id = ? AND role = 'assistant' AND rewound_at IS NULL ORDER BY created_at DESC, id DESC LIMIT 1")
            .bind(&occ.turn_id).fetch_optional(pool).await?;
        let outcome = if next == "done" || (next == "failed" && reply.is_some()) { super::outcome::classify(model, reply.as_ref().map_or("", |r| r.1.as_str())).await }
            else if next == "failed" { "failed".into() } else { "unknown".into() };
        let dry_run = occ.source == "dry_run";
        let summary = super::outcome::summary(reply.as_ref().map_or(error.unwrap_or(""), |r| r.1.as_str()));
        let mut tx = crate::db::begin_write(pool).await?;
        let Some(cur) = get_occurrence_tx(&mut tx, &occ.id).await?.filter(|o| o.status == "queued" && o.turn_id == occ.turn_id && o.attempt_no == occ.attempt_no) else { continue };
        let Some(sched) = get_schedule_tx(&mut tx, &occ.schedule_id).await? else { continue };
        // Turn evidence may have changed while awaiting the classifier. Retry next tick, never guess.
        let current: Option<(String, String)> = sqlx::query_as("SELECT status, delivery FROM turns WHERE id = ?").bind(&occ.turn_id).fetch_optional(&mut *tx).await?;
        if current != turn { continue; }
        let current_reply: Option<(String, String)> = sqlx::query_as("SELECT id, content FROM messages WHERE turn_id = ? AND role = 'assistant' AND rewound_at IS NULL ORDER BY created_at DESC, id DESC LIMIT 1").bind(&occ.turn_id).fetch_optional(&mut *tx).await?;
        if current_reply != reply { continue; }
        let last = reply.as_ref().map(|r| &r.0);
        let retry = retry_at(&sched, &cur, &outcome, now);
        sqlx::query("INSERT INTO schedule_attempts (occurrence_id, attempt_no, turn_id, reply_message_id, outcome, outcome_summary, finished_at) VALUES (?,?,?,?,?,?,?)")
            .bind(&occ.id).bind(occ.attempt_no).bind(&occ.turn_id).bind(&last).bind(&outcome).bind(&summary).bind(now).execute(&mut *tx).await?;
        sqlx::query("UPDATE schedule_occurrences SET status = ?, error = ?, waiting_reason = NULL, notified = ?, outcome = ?, outcome_summary = ?, reply_message_id = ?, next_retry_at = ?, attempt_no = ?, turn_id = CASE WHEN ? IS NOT NULL THEN NULL ELSE turn_id END, updated_at = ? WHERE id = ?")
            .bind(if retry.is_some() { "pending" } else { next }).bind(error).bind(i64::from(retry.is_none())).bind(&outcome).bind(&summary).bind(if retry.is_some() { None } else { last })
            .bind(&retry).bind(occ.attempt_no + i64::from(retry.is_some())).bind(&retry).bind(now).bind(&occ.id).execute(&mut *tx).await?;
        if next == "unknown" { sqlx::query("UPDATE schedules SET enabled = 0, updated_at = ? WHERE id = ?").bind(now).bind(&sched.id).execute(&mut *tx).await?; }
        tx.commit().await?;
        out.push(Transition { schedule_id: sched.id, schedule_name: sched.name, project_id: sched.project_id, occurrence_id: occ.id,
            status: next.into(), notify: if dry_run { "none".into() } else { sched.notify }, error: error.map(str::to_owned), bot_id: occ.bot_id,
            clear_after: sched.clear_after == 1 && retry.is_none(), outcome, attempt_no: occ.attempt_no });
    }
    Ok(out)
}

/// Only known failures retry. Deadline is measured from the original scheduled time, not each retry.
pub(super) fn retry_at(s: &Schedule, o: &Occurrence, outcome: &str, now: &str) -> Option<String> {
    // 試跑不重試：只回結果，不排下一次（P1 第 2 部分）。
    if o.source == "dry_run" { return None; }
    use chrono::{DateTime, Duration, SecondsFormat};
    if outcome != "failed" || s.enabled != 1 || s.deleted_at.is_some() || s.generation != o.generation || o.attempt_no > s.retry_count { return None; }
    let due = DateTime::parse_from_rfc3339(&o.scheduled_for).ok()?;
    let now = DateTime::parse_from_rfc3339(now).ok()?;
    let retry = now.checked_add_signed(Duration::minutes(s.retry_interval_mins))?;
    let deadline = due.checked_add_signed(Duration::minutes(s.retry_deadline_mins))?;
    if retry.timestamp_millis() > deadline.timestamp_millis() {
        return None;
    }
    Some(retry.to_rfc3339_opts(SecondsFormat::Millis, true))
}

/// 列表／詳情用：occurrence 加上綁定 turn 的真實狀態與 delivery，UI 才能分辨等待、執行中、需確認。
pub async fn occurrence_view(pool: &SqlitePool, occ: &Occurrence) -> Result<serde_json::Value> {
    let turn: Option<(String, String, Option<String>, i64)> = match occ.turn_id.as_deref() {
        Some(t) => sqlx::query_as("SELECT status, delivery, start_error, awaits_start FROM turns WHERE id = ?").bind(t).fetch_optional(pool).await?,
        None => None,
    };
    let mut v = serde_json::to_value(occ)?;
    let attempts: Vec<(i64, Option<String>, Option<String>, String, String)> = sqlx::query_as("SELECT attempt_no, turn_id, reply_message_id, outcome, outcome_summary FROM schedule_attempts WHERE occurrence_id = ? ORDER BY attempt_no")
        .bind(&occ.id).fetch_all(pool).await?;
    v["attempts"] = serde_json::json!(attempts.into_iter().map(|(attempt_no,turn_id,reply_message_id,outcome,outcome_summary)| serde_json::json!({"attempt_no":attempt_no,"turn_id":turn_id,"reply_message_id":reply_message_id,"outcome":outcome,"outcome_summary":outcome_summary})).collect::<Vec<_>>());
    v["turn_status"] = serde_json::json!(turn.as_ref().map(|t| t.0.clone()));
    v["turn_delivery"] = serde_json::json!(turn.as_ref().map(|t| t.1.clone()));
    v["turn_start_error"] = serde_json::json!(turn.as_ref().and_then(|t| t.2.clone()));
    v["turn_awaits_start"] = serde_json::json!(turn.as_ref().map(|t| t.3));
    Ok(v)
}

pub async fn history(pool: &SqlitePool, schedule_id: &str, limit: i64, offset: i64) -> Result<Vec<Occurrence>> {
    Ok(sqlx::query_as::<_, Occurrence>(
        "SELECT * FROM schedule_occurrences WHERE schedule_id = ? ORDER BY created_at DESC, id DESC LIMIT ? OFFSET ?",
    )
    .bind(schedule_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?)
}

/// 手動執行 key 的 mapping（同一交易內查：重送時回原來綁的那一筆）。
pub async fn find_request_tx(tx: &mut Transaction<'_, Sqlite>, schedule_id: &str, crid: &str) -> Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as("SELECT occurrence_id FROM schedule_manual_requests WHERE schedule_id = ? AND client_request_id = ?")
        .bind(schedule_id)
        .bind(crid)
        .fetch_optional(&mut **tx)
        .await?;
    Ok(row.map(|(o,)| o))
}

/// 記下 key → 實際的 occurrence（不論合併進哪一筆，同一交易內寫）。
pub async fn bind_request_tx(tx: &mut Transaction<'_, Sqlite>, schedule_id: &str, crid: &str, occurrence_id: &str, now: &str) -> Result<()> {
    sqlx::query("INSERT INTO schedule_manual_requests (schedule_id, client_request_id, occurrence_id, created_at) VALUES (?,?,?,?)")
        .bind(schedule_id)
        .bind(crid)
        .bind(occurrence_id)
        .bind(now)
        .execute(&mut **tx)
        .await
        .context("bind manual request")?;
    Ok(())
}

/// 試跑與正式手動執行分表，互不佔用冪等鍵。
pub async fn find_request_kind_tx(tx: &mut Transaction<'_, Sqlite>, schedule_id: &str, crid: &str, dry: bool) -> Result<Option<String>> {
    if !dry { return find_request_tx(tx, schedule_id, crid).await; }
    Ok(sqlx::query_scalar("SELECT occurrence_id FROM schedule_dry_run_requests WHERE schedule_id = ? AND client_request_id = ?")
        .bind(schedule_id).bind(crid).fetch_optional(&mut **tx).await?)
}
pub async fn bind_request_kind_tx(tx: &mut Transaction<'_, Sqlite>, schedule_id: &str, crid: &str, occurrence_id: &str, now: &str, dry: bool) -> Result<()> {
    if !dry { return bind_request_tx(tx, schedule_id, crid, occurrence_id, now).await; }
    sqlx::query("INSERT INTO schedule_dry_run_requests (schedule_id, client_request_id, occurrence_id, created_at) VALUES (?,?,?,?)")
        .bind(schedule_id).bind(crid).bind(occurrence_id).bind(now).execute(&mut **tx).await?;
    Ok(())
}
