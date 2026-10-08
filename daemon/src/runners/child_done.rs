use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};
use crate::child_done::{has_recent_near_duplicate, message_for, truncate, CRID_PREFIX, MAX_REPLY_CHARS};
use crate::events::ports::TurnCommands;
use crate::state::App;

fn notification_lock() -> &'static tokio::sync::Mutex<()> {
    static V: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    V.get_or_init(Default::default)
}

fn working() -> &'static Mutex<HashSet<String>> {
    static V: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    V.get_or_init(Default::default)
}

struct Working(String);

impl Working {
    fn start(turn_id: &str) -> Option<Self> {
        working()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(turn_id.to_string())
            .then(|| Self(turn_id.to_string()))
    }
}

impl Drop for Working {
    fn drop(&mut self) {
        working()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}

/// 終端擷取（`completed_fallback`）收下的回合，先等這麼久再通知，讓「剛開出來、只回了開場白」的子 agent 有時間繼續動起來（#878）。
const FALLBACK_SETTLE: std::time::Duration = std::time::Duration::from_secs(20);

/// 通知被佇列收成 failed（字沒打進去）之後，隔多久才補送下一次（#874）。
const CHILD_DONE_RETRY_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(120);

/// 同一個來源回合最多補送幾次（第 0 次之外）。
const CHILD_DONE_RETRY_LIMIT: u32 = 3;

/// Start a best-effort immediate notification after a completed turn has been committed.
pub fn on_completed_turn(app: &Arc<App>, turn_id: &str) {
    if cfg!(test) {
        return;
    }
    let Some(working) = Working::start(turn_id) else {
        return;
    };
    let (app, turn_id) = (app.clone(), turn_id.to_string());
    tokio::spawn(async move {
        let _working = working;
        // 畫面備援收的回合不一定是結尾：先等它安定，notify_turn 再看子 agent 是不是又在忙。
        let fallback: Option<String> = sqlx::query_scalar("SELECT status FROM turns WHERE id = ? AND status = 'completed_fallback'")
            .bind(&turn_id)
            .fetch_optional(&app.db)
            .await
            .ok()
            .flatten();
        if fallback.is_some() {
            tokio::time::sleep(FALLBACK_SETTLE).await;
        }
        if let Err(e) = notify_turn(&app, &turn_id).await {
            tracing::warn!(turn = %turn_id, error = ?e, "could not notify parent about completed child turn; sweep will retry");
        }
    });
}

/// Notify all completed child turns that have no durable parent notice yet.
pub async fn sweep(app: &Arc<App>) -> usize {
    let candidates: Vec<String> = match sqlx::query_scalar(
        "SELECT t.id
           FROM turns t
           JOIN conversations c ON c.id = t.conversation_id
           JOIN bots b ON b.id = c.bot_id
          WHERE t.status IN ('completed','completed_fallback')
            AND t.completed_at IS NOT NULL
            AND julianday(t.completed_at) >= julianday('now', '-1 hour')
            AND b.managed_by = 'child' AND b.deleted_at IS NULL
            AND TRIM(COALESCE(b.parent_bot_id, '')) <> ''
            AND EXISTS (
                SELECT 1 FROM runs pr
                 WHERE pr.bot_id = b.parent_bot_id
                   AND pr.state IN ('starting','running','stopping')
            )
            AND EXISTS (SELECT 1 FROM messages a WHERE a.turn_id = t.id AND a.role = 'assistant')
            AND NOT EXISTS (
                SELECT 1 FROM turns sent
                  JOIN conversations pc ON pc.id = sent.conversation_id
                 WHERE pc.bot_id = b.parent_bot_id
                   AND (sent.client_request_id = 'child-done:' || b.id || ':' || t.id
                        OR substr(sent.client_request_id, 1, length('child-done:' || b.id || ':' || t.id || ':r'))
                           = 'child-done:' || b.id || ':' || t.id || ':r')
                   AND NOT (sent.status = 'failed' AND sent.delivery = 'failed')
            )
            -- 這顆 child 較新的回合已經有（不是 undelivered 的）通知：舊的由 notify_turn 的 `superseded` 擋掉，不必每輪撈出來。
            AND NOT EXISTS (
                SELECT 1 FROM turns sent
                  JOIN conversations pc ON pc.id = sent.conversation_id
                  JOIN turns source ON source.id = CASE
                         WHEN instr(substr(sent.client_request_id, length('child-done:' || b.id || ':') + 1), ':') > 0
                         THEN substr(substr(sent.client_request_id, length('child-done:' || b.id || ':') + 1), 1,
                                     instr(substr(sent.client_request_id, length('child-done:' || b.id || ':') + 1), ':') - 1)
                         ELSE substr(sent.client_request_id, length('child-done:' || b.id || ':') + 1)
                       END
                 WHERE pc.bot_id = b.parent_bot_id
                   AND substr(sent.client_request_id, 1, length('child-done:' || b.id || ':')) = 'child-done:' || b.id || ':'
                   AND NOT (sent.status = 'failed' AND sent.delivery = 'failed')
                   AND (source.completed_at > t.completed_at
                        OR (source.completed_at = t.completed_at AND source.rowid > t.rowid))
            )
            AND NOT EXISTS (
                SELECT 1 FROM messages reported
                  JOIN conversations pc ON pc.id = reported.conversation_id
                 WHERE pc.bot_id = b.parent_bot_id
                   AND reported.role = 'user' AND reported.relay_from = b.id
                   -- daemon 自己送的 child-done 通知也帶 relay_from：不算 child 自己回報（否則補送會被前一次的通知擋掉，#874）
                   AND NOT EXISTS (SELECT 1 FROM turns nt WHERE nt.id = reported.turn_id
                                    AND substr(nt.client_request_id, 1, length('child-done:' || b.id || ':')) = 'child-done:' || b.id || ':')
                   AND julianday(reported.created_at) >= julianday(t.created_at)
                   AND julianday(reported.created_at) <= julianday(t.completed_at) + (5.0 / 1440.0)
            )
          ORDER BY t.completed_at DESC, t.rowid DESC",
    )
    .fetch_all(&app.db)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = ?e, "child done sweep: could not list completed child turns; next round");
            return 0;
        }
    };

    let mut started = 0;
    for turn_id in candidates {
        let Some(working) = Working::start(&turn_id) else {
            continue;
        };
        started += 1;
        if let Err(e) = notify_turn(app, &turn_id).await {
            tracing::warn!(turn = %turn_id, error = ?e, "child done sweep notification failed");
        }
        drop(working);
    }
    started
}

