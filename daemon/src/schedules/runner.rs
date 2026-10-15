//! 排程 runner：到期→ occurrence（帳本）→ 在 bot 鎖內同交易綁 turn → commit 後放鎖、叫既有 kick／flush。
//!
//! 鎖序：bot 鎖 → DB 交易（BEGIN IMMEDIATE）→ commit → 放鎖 → 事件／kick。
//! 不在 DB 交易裡等 bot 鎖，也不在 bot 鎖裡呼叫會再拿同一把鎖的公開 prompt／start。
//! 等待原因（bot 忙、stopping、Bot 不合格…）記在 occurrence.waiting_reason，UI 看得到；不靜默重試、不覆蓋使用者的佇列。

use super::store::{self, Occurrence, Schedule, Transition};
use super::time::{self, Spec};
use crate::db;
use crate::lifecycle;
use crate::state::App;
use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::json;
use crate::lifecycle::send_now::ports::MaintenancePort;
use std::sync::Arc;

/// tick 間隔：排程精度到分鐘即可；太密只會增加空轉。
pub const TICK_EVERY: std::time::Duration = std::time::Duration::from_secs(20);

fn fmt(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// 一次 tick：收斂終態與通知、到期落成 occurrence、把 pending 綁成 turn、重啟後補 kick。
pub async fn tick(app: &Arc<App>) -> Result<()> {
    let pool = app.db.clone();
    let now = db::now();
    for s in store::list_due(&pool, &now).await? {
        if let Err(e) = materialize(&pool, &s, &now).await {
            tracing::warn!(schedule = %s.id, error = ?e, "schedule materialize failed");
        }
    }
    for t in store::settle_queued(&pool, &now).await? {
        notify_transition(app, &t).await;
        if should_clear_after(&t) {
            spawn_clear_after(app, t.bot_id.clone());
        }
    }
    for occ in store::open_occurrences(&pool).await? {
        if occ.status != "pending" {
            continue;
        }
        if let Err(e) = enqueue(app, &occ.id).await {
            tracing::warn!(occurrence = %occ.id, error = ?e, "schedule enqueue failed; ledger keeps it pending");
        }
    }
    // 重啟後：已綁定、還在等啟動（awaits_start）且沒有 run 的 bot，依既有 kick 的門檻補叫一次。
    let waiting: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT o.bot_id FROM schedule_occurrences o JOIN turns t ON t.id = o.turn_id
          WHERE o.status = 'queued' AND t.status = 'queued' AND t.awaits_start = 1",
    )
    .fetch_all(&pool)
    .await?;
    for (bot_id,) in waiting {
        // 維護窗口（或讀不到）時不喚醒：先過同一道窗口閘，才 kick。
        if !matches!(app.window_held().await, Ok(None)) {
            continue;
        }
        if db::active_run(&pool, &bot_id).await?.is_none() {
            crate::lifecycle::start_send::kick(app, &bot_id);
        }
    }
    Ok(())
}

/// 這次終態要不要做「跑完就清空對話」：只有 done 且排程勾了（failed／unknown／cancelled 留著，讓使用者看得出出了什麼事）。
pub(crate) fn should_clear_after(t: &Transition) -> bool {
    t.clear_after && t.status == "done"
}

/// 跑完就清空對話：放到背景做，tick 不等它（重啟要拿 bot 鎖，可能要等一陣子）。
fn spawn_clear_after(app: &Arc<App>, bot_id: String) {
    let app = app.clone();
    tokio::spawn(Box::pin(async move {
        if let Err(e) = clear_bot_after(&app, &bot_id).await {
            tracing::warn!(bot = %bot_id, error = %e, "clear-after-done restart failed");
        }
    }));
}

