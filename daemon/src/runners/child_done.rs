use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use futures::{stream, StreamExt};
use crate::child_done::{alert_undeliverable, child_already_reported, message_for_child, notice_reply_limit, record_unsent_attempt, unsent_composer_conflict, CRID_PREFIX, RETRY_LIMIT};
use crate::events::ports::TurnCommands;
use crate::state::App;

/// 同一位 parent 的完成通知（DB 判斷＋送出）串行；不同 parent 互不等（#1033）。
/// 只包住 DB 判斷與送出，畫面讀取、背景工作查詢這類外部 I/O 都在鎖外做。
pub(crate) fn parent_lock(parent_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    static V: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let mut map = V.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    // 沒有人持有、也沒人在等的 parent 順手移掉，表不會隨 bot 數一直長。
    map.retain(|k, m| k == parent_id || Arc::strong_count(m) > 1);
    map.entry(parent_id.to_string()).or_default().clone()
}

/// sweep 同時處理幾位 parent 的完成通知；一位 parent 慢只佔一格，其餘照跑（#1033）。
const SWEEP_PARENTS_AT_ONCE: usize = 8;

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

/// A short settling window for screen fallback, which can capture an opening message.
const FALLBACK_SETTLE_SECS: i64 = 3;
const CHILD_DONE_RETRY_COOLDOWN: i64 = 120;

pub fn on_completed_turn(app: &Arc<App>, turn_id: &str) {
    if cfg!(test) { return; }
    let Some(working) = Working::start(turn_id) else { return };
    let (app, turn_id) = (app.clone(), turn_id.to_string());
    tokio::spawn(Box::pin(async move {
        let _working = working;
        if let Err(e) = notify_turn(&app, &turn_id).await {
            tracing::warn!(turn = %turn_id, error = ?e, "child done initial reconciliation failed");
        }
        tokio::time::sleep(std::time::Duration::from_secs(FALLBACK_SETTLE_SECS as u64)).await;
        if let Err(e) = notify_turn(&app, &turn_id).await {
            tracing::warn!(turn = %turn_id, error = ?e, "child done reconciliation failed; sweep will retry");
        }
    }));
}

/// Revisit children even when their latest notice is queued, failed, or background work changed.
pub async fn sweep(app: &Arc<App>) -> usize {
    let candidates: Vec<(String, String)> = match sqlx::query_as(
        "SELECT COALESCE((SELECT t.id FROM turns t JOIN conversations c ON c.id=t.conversation_id
                     WHERE c.bot_id=b.id AND t.status IN ('completed','completed_fallback','failed') AND t.completed_at IS NOT NULL
                     ORDER BY t.completed_at DESC,t.rowid DESC LIMIT 1),r.id), b.parent_bot_id
         FROM bots b JOIN runs r ON r.bot_id=b.id AND (r.state='running' OR (b.deleted_at IS NOT NULL AND r.rowid=(SELECT max(rowid) FROM runs WHERE bot_id=b.id) AND EXISTS(SELECT 1 FROM intents i WHERE i.kind='retire_child' AND i.subject_id=b.id AND json_extract(i.payload_json,'$.cause')='agent_missing')))
         WHERE b.managed_by='child' AND (b.deleted_at IS NULL OR EXISTS(SELECT 1 FROM intents i WHERE i.kind='retire_child' AND i.subject_id=b.id AND json_extract(i.payload_json,'$.cause')='agent_missing')) AND COALESCE(b.parent_bot_id,'')<>''
        "
    ).fetch_all(&app.db).await {
        Ok(rows) => rows,
        Err(e) => { tracing::warn!(error = ?e, "child done sweep failed"); return 0; }
    };
    let mut by_parent: HashMap<String, Vec<String>> = HashMap::new();
    for (turn, parent) in candidates { by_parent.entry(parent).or_default().push(turn); }
    stream::iter(by_parent.into_values()).map(|turns| async move {
        let mut count = 0;
        for turn in turns {
            let Some(_working) = Working::start(&turn) else { continue };
            count += 1;
            if let Err(e) = notify_turn(app, &turn).await {
                tracing::warn!(turn = %turn, error = ?e, "child done reconciliation failed");
            }
        }
        count
    }).buffer_unordered(SWEEP_PARENTS_AT_ONCE).fold(0, |a,b| async move { a+b }).await
}