#[derive(sqlx::FromRow, Debug)]
struct CompletedTurn {
    id: String,
    status: String,
    child_id: String,
    child_name: String,
    parent_id: String,
    started_at: String,
    completed_at: String,
    run_id: Option<String>,
    kind: String,
    reply: String,
}

pub(crate) async fn notify_turn(app: &Arc<App>, turn_id: &str) -> anyhow::Result<()> {
    let _notification_guard = notification_lock().lock().await;
    let row: Option<CompletedTurn> = sqlx::query_as(
        "SELECT t.id, t.status, b.id AS child_id, b.name AS child_name, b.parent_bot_id AS parent_id,
                t.created_at AS started_at, t.completed_at, t.run_id, b.kind,
                (SELECT a.content FROM messages a
                  WHERE a.turn_id = t.id AND a.role = 'assistant'
                  ORDER BY a.created_at DESC, a.rowid DESC LIMIT 1) AS reply
           FROM turns t
           JOIN conversations c ON c.id = t.conversation_id
           JOIN bots b ON b.id = c.bot_id
          WHERE t.id = ?
            AND t.status IN ('completed','completed_fallback')
            AND t.completed_at IS NOT NULL
            AND b.managed_by = 'child' AND b.deleted_at IS NULL
            AND TRIM(COALESCE(b.parent_bot_id, '')) <> ''
            AND EXISTS (SELECT 1 FROM messages a WHERE a.turn_id = t.id AND a.role = 'assistant')",
    )
    .bind(turn_id)
    .fetch_optional(&app.db)
    .await?;
    let Some(turn) = row else { return Ok(()) };
    if turn.reply.trim().is_empty() {
        return Ok(());
    }
    // 終端擷取的回合不保證是結尾（剛開出來只回了開場白、工作還在跑）：子 agent 還在忙就先不報，等它真的停下來
    // （sweep 會再來；那時更新的回合已經在了，舊的由 `superseded` 擋掉）。
    if turn.status == "completed_fallback" && child_still_active(app, turn.run_id.as_deref()).await {
        tracing::debug!(child = %turn.child_name, turn = %turn.id, "deferring a terminal-fallback completion notice: the child is still working");
        return Ok(());
    }

    let base_crid = format!("{CRID_PREFIX}{}:{}", turn.child_id, turn.id);
    let parent_conversation = crate::db::conversation_id(&app.db, &turn.parent_id).await?;
    // 這個來源回合在 parent 那邊最後一則通知怎麼了（#874）：字沒打進去就收成 failed 的，冷卻過後以 `:r<n>` 補送；
    // 送到、結果不明、還在路上、使用者撤回都不重送。全部從 DB 推導，重啟後照舊。
    let client_request_id = match crate::child_alerts::last_sent(app, &turn.parent_id, &base_crid).await? {
        crate::child_alerts::Sent::Never => base_crid.clone(),
        crate::child_alerts::Sent::Undelivered { attempt, at } => {
            let cooled = chrono::Utc::now()
                .signed_duration_since(at)
                .to_std()
                .is_ok_and(|d| d >= CHILD_DONE_RETRY_COOLDOWN);
            if !cooled || attempt >= CHILD_DONE_RETRY_LIMIT {
                return Ok(());
            }
            format!("{base_crid}:r{}", attempt + 1)
        }
        crate::child_alerts::Sent::Outstanding
        | crate::child_alerts::Sent::Delivered
        | crate::child_alerts::Sent::Withdrawn => return Ok(()),
    };

    let prefix = format!("{CRID_PREFIX}{}:", turn.child_id);
    let superseded: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM turns sent
             JOIN turns source ON source.id = CASE
                    WHEN instr(substr(sent.client_request_id, length(?) + 1), ':') > 0
                    THEN substr(substr(sent.client_request_id, length(?) + 1), 1,
                                instr(substr(sent.client_request_id, length(?) + 1), ':') - 1)
                    ELSE substr(sent.client_request_id, length(?) + 1)
                  END
            WHERE sent.conversation_id = ?
              AND substr(sent.client_request_id, 1, length(?)) = ?
              AND NOT (sent.status = 'failed' AND sent.delivery = 'failed')
              AND (source.completed_at > ? OR (
                    source.completed_at = ?
                    AND source.rowid > (SELECT rowid FROM turns WHERE id = ?)
              ))
        )",
    )
    .bind(&prefix)
    .bind(&prefix)
    .bind(&prefix)
    .bind(&prefix)
    .bind(&parent_conversation)
    .bind(&prefix)
    .bind(&prefix)
    .bind(&turn.completed_at)
    .bind(&turn.completed_at)
    .bind(&turn.id)
    .fetch_one(&app.db)
    .await?;
    if superseded {
        return Ok(());
    }

    let child_already_reported: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM messages m
             WHERE m.conversation_id = ? AND m.role = 'user' AND m.relay_from = ?
               AND NOT EXISTS (SELECT 1 FROM turns nt WHERE nt.id = m.turn_id
                                AND substr(nt.client_request_id, 1, length(?)) = ?)
               AND julianday(m.created_at) >= julianday(?)
               AND julianday(m.created_at) <= julianday(?) + (5.0 / 1440.0)
        )",
    )
    .bind(&parent_conversation)
    .bind(&turn.child_id)
    .bind(&prefix)
    .bind(&prefix)
    .bind(&turn.started_at)
    .bind(&turn.completed_at)
    .fetch_one(&app.db)
    .await?;
    if child_already_reported {
        return Ok(());
    }

    if crate::db::active_run(&app.db, &turn.parent_id)
        .await?
        .is_none()
    {
        return Ok(());
    }

    if background_work_running(app, turn.run_id.as_deref(), &turn.kind).await {
        tracing::debug!(child = %turn.child_name, turn = %turn.id, "deferring child completion notice while background work is active");
        return Ok(());
    }

    let quoted_reply = truncate(turn.reply.trim(), MAX_REPLY_CHARS);
    if has_recent_near_duplicate(
        &app.db,
        &parent_conversation,
        &prefix,
        &base_crid,
        &quoted_reply,
    )
    .await?
    {
        tracing::debug!(child = %turn.child_name, turn = %turn.id, "suppressing near-duplicate child completion notice");
        return Ok(());
    }

    let message = message_for(&turn.child_name, &quoted_reply);
    match app
        .prompt_relayed_queueable(&turn.parent_id, &message, &client_request_id, Some(&turn.child_id))
        .await
    {
        Ok(out) => {
            tracing::info!(child = %turn.child_name, parent = %turn.parent_id, turn = %turn.id, delivery = %out.delivery, "notified parent about completed child turn");
            Ok(())
        }
        Err(e) => Err(anyhow::anyhow!("queueable parent prompt failed: {e:?}")),
    }
}