/// 排程跑完（done）且勾了「跑完就清空對話」：讓這顆 Bot 開新對話（`StartOpts::default()`，同 `restart?resume=fresh`）。
/// 條件全成立才做，任一不成立就什麼都不動、回 `None`：Bot 還在跑（沒在跑不替它啟動）、沒有排隊或執行中的回合、
/// 沒有其他未結束的排程 occurrence、維護窗口沒開。重啟本身在 bot 鎖內再確認一次閒置（`require_idle`），
/// 不閒置就回錯、不動。回傳新的 run id（真的重啟了）。
pub(crate) async fn clear_bot_after(app: &Arc<App>, bot_id: &str) -> Result<Option<String>> {
    let pool = app.db.clone();
    if db::active_run(&pool, bot_id).await?.is_none() {
        return Ok(None);
    }
    let open: i64 = sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM turns t JOIN conversations c ON c.id = t.conversation_id
                  WHERE c.bot_id = ? AND t.status IN ('queued','in_flight'))
              + (SELECT COUNT(*) FROM schedule_occurrences WHERE bot_id = ? AND status IN ('pending','queued'))",
    )
    .bind(bot_id)
    .bind(bot_id)
    .fetch_one(&pool)
    .await?;
    if open > 0 || !matches!(app.window_held().await, Ok(None)) {
        return Ok(None);
    }
    let opts = lifecycle::StartOpts { require_idle: true, ..Default::default() };
    // 重啟的 future 很大：裝箱，不放進這個 async fn 的 poll 框（AGENTS.md 的 1 MiB 堆疊守門）。
    let run = Box::pin(lifecycle::restart_bot_with(app, bot_id, opts)).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
    tracing::info!(bot = %bot_id, run = %run, "schedule cleared the bot conversation after done");
    Ok(Some(run))
}

/// 終態：一律推 `schedules_changed`（只帶 id，網頁自己重抓）；依 notify 設定另推 `schedule_notice`。
/// 每筆終態只會出現一次（store 的 notified 去重），所以通知不會重送。
async fn notify_transition(app: &Arc<App>, t: &Transition) {
    app.emit("schedules_changed", json!({"project_id": t.project_id, "id": t.schedule_id})).await;
    if store::wants_notice(&t.notify, &t.status) {
        app.emit(
            "schedule_notice",
            json!({"project_id": t.project_id, "schedule_id": t.schedule_id, "schedule_name": t.schedule_name, "occurrence_id": t.occurrence_id, "status": t.status, "error": t.error}),
        )
        .await;
    }
}

async fn changed(app: &Arc<App>, project_id: &str, schedule_id: &str) {
    app.emit("schedules_changed", json!({"project_id": project_id, "id": schedule_id})).await;
}

/// 到期：只產生一筆 occurrence（或合併進未結束的那筆），並把 next_due 推到未來（錯過的不補跑）。
pub(crate) async fn materialize(pool: &sqlx::SqlitePool, s: &Schedule, now: &str) -> Result<()> {
    let Some(due) = s.next_due.clone() else { return Ok(()) };
    let spec: Spec = serde_json::from_str(&s.spec_json).context("spec_json")?;
    let now_dt = DateTime::parse_from_rfc3339(now)?.with_timezone(&Utc);
    let next = time::next_after(&spec, &s.tz, now_dt)?.map(fmt);
    let mut tx = db::begin_write(pool).await?;
    // 交易內重讀：到期判斷與寫入之間可能被停用、編輯或刪除。
    let Some(cur) = store::get_schedule_tx(&mut tx, &s.id).await? else { return Ok(()) };
    if cur.enabled != 1 || cur.deleted_at.is_some() || cur.next_due.as_deref() != Some(due.as_str()) || cur.generation != s.generation {
        return Ok(());
    }
    match store::open_occurrence_tx(&mut tx, &s.id).await? {
        Some(open) => store::merge_into_open_tx(&mut tx, &open.id, now).await?,
        None => {
            let occ = Occurrence {
                id: db::ulid(),
                schedule_id: s.id.clone(),
                source: "scheduled".into(),
                scheduled_for: due.clone(),
                generation: s.generation,
                bot_id: s.bot_id.clone(),
                prompt: s.prompt.clone(),
                client_request_id: None,
                status: "pending".into(),
                waiting_reason: None,
                notified: 0,
                merged_count: 0,
                conversation_id: None,
                turn_id: None,
                error: None,
                created_at: now.into(),
                updated_at: now.into(),
            };
            store::insert_occurrence_tx(&mut tx, &occ).await?;
        }
    }
    store::set_next_due_tx(&mut tx, &s.id, next.as_deref(), now).await?;
    tx.commit().await?;
    Ok(())
}

