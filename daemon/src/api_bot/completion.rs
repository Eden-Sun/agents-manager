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

/// Only decide whether this old recovery actor should retire. This is NOT commit/join proof.
/// A completed turn can still need its Applied receipt acknowledgment and cursor publication.
pub(super) async fn completion_owner_is_current_result(
    app: &Arc<App>,
    key: &ApiTurnKey,
    identity: &AdapterIdentity,
) -> Result<bool, StoreError> {
    if key.bot_id != identity.bot_id
        || key.run_id != identity.run_id
        || key.turn_id != identity.turn_id
    {
        return Ok(false);
    }
    let expiry = crate::db::ts_sql("s.expires_at");
    sqlx::query_scalar(&format!("SELECT EXISTS(
        SELECT 1 FROM bots b JOIN runs r ON r.bot_id=b.id
        JOIN turns t ON t.run_id=r.id JOIN conversations c ON c.id=t.conversation_id AND c.bot_id=b.id
        JOIN api_bot_sessions s ON s.bot_id=b.id AND s.owner_run_id=r.id AND s.owner_turn_id=t.id
        WHERE b.id=? AND r.id=? AND t.id=? AND s.id=? AND s.generation=?
          AND b.kind='claude-api' AND b.deleted_at IS NULL AND r.state='running'
          AND t.status IN ('in_flight','completed','failed') AND t.delivery='ok'
          AND s.retired_at IS NULL AND {expiry}>?
          AND NOT EXISTS(SELECT 1 FROM runs successor WHERE successor.bot_id=b.id AND successor.id!=r.id AND successor.state IN ('starting','running','stopping'))
    )"))
        .bind(&key.bot_id).bind(&key.run_id).bind(&key.turn_id).bind(&identity.session_id).bind(identity.generation).bind(crate::db::now())
        .fetch_one(&app.db).await.map_err(|_| StoreError::Retryable)
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
        State::Fenced => Ok(Probe::Stale(item)),
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

pub(super) fn format_http_failure_reason(
    status: u16,
    kind: Option<&str>,
    message: Option<&str>,
    request_id: Option<&str>,
) -> String {
    if kind.is_none() && message.is_none() && request_id.is_none() {
        format!("api_http_{status}")
    } else {
        let payload = serde_json::json!({
            "type": kind,
            "message": message,
            "request_id": request_id,
        });
        format!("api_http_{status}:{payload}")
    }
}

/// 失敗原因的人話；若為 HTTP 錯誤且帶有詳細資訊則帶出 type/message/request-id。
pub(super) fn failure_explanation(reason: &str) -> std::borrow::Cow<'static, str> {
    if let Some(rest) = reason.strip_prefix("api_total_cap_reached:cap=") {
        if let Some((cap, spent)) = rest.split_once(":spent=") {
            return format!("已達總花費上限 ${cap}（已花 ${spent}）").into();
        }
    }
    if let Some(rest) = reason.strip_prefix("api_http_") {
        let (status_str, json_str) = match rest.split_once(':') {
            Some((s, j)) => (s, Some(j)),
            None => (rest, None),
        };
        let status = status_str.parse::<u16>().ok();
        if let Some(json_str) = json_str {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(json_str) {
                let kind = v.get("type").and_then(serde_json::Value::as_str);
                let message = v.get("message").and_then(serde_json::Value::as_str);
                let request_id = v.get("request_id").and_then(serde_json::Value::as_str);
                let mut parts = Vec::new();
                if let Some(k) = kind {
                    parts.push(k.to_string());
                }
                if let Some(m) = message {
                    parts.push(m.to_string());
                }
                if let Some(rid) = request_id {
                    parts.push(format!("request-id: {rid}"));
                }
                if !parts.is_empty() {
                    let st = status.map(|s| format!("（HTTP {s}）")).unwrap_or_default();
                    return format!("Anthropic API 回了錯誤狀態碼{st}：{}。", parts.join(" - ")).into();
                }
            }
        }
        if let Some(s) = status {
            return format!("Anthropic API 回了錯誤狀態碼（HTTP {s}）。").into();
        }
        return "Anthropic API 回了錯誤狀態碼。".into();
    }
    match reason {
        "api_total_cap_reached" => "已達總花費上限。".into(),
        "api_bot_turn_cost_cap_exceeded" => "這一回合累計花費加上下一次請求的預留額，會超過單回合成本上限（max_turn_cost_microusd），所以沒有送出請求。".into(),
        "api_bot_hourly_limit_reached" => "過去一小時的請求數已達上限（max_requests_per_hour），所以沒有送出請求。".into(),
        "api_bot_key_unavailable" => "讀不到 API key（[claude_api] key_file），所以沒有送出請求。".into(),
        "api_bot_quote_refused" => "請求無法估價（超過上下文上限或含未核准的欄位），所以沒有送出請求。".into(),
        "api_bot_max_calls_reached" => "這一回合的 API 呼叫次數已達上限（max_api_calls_per_turn）。".into(),
        "turn_timeout" => "這一回合超過時間上限（turn_timeout_secs）。".into(),
        "max_tokens" => "回覆在輸出 token 上限被截斷。".into(),
        "unknown_stop" => "模型以未知的原因停止。".into(),
        "pending_unknown" => "上一次請求的送達與計費無法確認，為了避免重複計費已停止；請退役這個對話後再試。".into(),
        "journal" => "對話狀態無法寫入，已停止。".into(),
        "invalid_response" | "api_invalid_response" => "收到無法驗證的回應（用量或內容不合規則），已停止。".into(),
        "api_rate_limited" => "Anthropic API 回報請求過於頻繁（429）。".into(),
        "api_timeout" => "Anthropic API 請求逾時。".into(),
        "api_transport_unknown" => "與 Anthropic API 的連線中斷，無法確認請求是否送達。".into(),
        "api_invalid_request" => "請求不合法，沒有送出。".into(),
        r if r.starts_with("api_http_") => "Anthropic API 回了錯誤狀態碼。".into(),
        _ => "原因未知。".into(),
    }
}

/// 失敗的 turn 在對話裡留一則可見的系統訊息（帶 reason），並記 warn。寫不進去只記 log，不影響已提交的失敗結算。
pub(super) async fn post_failure_notice(app: &Arc<App>, key: &ApiTurnKey, reason: &str) {
    tracing::warn!(bot = %key.bot_id, run = %key.run_id, turn = %key.turn_id, reason = %reason, "API bot turn failed");
    let text = if let Some(rest) = reason.strip_prefix("api_total_cap_reached:cap=") {
        if let Some((cap, spent)) = rest.split_once(":spent=") {
            format!("⚠ 這一回合失敗（api_total_cap_reached）：已達總花費上限 ${cap}（已花 ${spent}）")
        } else {
            format!("⚠ 這一回合失敗（api_total_cap_reached）：{}", failure_explanation(reason))
        }
    } else if let Some(rest) = reason.strip_prefix("api_http_") {
        let status = rest.split(':').next().unwrap_or(rest);
        format!("⚠ 這一回合失敗（api_http_{status}）：{}", failure_explanation(reason))
    } else {
        format!("⚠ 這一回合失敗（{reason}）：{}", failure_explanation(reason))
    };
    let posted: Result<Option<crate::db::Message>> = async {
        let mut tx = crate::db::begin_write(&app.db).await?;
        let conversation: Option<String> = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id=? AND run_id=?")
            .bind(&key.turn_id).bind(&key.run_id).fetch_optional(&mut *tx).await?;
        let Some(conversation) = conversation else { return Ok(None) };
        let message = crate::lifecycle::insert_message_tx(&mut tx, &conversation, Some(&key.turn_id), "system", &text, "system", false, None).await?;
        tx.commit().await?;
        Ok(Some(message))
    }.await;
    match posted {
        Ok(Some(message)) => crate::lifecycle::emit_message_added(app, &key.bot_id, message).await,
        Ok(None) => {}
        Err(_) => tracing::warn!(bot = %key.bot_id, turn = %key.turn_id, "API bot failure notice could not be stored"),
    }
}

/// 達到 API 呼叫上限時在對話中留一則可見的系統訊息，通知使用者可回覆 continue 接著做。
pub(super) async fn post_cap_notice(app: &Arc<App>, key: &ApiTurnKey, max_calls: usize) {
    let text = format!("已達本回合 API 呼叫上限（{max_calls} 次），回覆 continue 可接著做。");
    let posted: Result<Option<crate::db::Message>> = async {
        let mut tx = crate::db::begin_write(&app.db).await?;
        let conversation: Option<String> = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id=? AND run_id=?")
            .bind(&key.turn_id).bind(&key.run_id).fetch_optional(&mut *tx).await?;
        let Some(conversation) = conversation else { return Ok(None) };
        let message = crate::lifecycle::insert_message_tx(&mut tx, &conversation, Some(&key.turn_id), "system", &text, "system", false, None).await?;
        tx.commit().await?;
        Ok(Some(message))
    }.await;
    match posted {
        Ok(Some(message)) => crate::lifecycle::emit_message_added(app, &key.bot_id, message).await,
        Ok(None) => {}
        Err(_) => tracing::warn!(bot = %key.bot_id, turn = %key.turn_id, "API bot cap notice could not be stored"),
    }
}