/// 這顆子 agent 的 run 現在還在忙：herdr 狀態是 working／blocked，或畫面上還有進行中的動詞列。讀不到＝當作沒在忙（不擋通知）。
async fn child_still_active(app: &Arc<App>, run_id: Option<&str>) -> bool {
    let Some(run_id) = run_id else { return false };
    let Ok(Some(run)) = crate::db::run(&app.db, run_id).await else { return false };
    if run.state != "running" {
        return false;
    }
    if matches!(run.agent_status.as_str(), "working" | "blocked") {
        return true;
    }
    let (Some(pane), Some(client)) = (run.pane_id.as_deref(), app.herdr_for_run(&run).await) else { return false };
    match client.pane_read(pane, "visible", 60).await {
        Ok(read) => crate::lifecycle::poller::pane_still_busy(&read.text),
        Err(_) => false,
    }
}

async fn background_work_running(app: &Arc<App>, run_id: Option<&str>, kind: &str) -> bool {
    let Some(run_id) = run_id else { return false };
    if let Some(n) = crate::background_jobs::known(app, run_id) {
        return n > 0;
    }
    let Ok(Some(run)) = crate::db::run(&app.db, run_id).await else {
        return false;
    };
    crate::runners::background_jobs::refresh(app, &run, kind).await;
    crate::background_jobs::known(app, run_id).is_some_and(|n| n > 0)
}