/// 把一筆 pending occurrence 綁成 queued turn。忙碌時照常排進既有佇列（不 steer、不 interrupt）。
/// 回傳 `true` 代表已綁定；`false` 代表這次不送（等待中、已取消、Bot 不合格等，原因記在 waiting_reason 或 status）。
pub async fn enqueue(app: &Arc<App>, occurrence_id: &str) -> Result<bool> {
    let pool = app.db.clone();
    let Some(head) = store::get_occurrence(&pool, occurrence_id).await? else { return Ok(false) };
    if head.status != "pending" {
        return Ok(false);
    }
    let lock = app.bot_lock(&head.bot_id).await;
    let _guard = lock.lock().await;
    let now = db::now();
    // 會話與 run 在 bot 鎖內讀（鎖住期間不會變），交易開之前讀，避免交易佔住唯一連線時再向池子要連線。
    let conv = db::conversation_id(&pool, &head.bot_id).await?;
    let run = db::active_run(&pool, &head.bot_id).await?;
    // 維護窗口閘：在 bot 鎖內、DB 交易之前檢查（不在交易裡讀窗口，免得交易中又拿 DB 連線）。
    // 窗口開著或讀不到 → 不造 turn、不 kick；ledger 保留 pending，tick 下一輪再試。
    match app.window_held().await {
        Ok(None) => {}
        Ok(Some(_)) => {
            set_wait_for(app, occurrence_id, "maintenance_window", &now).await;
            return Ok(false);
        }
        Err(_) => {
            set_wait_for(app, occurrence_id, "maintenance_unreadable", &now).await;
            return Ok(false);
        }
    }
    let queued_ahead: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id = ? AND status = 'queued'")
        .bind(&conv)
        .fetch_one(&pool)
        .await?;
    let mut tx = db::begin_write(&pool).await?;
    let Some(occ) = store::get_occurrence_tx(&mut tx, occurrence_id).await?.filter(|o| o.status == "pending") else {
        return Ok(false);
    };
    let sched = store::get_schedule_tx(&mut tx, &occ.schedule_id).await?;
    let sched_ok = sched.as_ref().is_some_and(|s| s.enabled == 1 && s.deleted_at.is_none() && s.generation == occ.generation);
    if !sched_ok {
        // 排程已停用、刪除或換過設定（generation 不同）：這筆不再送，記成取消而不是永遠 pending。
        store::finish_open_tx(&mut tx, occurrence_id, "cancelled", "排程已停用、刪除或已修改", &now).await?;
        tx.commit().await?;
        return Ok(false);
    }
    let sched = sched.context("schedule vanished")?;
    // Bot 資格：同一個專案（排程建立時的專案）、未刪、一般 Bot。換到別的專案＝不合格。
    let Some(kind) = eligible_bot_tx(&mut tx, &occ.bot_id, Some(&sched.project_id)).await? else {
        store::finish_open_tx(&mut tx, occurrence_id, "failed", "綁定的 Bot 已不存在、已移交、或不能接收排程", &now).await?;
        tx.commit().await?;
        notify_terminal(app, &sched, occurrence_id, "failed", Some("綁定的 Bot 不合格")).await;
        return Ok(false);
    };
    let text = crate::lifecycle::pane_text::for_pane(&kind, &occ.prompt);
    if crate::lifecycle::pane_text::nothing_left(&occ.prompt, &text) {
        store::finish_open_tx(&mut tx, occurrence_id, "failed", "排程的 prompt 清理後是空的", &now).await?;
        tx.commit().await?;
        notify_terminal(app, &sched, occurrence_id, "failed", Some("prompt 是空的")).await;
        return Ok(false);
    }
    // 停止中：不收、不踢；等它停完再由 tick 重試。
    if run.as_ref().is_some_and(|r| r.state == "stopping") {
        tx.commit().await?;
        set_wait(app, &sched, occurrence_id, "stopping", &now).await;
        return Ok(false);
    }
    // 既有的 queue 上限是每個會話 1 筆 queued：已有人排著就等，不覆蓋、不另開。
    if queued_ahead > 0 {
        tx.commit().await?;
        set_wait(app, &sched, occurrence_id, "bot_busy", &now).await;
        return Ok(false);
    }
    // 沒有 run 或還在起：awaits_start=1，讓既有的啟動路徑認領；有 run 且在跑：只等 idle。
    let starting_or_none = run.as_ref().is_none_or(|r| r.state == "starting");
    let awaits_start: i64 = if starting_or_none { 1 } else { 0 };
    let turn_id = db::ulid();
    let msg_id = db::ulid();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, client_request_id, created_at, prompt_text, awaits_idle, awaits_start)
         VALUES (?,?,NULL,'web','queued','pending',NULL,?,?,1,?)",
    )
    .bind(&turn_id)
    .bind(&conv)
    .bind(&now)
    .bind(&text)
    .bind(awaits_start)
    .execute(&mut *tx)
    .await
    .context("insert scheduled turn")?;
    sqlx::query(
        "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, relay_from, relay_unverified, created_at)
         VALUES (?,?,?,'user',?,'web','daemon',0,?)",
    )
    .bind(&msg_id)
    .bind(&conv)
    .bind(&turn_id)
    .bind(&occ.prompt)
    .bind(&now)
    .execute(&mut *tx)
    .await
    .context("insert scheduled message")?;
    store::bind_queued_tx(&mut tx, occurrence_id, &conv, &turn_id, &now).await?;
    tx.commit().await?;
    drop(_guard);
    tracing::info!(occurrence = occurrence_id, turn = %turn_id, bot = %occ.bot_id, "schedule occurrence queued");
    // 讓 Bot 對話立刻看見這則排程的 user 訊息與它排隊的 turn（既有的發布路徑）。
    crate::lifecycle::prompt::publish_prompt_message(&**app, &occ.bot_id, &msg_id).await;
    crate::lifecycle::emit_turn(app, &turn_id).await;
    changed(app, &sched.project_id, &sched.id).await;
    // 叫既有的 kick／flush：沒有 run 走啟動路徑（kick），有 run 走佇列 flush。
    if run.is_none() {
        crate::lifecycle::start_send::kick(app, &occ.bot_id);
    }
    crate::lifecycle::schedule_flush_queued(app, &occ.bot_id);
    Ok(true)
}