#[derive(sqlx::FromRow)]
struct Child {
    child_id: String, child_name: String, parent_id: String, kind: String,
    run_id: String, status_since: Option<String>, gone: bool, cwd: Option<String>, last_status: String,
}

#[derive(sqlx::FromRow)]
struct State {
    covered_rowid: i64, covered_at: Option<String>, pending_turn_id: Option<String>, pending_rowid: Option<i64>, pending_at: Option<String>, pending_idle_at: Option<String>,
    pending_background: Option<i64>, notified_background: i64, notified_at: Option<String>, idle_at: String,
}

#[derive(sqlx::FromRow)]
struct SourceTurn { rowid: i64, id: String, completed_at: String, status: String, delivery: String, reply: Option<String> }

/// A parent's result request owns its child turn until its result was delivered or failed.
/// A verified result delivery can cover the child turn, but never skip unrelated outstanding work.
async fn cover_delivered_relay(app: &Arc<App>, child: &Child, background: bool) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO child_done_sources(child_id,turn_id,covered) SELECT ?,t.id,1 FROM relay_requests r JOIN turns t ON (t.id=r.to_turn_id OR t.client_request_id='relay-request:'||r.id) JOIN conversations c ON c.id=t.conversation_id AND c.bot_id=r.to_bot_id WHERE r.from_bot_id=? AND r.to_bot_id=? AND (r.state='notified' OR (r.state='failed' AND r.notified_at IS NOT NULL)) AND t.status IN ('completed','completed_fallback','failed') ON CONFLICT(child_id,turn_id) DO UPDATE SET covered=1 WHERE child_done_sources.batch IS NULL")
        .bind(&child.child_id).bind(&child.parent_id).bind(&child.child_id).execute(&app.db).await?;
    sqlx::query("INSERT INTO child_done_sources(child_id,turn_id,covered) SELECT ?,t.id,1 FROM relay_requests r JOIN turns t ON t.id=r.notice_turn_id JOIN conversations c ON c.id=t.conversation_id AND c.bot_id=r.from_bot_id WHERE r.from_bot_id=? AND r.to_bot_id=? AND (r.state='notified' OR (r.state='failed' AND r.notified_at IS NOT NULL)) AND t.status IN ('completed','completed_fallback','failed') ON CONFLICT(child_id,turn_id) DO UPDATE SET covered=1 WHERE child_done_sources.batch IS NULL")
        .bind(&child.child_id).bind(&child.child_id).bind(&child.parent_id).execute(&app.db).await?;
    sqlx::query("UPDATE child_done_state SET covered_rowid=(SELECT t.rowid FROM turns t JOIN conversations c ON c.id=t.conversation_id WHERE c.bot_id=? ORDER BY t.rowid DESC LIMIT 1), idle_turn_id=(SELECT t.id FROM turns t JOIN conversations c ON c.id=t.conversation_id WHERE c.bot_id=? ORDER BY t.rowid DESC LIMIT 1), idle_at=COALESCE(?,?), active=0, notified_at=?, notified_background=? WHERE child_id=? AND pending_turn_id IS NULL AND NOT EXISTS(SELECT 1 FROM child_done_sources s WHERE s.child_id=? AND s.covered=0) AND EXISTS(SELECT 1 FROM relay_requests r JOIN turns t ON (t.id=r.to_turn_id OR t.id=r.notice_turn_id) JOIN conversations c ON c.id=t.conversation_id WHERE ((r.from_bot_id=? AND r.to_bot_id=? AND t.id=r.to_turn_id) OR (r.from_bot_id=? AND r.to_bot_id=? AND t.id=r.notice_turn_id)) AND (r.state='notified' OR (r.state='failed' AND r.notified_at IS NOT NULL)) AND t.rowid=(SELECT max(t2.rowid) FROM turns t2 WHERE t2.conversation_id=c.id)) AND NOT EXISTS(SELECT 1 FROM turns t JOIN conversations c ON c.id=t.conversation_id WHERE c.bot_id=? AND t.status IN ('completed','completed_fallback','failed') AND t.rowid>child_done_state.covered_rowid AND NOT EXISTS(SELECT 1 FROM child_done_sources s WHERE s.child_id=c.bot_id AND s.turn_id=t.id AND s.covered=1))")
        .bind(&child.child_id).bind(&child.child_id).bind(&child.status_since).bind(crate::db::now()).bind(crate::db::now()).bind(i64::from(background)).bind(&child.child_id).bind(&child.child_id).bind(&child.parent_id).bind(&child.child_id).bind(&child.child_id).bind(&child.parent_id).bind(&child.child_id).execute(&app.db).await?;
    Ok(())
}