/// 等待原因（只給定 occurrence 時用：自己查排程）。改變時才推 WS。
async fn set_wait_for(app: &Arc<App>, occurrence_id: &str, reason: &str, now: &str) {
    let Ok(Some(occ)) = store::get_occurrence(&app.db, occurrence_id).await else { return };
    let Ok(Some(sched)) = store::get_schedule(&app.db, &occ.schedule_id).await else { return };
    set_wait(app, &sched, occurrence_id, reason, now).await;
}

/// 等待原因：只在改變時推 WS，UI 才知道為什麼還沒送。
async fn set_wait(app: &Arc<App>, sched: &Schedule, occurrence_id: &str, reason: &str, now: &str) {
    match store::set_waiting_reason(&app.db, occurrence_id, Some(reason), now).await {
        Ok(true) => changed(app, &sched.project_id, &sched.id).await,
        Ok(false) => {}
        Err(e) => tracing::warn!(occurrence = occurrence_id, error = ?e, "set waiting reason failed"),
    }
}

async fn notify_terminal(app: &Arc<App>, sched: &Schedule, occurrence_id: &str, status: &str, error: Option<&str>) {
    let t = Transition {
        schedule_id: sched.id.clone(),
        schedule_name: sched.name.clone(),
        project_id: sched.project_id.clone(),
        occurrence_id: occurrence_id.into(),
        status: status.into(),
        notify: sched.notify.clone(),
        error: error.map(str::to_string),
        bot_id: sched.bot_id.clone(),
        clear_after: sched.clear_after == 1,
    };
    notify_transition(app, &t).await;
}