/// Reconcile one child; the turn id is only a lookup hint, not the unit of deduplication.
pub(crate) async fn notify_turn(app: &Arc<App>, turn_id: &str) -> anyhow::Result<()> {
    let child: Option<Child> = sqlx::query_as(
        "SELECT b.id child_id, b.name child_name, b.parent_bot_id parent_id, b.kind, r.id run_id, r.agent_status_since status_since, b.deleted_at IS NOT NULL gone, b.cwd, r.agent_status last_status
         FROM bots b JOIN runs r ON r.bot_id=b.id AND (r.state='running' OR (b.deleted_at IS NOT NULL AND r.rowid=(SELECT max(rowid) FROM runs WHERE bot_id=b.id) AND EXISTS(SELECT 1 FROM intents i WHERE i.kind='retire_child' AND i.subject_id=b.id AND json_extract(i.payload_json,'$.cause')='agent_missing')))
         WHERE (r.id=? OR EXISTS(SELECT 1 FROM turns t JOIN conversations c ON c.id=t.conversation_id WHERE c.bot_id=b.id AND t.id=?))
           AND b.managed_by='child' AND (b.deleted_at IS NULL OR EXISTS(SELECT 1 FROM intents i WHERE i.kind='retire_child' AND i.subject_id=b.id AND json_extract(i.payload_json,'$.cause')='agent_missing')) AND COALESCE(b.parent_bot_id,'')<>''"
    ).bind(turn_id).bind(turn_id).fetch_optional(&app.db).await?;
    let Some(child) = child else { return Ok(()) };
    // Persist the obligation before inspecting the pane, even without a captured turn.
    sqlx::query("INSERT OR IGNORE INTO child_done_state(child_id,idle_at,idle_turn_id,initialized_at) VALUES(?,?,?,?)")
        .bind(&child.child_id).bind(child.status_since.as_deref().unwrap_or(&crate::db::now())).bind(turn_id).bind(crate::db::now()).execute(&app.db).await?;
    // A result notice is not a new request, including when its recipient is this child's parent.
    // Do not turn the notice's reply (or its idle edge) into a child_done echo. Uncovered older
    // child work and already-pending child_done notices still reconcile normally.
    let notice_only: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM turns t JOIN conversations c ON c.id=t.conversation_id WHERE c.bot_id=? AND t.status IN ('completed','completed_fallback','failed') AND t.id=(SELECT t2.id FROM turns t2 WHERE t2.conversation_id=c.id AND t2.status IN ('completed','completed_fallback','failed') ORDER BY t2.completed_at DESC,t2.rowid DESC LIMIT 1) AND t.queue_class='daemon' AND t.client_request_id LIKE 'relay-result:%') AND NOT EXISTS(SELECT 1 FROM turns t JOIN conversations c ON c.id=t.conversation_id JOIN child_done_state d ON d.child_id=c.bot_id WHERE c.bot_id=? AND t.status IN ('completed','completed_fallback','failed') AND NOT (t.queue_class='daemon' AND COALESCE(t.client_request_id,'') LIKE 'relay-result:%') AND t.rowid>d.covered_rowid AND NOT EXISTS(SELECT 1 FROM child_done_sources s WHERE s.child_id=c.bot_id AND s.turn_id=t.id AND s.covered=1)) AND NOT EXISTS(SELECT 1 FROM child_done_state d WHERE d.child_id=? AND d.pending_turn_id IS NOT NULL) AND NOT EXISTS(SELECT 1 FROM child_done_sources s WHERE s.child_id=? AND s.covered=0)")
        .bind(&child.child_id).bind(&child.child_id).bind(&child.child_id).bind(&child.child_id).fetch_one(&app.db).await?;
    if !child.gone && child_still_active(app, Some(&child.run_id)).await {
        sqlx::query("UPDATE child_done_state SET active=1 WHERE child_id=?").bind(&child.child_id).execute(&app.db).await?;
        return Ok(())
    }
    let inflight: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM turns WHERE run_id=? AND status='in_flight')")
        .bind(&child.run_id).fetch_one(&app.db).await?;
    if inflight && !child.gone {
        sqlx::query("UPDATE child_done_state SET active=1 WHERE child_id=?").bind(&child.child_id).execute(&app.db).await?;
        return Ok(())
    }
    let background = !child.gone && background_work_running(app, Some(&child.run_id), &child.kind).await;
    let parent = parent_lock(&child.parent_id);
    let _guard = parent.lock().await;
    // Recheck DB state under the lock: a new turn may have started during pane I/O.
    let idle: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runs r WHERE r.id=? AND r.state='running' AND r.agent_status IN ('idle','unknown') AND NOT EXISTS (SELECT 1 FROM turns t WHERE t.run_id=r.id AND t.status='in_flight'))")
        .bind(&child.run_id).fetch_one(&app.db).await?;
    if !idle && !child.gone { return Ok(()) }
    cover_delivered_relay(app, &child, background).await?;
    if notice_only && !child.gone { return Ok(()) }
    let mut turns: Vec<SourceTurn> = sqlx::query_as("SELECT t.rowid, t.id, t.completed_at, t.status, t.delivery,
        (SELECT m.content FROM messages m WHERE m.turn_id=t.id AND m.role='assistant' ORDER BY m.created_at DESC,m.rowid DESC LIMIT 1) reply
        FROM turns t JOIN conversations c ON c.id=t.conversation_id WHERE c.bot_id=?
        AND t.status IN ('completed','completed_fallback','failed') AND t.completed_at IS NOT NULL
        AND NOT (t.queue_class='daemon' AND COALESCE(t.client_request_id,'') LIKE 'relay-result:%')
        AND NOT EXISTS(SELECT 1 FROM relay_requests rr WHERE rr.from_bot_id=? AND rr.to_bot_id=c.bot_id AND (rr.state<>'failed' OR rr.notified_at IS NOT NULL OR rr.alerted_at IS NULL OR rr.error LIKE 'notification delivery unconfirmed%') AND (rr.to_turn_id=t.id OR t.client_request_id='relay-request:'||rr.id))
        AND (t.rowid=(SELECT recent.rowid FROM turns recent WHERE recent.conversation_id=c.id AND recent.status IN ('completed','completed_fallback','failed') AND recent.completed_at IS NOT NULL ORDER BY recent.completed_at DESC,recent.rowid DESC LIMIT 1)
          OR NOT EXISTS(SELECT 1 FROM child_done_sources cs WHERE cs.child_id=c.bot_id AND cs.turn_id=t.id AND cs.covered=1))
        AND (julianday(t.completed_at)>=julianday((SELECT initialized_at FROM child_done_state WHERE child_id=c.bot_id),'-1 hour')
          OR (julianday(t.completed_at)>=julianday((SELECT initialized_at FROM child_done_state WHERE child_id=c.bot_id),'-7 days') AND NOT EXISTS
              (SELECT 1 FROM runs pr WHERE pr.bot_id=? AND julianday(pr.started_at)<=julianday(t.completed_at)
                 AND (pr.ended_at IS NULL OR julianday(pr.ended_at)>=julianday(t.completed_at)))))
        ORDER BY t.completed_at,t.rowid")
        .bind(&child.child_id).bind(&child.parent_id).bind(&child.parent_id).fetch_all(&app.db).await?;
    let has_pending: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM child_done_state WHERE child_id=? AND pending_turn_id IS NOT NULL)")
        .bind(&child.child_id).fetch_one(&app.db).await?;
    if turns.is_empty() && !has_pending {
        let latest_is_notice: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM turns t JOIN conversations c ON c.id=t.conversation_id WHERE c.bot_id=? AND t.status IN ('completed','completed_fallback','failed') AND t.id=(SELECT t2.id FROM turns t2 WHERE t2.conversation_id=c.id AND t2.status IN ('completed','completed_fallback','failed') ORDER BY t2.completed_at DESC,t2.rowid DESC LIMIT 1) AND ((t.queue_class='daemon' AND t.client_request_id LIKE 'relay-result:%') OR EXISTS(SELECT 1 FROM relay_requests r WHERE r.from_bot_id=? AND r.to_bot_id=c.bot_id AND (r.state<>'failed' OR r.notified_at IS NOT NULL OR r.alerted_at IS NULL OR r.error LIKE 'notification delivery unconfirmed%') AND (r.to_turn_id=t.id OR t.client_request_id='relay-request:'||r.id))))")
            .bind(&child.child_id).bind(&child.parent_id).fetch_one(&app.db).await?;
        if latest_is_notice && !child.gone { return Ok(()) }
    }
    if turns.is_empty() && (child.status_since.is_some() || has_pending || child.gone) {
        turns.push(SourceTurn { rowid: 0, id: child.run_id.clone(), completed_at: child.status_since.clone().unwrap_or_else(crate::db::now), status: "idle_without_turn".into(), delivery: "ok".into(), reply: None });
    }
    let Some(latest) = turns.last() else { return Ok(()) };
    for t in turns.iter().filter(|t| t.status != "idle_without_turn") {
        sqlx::query("INSERT OR IGNORE INTO child_done_sources(child_id,turn_id) VALUES(?,?)")
            .bind(&child.child_id).bind(&t.id).execute(&app.db).await?;
    }
    if !child.gone && latest.status == "completed_fallback" && chrono::DateTime::parse_from_rfc3339(&latest.completed_at)
        .is_ok_and(|t| chrono::Utc::now().signed_duration_since(t).num_seconds() < FALLBACK_SETTLE_SECS) { return Ok(()) }
    sqlx::query("UPDATE child_done_state SET idle_at=CASE WHEN active=1 OR idle_turn_id<>? OR (? IS NOT NULL AND ? > idle_at) THEN max(COALESCE(?,''),?) ELSE idle_at END, idle_turn_id=?, active=0 WHERE child_id=?")
        .bind(&latest.id).bind(&child.status_since).bind(&child.status_since).bind(&child.status_since).bind(&latest.completed_at).bind(&latest.id).bind(&child.child_id).execute(&app.db).await?;
    let mut state: State = sqlx::query_as("SELECT covered_rowid,covered_at,pending_turn_id,pending_rowid,pending_at,pending_idle_at,pending_background,notified_background,notified_at,idle_at FROM child_done_state WHERE child_id=?")
        .bind(&child.child_id).fetch_one(&app.db).await?;
    let conv = crate::db::conversation_id(&app.db, &child.parent_id).await?;
    let prefix = format!("{CRID_PREFIX}{}:", child.child_id);
    if let Some(base) = state.pending_turn_id.as_deref() {
        // A queued notification is not an acknowledgement. In-flight with delivery=ok is.
        let last: Option<(String,String,String,String,bool,String,i64)> = sqlx::query_as("SELECT client_request_id,status,delivery,COALESCE(completed_at,created_at),
            EXISTS(SELECT 1 FROM messages m WHERE m.turn_id=t.id AND m.role='system' AND m.content=?), COALESCE(delivered_at,created_at), delivery_verified
            FROM turns t WHERE conversation_id=? AND (client_request_id=? OR substr(client_request_id,1,length(?)+2)=?||':r')
            ORDER BY created_at DESC,rowid DESC LIMIT 1")
            .bind(crate::lifecycle::daemon_notice::WITHDRAWN_WHY).bind(&conv).bind(base).bind(base).bind(base).fetch_optional(&app.db).await?;
        if let Some((crid, status, delivery, at, withdrawn, delivered_at, verified)) = last {
            if delivery == "ok" && verified == 1 && status != "queued" {
                let mut tx = app.db.begin().await?;
                sqlx::query("UPDATE child_done_sources SET covered=1 WHERE child_id=? AND batch=?")
                    .bind(&child.child_id).bind(base).execute(&mut *tx).await?;
                sqlx::query("UPDATE child_done_state SET notified_at=?,covered_rowid=pending_rowid,covered_at=pending_at,notified_background=pending_background,pending_turn_id=NULL,pending_rowid=NULL,pending_at=NULL,pending_idle_at=NULL,pending_background=NULL WHERE child_id=?")
                    .bind(&delivered_at).bind(&child.child_id).execute(&mut *tx).await?;
                tx.commit().await?;
                state.notified_at = Some(delivered_at);
                state.covered_rowid = state.pending_rowid.unwrap_or(state.covered_rowid);
                state.covered_at = state.pending_at.clone();
                state.notified_background = state.pending_background.unwrap_or(0);
                state.pending_turn_id = None;
            } else if withdrawn {
                let new_sources: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM child_done_sources WHERE child_id=? AND batch IS NULL AND covered=0)")
                    .bind(&child.child_id).fetch_one(&app.db).await?;
                if new_sources || state.pending_idle_at.as_ref().is_some_and(|at| at < &state.idle_at)
                        || (state.pending_background == Some(1) && !background) {
                    let mut tx = app.db.begin().await?;
                    sqlx::query("UPDATE child_done_sources SET covered=1 WHERE child_id=? AND batch=?")
                        .bind(&child.child_id).bind(base).execute(&mut *tx).await?;
                    sqlx::query("UPDATE child_done_state SET pending_turn_id=NULL,pending_rowid=NULL,pending_at=NULL,pending_background=NULL WHERE child_id=?")
                        .bind(&child.child_id).execute(&mut *tx).await?;
                    tx.commit().await?;
                }
                return Ok(())
            }
            else if status == "queued" || (status == "in_flight" && delivery == "pending") { return Ok(()) }
            else {
                let attempt = crate::child_done::attempt_of(base, &crid);
                if attempt >= RETRY_LIMIT {
                    alert_undeliverable(&app.db, &child.parent_id, &child.child_name, base, attempt).await?;
                    let new_sources: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM child_done_sources WHERE child_id=? AND batch IS NULL AND covered=0)")
                        .bind(&child.child_id).fetch_one(&app.db).await?;
                    if new_sources || state.pending_idle_at.as_ref().is_some_and(|at| at < &state.idle_at)
                        || (state.pending_background == Some(1) && !background) {
                        let mut tx = app.db.begin().await?;
                        sqlx::query("UPDATE child_done_sources SET batch=NULL WHERE child_id=? AND batch=? AND covered=0")
                            .bind(&child.child_id).bind(base).execute(&mut *tx).await?;
                        sqlx::query("UPDATE child_done_state SET pending_turn_id=NULL,pending_rowid=NULL,pending_at=NULL,pending_background=NULL WHERE child_id=?")
                            .bind(&child.child_id).execute(&mut *tx).await?;
                        tx.commit().await?;
                    }
                    return Ok(())
                }
                let cooled = chrono::DateTime::parse_from_rfc3339(&at).is_ok_and(|t| chrono::Utc::now().signed_duration_since(t).num_seconds() >= CHILD_DONE_RETRY_COOLDOWN);
                if !cooled { return Ok(()) }
            }
        }
    }
    if crate::db::active_run(&app.db, &child.parent_id).await?.is_none() { return Ok(()) }
    let unassigned: Vec<String> = sqlx::query_scalar("SELECT turn_id FROM child_done_sources WHERE child_id=? AND batch IS NULL AND covered=0")
        .bind(&child.child_id).fetch_all(&app.db).await?;
    let gone_base = format!("{prefix}gone:{}", child.run_id);
    let gone_delivered: bool = if child.gone { sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM turns WHERE conversation_id=? AND (client_request_id=? OR substr(client_request_id,1,length(?)+2)=?||':r') AND delivery='ok' AND delivery_verified=1 AND status<>'queued')")
        .bind(&conv).bind(&gone_base).bind(&gone_base).bind(&gone_base).fetch_one(&app.db).await? } else { false };
    let gone_owed = child.gone && !gone_delivered;
    let background_finished = state.notified_background != 0 && !background;
    let idle_again = state.notified_at.as_ref().is_some_and(|at| at < &state.idle_at)
        || (state.notified_at.is_none() && latest.status == "idle_without_turn");
    if state.pending_turn_id.is_none() {
        if unassigned.is_empty() && !background_finished && !idle_again && !gone_owed { return Ok(()) }
        if child.gone && !gone_owed { return Ok(()) }
        let latest_unassigned = turns.iter().rev().find(|t| unassigned.contains(&t.id)).unwrap_or(latest);
        let mut base = if gone_owed { gone_base.clone() } else if unassigned.is_empty() {
            let tag = if background_finished { "background" } else { "idle" };
            format!("{prefix}{}:{tag}{}", latest.id, crate::db::ulid())
        } else { format!("{prefix}{}", latest_unassigned.id) };
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM turns WHERE conversation_id=? AND (client_request_id=? OR substr(client_request_id,1,length(?)+2)=?||':r'))")
            .bind(&conv).bind(&base).bind(&base).bind(&base).fetch_one(&app.db).await?;
        // Preserve the retry history for a single legacy turn. A multi-turn batch needs a
        // fresh key: the old notice may have omitted the earlier turns.
        let adopt_legacy = !child.gone && exists && unassigned.len() == 1 && state.pending_idle_at.is_none() && !background;
        if exists && !adopt_legacy && !gone_owed { base = format!("{base}:episode{}", crate::db::ulid()); }
        let mut tx = app.db.begin().await?;
        sqlx::query("UPDATE child_done_sources SET batch=? WHERE child_id=? AND batch IS NULL AND covered=0")
            .bind(&base).bind(&child.child_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE child_done_state SET pending_turn_id=?,pending_rowid=?,pending_at=?,pending_idle_at=?,pending_background=? WHERE child_id=?")
            .bind(&base).bind(latest.rowid).bind(&latest.completed_at).bind(&state.idle_at).bind(i64::from(background)).bind(&child.child_id).execute(&mut *tx).await?;
        tx.commit().await?;
        if adopt_legacy { return Ok(()) }
        state.pending_turn_id=Some(base); state.pending_rowid=Some(latest.rowid); state.pending_at=Some(latest.completed_at.clone()); state.pending_background=Some(i64::from(background));
    }
    let base = state.pending_turn_id.as_deref().unwrap();
    let last_crid: Option<String> = sqlx::query_scalar("SELECT client_request_id FROM turns WHERE conversation_id=? AND (client_request_id=? OR substr(client_request_id,1,length(?)+2)=?||':r') ORDER BY created_at DESC,rowid DESC LIMIT 1")
        .bind(&conv).bind(base).bind(base).bind(base).fetch_optional(&app.db).await?;
    let crid = last_crid.as_deref().map(|s| format!("{base}:r{}",crate::child_done::attempt_of(base,s)+1)).unwrap_or_else(|| base.to_string());
    let mut excerpts = Vec::new();
    let batch_turns: Vec<String> = sqlx::query_scalar("SELECT turn_id FROM child_done_sources WHERE child_id=? AND batch=? AND covered=0")
        .bind(&child.child_id).bind(base).fetch_all(&app.db).await?;
    if batch_turns.is_empty() && !child.gone && state.pending_rowid.is_some_and(|row| row > 0) {
        excerpts.push(format!("來源回合紀錄已刪除，請讀 child 的 pane：herdr agent read {} --lines 200", child.child_name));
    }
    for t in turns.iter().filter(|t| batch_turns.contains(&t.id)) {
        if t.status == "failed" && t.delivery == "unknown" {
            excerpts.push(format!("回合 {}：送出未確認，請檢查輸入框：herdr agent read {} --lines 200", t.id, child.child_name));
            continue;
        }
        if t.status == "failed" {
            let failed_relay: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM relay_requests r WHERE r.from_bot_id=? AND r.to_bot_id=? AND r.state='failed' AND (r.to_turn_id=? OR EXISTS(SELECT 1 FROM turns rt WHERE rt.id=? AND rt.client_request_id='relay-request:'||r.id)))")
                .bind(&child.parent_id).bind(&child.child_id).bind(&t.id).bind(&t.id).fetch_one(&app.db).await?;
            if failed_relay {
                excerpts.push(format!("回合 {}：失敗，沒有可確認的完整回覆。", t.id));
                continue;
            }
        }
        let reply = t.reply.as_deref().filter(|r| !r.trim().is_empty());
        if let Some(reply) = reply {
            if child_already_reported(&app.db,&conv,&child.child_id,&prefix,&t.id,reply).await? { continue }
            excerpts.push(format!("回合 {}：\n{}", t.id, reply));
        } else if child.gone { excerpts.push(format!("回合 {}：沒有回覆文字；child pane 已收掉。", t.id)); } else { excerpts.push(format!("回合 {}：未擷取到回覆（沒有回覆文字），請讀它的 pane：herdr agent read {} --lines 200",t.id,child.child_name)); }
    }
    if excerpts.is_empty() && !background_finished && !idle_again && !gone_owed && state.pending_background != Some(1) {
        // All turns were explicitly reported by the child itself.
        let mut tx = app.db.begin().await?;
        sqlx::query("UPDATE child_done_sources SET covered=1 WHERE child_id=? AND batch=?")
            .bind(&child.child_id).bind(base).execute(&mut *tx).await?;
        sqlx::query("UPDATE child_done_state SET notified_at=?,covered_rowid=pending_rowid,covered_at=pending_at,notified_background=0,pending_turn_id=NULL,pending_rowid=NULL,pending_at=NULL,pending_idle_at=NULL,pending_background=NULL WHERE child_id=?")
            .bind(crate::db::now()).bind(&child.child_id).execute(&mut *tx).await?;
        tx.commit().await?;
        return Ok(())
    }
    let n = excerpts.len();
    excerpts.reverse();
    if n > 1 { excerpts.insert(0, format!("本次合併 {n} 個回合（最新回合在前；完整內容可讀 child 的對話或 pane）。")); }
    if excerpts.is_empty() { excerpts.push(if child.gone { "child pane 已收掉，沒有新的回覆文字。" } else if background_finished { "背景工作已結束，child 仍閒置。" } else { "未擷取到回覆，child 已閒置，請讀它的 pane。" }.into()); }
    let parent_kind: String = sqlx::query_scalar("SELECT kind FROM bots WHERE id=?").bind(&child.parent_id).fetch_one(&app.db).await?;
    let reply = excerpts.join("\n\n");
    let mut message = message_for_child(&child.child_id,&child.child_name,&reply,notice_reply_limit(&parent_kind));
    if turns.iter().any(|t| batch_turns.contains(&t.id) && t.status == "failed" && t.delivery == "unknown") {
        message = message.replacen("已完成一個回合", "有回合送出未確認", 1);
    }
    if child.gone { message = message.replacen("已完成一個回合。", "意外結束。", 1); }
    let last_turn: Option<String> = if child.gone { sqlx::query_scalar("SELECT t.id FROM turns t JOIN conversations c ON c.id=t.conversation_id WHERE c.bot_id=? ORDER BY t.rowid DESC LIMIT 1").bind(&child.child_id).fetch_optional(&app.db).await? } else { None };
    let gone_annotation = format!("child 意外結束（agent 不在）；最後狀態：{}；最後一回合 id：{}；worktree 路徑：{}。\n", child.last_status, last_turn.as_deref().unwrap_or("未知"), child.cwd.as_deref().unwrap_or("未知"));
    let annotation = if child.gone { gone_annotation.as_str() } else if state.pending_background == Some(1) { "回合停了、背景工作仍在跑。\n" }
        else if background_finished { "背景工作已結束，child 仍閒置。\n" } else { "" };
    if !annotation.is_empty() { message = message.replacen("\n\n", &format!("\n{annotation}\n"), 1); }
    // Another daemon notice occupies the single queue slot; waiting is not a failed delivery.
    let slot_busy: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM turns WHERE conversation_id=? AND status='queued' AND queue_class='daemon' AND client_request_id<>?)")
        .bind(&conv).bind(&crid).fetch_one(&app.db).await?;
    if slot_busy { return Ok(()) }
    match app.prompt_relayed_queueable(&child.parent_id,&message,&crid,Some(&child.child_id),crate::lifecycle::QueueClass::DaemonNotice).await {
        Ok(_) => Ok(()),
        Err(crate::lifecycle::LcError::Conflict(body)) if unsent_composer_conflict(&body) => {
            record_unsent_attempt(&app.db,&child.parent_id,&conv,&child.child_name,base,&crid,RETRY_LIMIT).await?;
            Ok(())
        }
        Err(e) => {
            if crate::db::active_run(&app.db, &child.parent_id).await?.is_none() { return Ok(()) }
            tracing::warn!(child = %child.child_name, error = ?e, "child notice attempt failed");
            // If a turn already exists, INSERT OR IGNORE preserves its actual delivery state.
            record_unsent_attempt(&app.db,&child.parent_id,&conv,&child.child_name,base,&crid,RETRY_LIMIT).await?;
            Ok(())
        },
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