/// 停用、編輯、刪除共用（呼叫端持有 bot 鎖、自己開交易）：
/// pending 直接取消；queued 若還沒被 flush 領走就同交易撤回，occurrence 一併取消。
/// 已經被領走（in_flight）的不撤、occurrence 維持 queued，等 [`store::settle_queued`] 依 turn 終態收斂。
pub async fn withdraw_open_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    schedule_id: &str,
    why: &str,
    now: &str,
) -> Result<Option<(String, crate::lifecycle::Revoked)>> {
    sqlx::query("UPDATE schedule_occurrences SET status = 'cancelled', error = ?, waiting_reason = NULL, updated_at = ? WHERE schedule_id = ? AND status = 'pending'")
        .bind(why)
        .bind(now)
        .bind(schedule_id)
        .execute(&mut **tx)
        .await?;
    let queued: Option<(Option<String>,)> = sqlx::query_as("SELECT turn_id FROM schedule_occurrences WHERE schedule_id = ? AND status = 'queued' LIMIT 1")
        .bind(schedule_id)
        .fetch_optional(&mut **tx)
        .await?;
    let Some((Some(turn_id),)) = queued else { return Ok(None) };
    let Some(revoked) = crate::lifecycle::revoke_queued_turn_tx(tx, &turn_id, why).await? else { return Ok(None) };
    sqlx::query("UPDATE schedule_occurrences SET status = 'cancelled', error = ?, updated_at = ? WHERE turn_id = ? AND status = 'queued'")
        .bind(why)
        .bind(now)
        .bind(&turn_id)
        .execute(&mut **tx)
        .await?;
    Ok(Some((turn_id, revoked)))
}

/// 取 bot 鎖後單獨撤回（沒有其他寫入時用）。
pub async fn withdraw_open_for(app: &Arc<App>, bot_id: &str, schedule_id: &str, why: &str) -> Result<()> {
    let pool = app.db.clone();
    let lock = app.bot_lock(bot_id).await;
    let _guard = lock.lock().await;
    let now = db::now();
    let mut tx = db::begin_write(&pool).await?;
    let revoked = withdraw_open_tx(&mut tx, schedule_id, why, &now).await?;
    tx.commit().await?;
    drop(_guard);
    if let Some((turn_id, r)) = revoked {
        crate::lifecycle::announce_revoked(app, &turn_id, r).await;
    }
    Ok(())
}

/// Bot 資格：未刪、同專案（指定時）、非 child／分享／受限，專案未刪未移交。回傳 Bot 種類。
pub async fn eligible_bot_tx(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, bot_id: &str, project_id: Option<&str>) -> Result<Option<String>> {
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT b.kind, b.project_id FROM bots b JOIN projects p ON p.id = b.project_id
          WHERE b.id = ? AND b.deleted_at IS NULL AND p.deleted_at IS NULL AND p.handed_off_to IS NULL
            AND b.managed_by = 'user' AND b.parent_bot_id IS NULL
            AND NOT EXISTS (SELECT 1 FROM bot_shares s WHERE s.bot_id = b.id)
            AND NOT EXISTS (SELECT 1 FROM shared_bots s WHERE s.bot_id = b.id AND s.profile = 'restricted')",
    )
    .bind(bot_id)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.filter(|(_, p)| project_id.is_none_or(|want| want == p)).map(|(kind, _)| kind))
}
