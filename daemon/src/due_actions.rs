//! 「daemon 接下來要做什麼、什麼一直做不成」的單一視窗（issue #75 驗收第 5 條）。
//!
//! **這裡不是新的排程器，一列都不寫。** 到期動作與重試本來就已經落在 DB 上了——每一條都掛在自己那張
//! 表的欄位，記憶體 timer 只是加速，重啟後由掃描或重掛補回來：
//!
//! | 動作 | 到期時間存在哪 | 重啟後誰把它接回來 |
//! |---|---|---|
//! | 排隊 prompt 的重試 | `turns.next_flush_at`（＋`flush_retries`） | `reconcile::rearm_progress` → `lifecycle::rearm_queue_retries` |
//! | 交辦重送 | `supervisor_assignments.next_attempt_at`（＋`attempts`） | 總管 tick（開機先 `reconcile`） |
//! | 等額度回來 | `supervisor_assignments.resume_at` | `controller::resume_quota_blocked` ＋ 開機回填 |
//! | 協調者補送 | `supervisor_inbox.notify_next_at`（＋`notify_attempts`） | 總管 tick |
//! | 總管看門狗 | `supervisors.watchdog_next_at`（＋`watchdog_attempts`） | 總管 tick |
//! | hook 事件 | `hook_events.next_attempt_at`（＋`attempts`） | `hook_inbox::spawn_worker`（先 drain 再等） |
//!
//! 所以 issue 建議的那張共用 `due_actions` 表**沒有加**：那等於把六條已經能重啟恢復的路重寫一遍，
//! 換不到任何新的保證，只換來一次大改的風險。缺的一直只是「看得到」——`attempts: 37` 沒有上下文時
//! 是一個沒有故事的數字，而「哪一件卡住了」以前得自己去翻六張表。

use crate::state::App;
use anyhow::Result;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::sync::Arc;

/// 一件還沒發生的事。`due_at` 是 RFC3339；`None` ＝「下一個事件來就做」，沒有排定時間。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DueAction {
    pub kind: String,
    pub entity_id: String,
    pub due_at: Option<String>,
    pub attempts: i64,
    pub last_error: Option<String>,
}

impl DueAction {
    fn json(&self, now: &str) -> Value {
        json!({
            "kind": self.kind,
            "entity_id": self.entity_id,
            "due_at": self.due_at,
            // 到期了還在這裡＝掃描還沒輪到它（正常）或它一直失敗（看 attempts／last_error）。
            "overdue": self.due_at.as_deref().is_some_and(|d| d <= now),
            "attempts": self.attempts,
            "last_error": self.last_error,
        })
    }
}

/// 每一類最多列這麼多：這是給人看的視窗，不是分頁 API。
const PER_KIND: i64 = 50;
/// 試過這麼多次還沒成功就算「一直做不成」，在摘要裡單獨算一格。
pub const FAILING_ATTEMPTS: i64 = 3;

/// 讀出所有還沒到期／還沒做成的動作，最早到期的在前。**只讀，不寫。**
pub async fn pending(pool: &SqlitePool) -> Result<Vec<DueAction>> {
    let mut out: Vec<DueAction> = Vec::new();

    // 排隊中的 prompt：`next_flush_at IS NULL` 的也算——那是 AGM 派工排進來、等回合結束事件送的，
    // 一樣是「還沒發生的事」，而且它正是重啟後最容易被忘記的一種（5fe77d5 那條路）。
    out.extend(
        sqlx::query_as::<_, DueAction>(
            "SELECT 'queue_retry' AS kind, t.id AS entity_id, t.next_flush_at AS due_at,
                    t.flush_retries AS attempts, NULL AS last_error
             FROM turns t WHERE t.status = 'queued'
             ORDER BY COALESCE(t.next_flush_at, '') , t.rowid LIMIT ?",
        )
        .bind(PER_KIND)
        .fetch_all(pool)
        .await?,
    );

    // 交辦重送與等額度：同一張表，但分成兩種 kind——「送不出去在退避」跟「額度沒了在等」
    // 對看的人是兩件事，混成一個數字就看不出該去修哪個。
    // 「未結案」用 `OPEN_STATES`，不自己寫一份清單：那張表的註解寫得很清楚，清單各寫各的就出過
    // 「`quota_blocked` 從未結案計數裡消失」那種事。`quota_blocked` 在下面自成一類，這裡排掉。
    let open = crate::supervisor::store::sql_list(&crate::supervisor::store::OPEN_STATES);
    out.extend(
        sqlx::query_as::<_, DueAction>(&format!(
            "SELECT 'assignment_retry' AS kind, id AS entity_id, next_attempt_at AS due_at, attempts, error AS last_error
             FROM supervisor_assignments
             WHERE next_attempt_at IS NOT NULL AND status IN ({open}) AND status != 'quota_blocked'
             ORDER BY next_attempt_at LIMIT ?"
        ))
        .bind(PER_KIND)
        .fetch_all(pool)
        .await?,
    );
    out.extend(
        sqlx::query_as::<_, DueAction>(
            "SELECT 'quota_resume' AS kind, id AS entity_id, resume_at AS due_at, attempts, error AS last_error
             FROM supervisor_assignments WHERE status = 'quota_blocked'
             ORDER BY COALESCE(resume_at, '') LIMIT ?",
        )
        .bind(PER_KIND)
        .fetch_all(pool)
        .await?,
    );

    out.extend(
        sqlx::query_as::<_, DueAction>(
            "SELECT 'responder_notify' AS kind, id AS entity_id, notify_next_at AS due_at,
                    notify_attempts AS attempts, NULL AS last_error
             FROM supervisor_inbox WHERE notify_next_at IS NOT NULL AND state != 'handled'
             ORDER BY notify_next_at LIMIT ?",
        )
        .bind(PER_KIND)
        .fetch_all(pool)
        .await?,
    );

    out.extend(
        sqlx::query_as::<_, DueAction>(
            "SELECT 'supervisor_watchdog' AS kind, id AS entity_id, watchdog_next_at AS due_at,
                    watchdog_attempts AS attempts, NULL AS last_error
             FROM supervisors WHERE watchdog_next_at IS NOT NULL LIMIT ?",
        )
        .bind(PER_KIND)
        .fetch_all(pool)
        .await?,
    );

    out.extend(
        sqlx::query_as::<_, DueAction>(
            "SELECT 'hook_event' AS kind, id AS entity_id, next_attempt_at AS due_at, attempts, last_error
             FROM hook_events WHERE processed_at IS NULL
             ORDER BY COALESCE(next_attempt_at, ''), rowid LIMIT ?",
        )
        .bind(PER_KIND)
        .fetch_all(pool)
        .await?,
    );

    // 沒有排定時間的排最後：它們在等事件，不在等時鐘。
    out.sort_by(|a, b| match (&a.due_at, &b.due_at) {
        (Some(x), Some(y)) => x.cmp(y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    Ok(out)
}

/// 給健康快照的那一段。讀不到就回 `null` 而不是讓整份快照失敗：這是觀測用的，不該變成新的故障點。
pub async fn snapshot(app: &Arc<App>) -> Value {
    let rows = match pending(&app.db).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = ?e, "cannot read due actions");
            return Value::Null;
        }
    };
    let now = crate::db::now();
    let overdue = rows.iter().filter(|r| r.due_at.as_deref().is_some_and(|d| d <= now.as_str())).count();
    let failing = rows.iter().filter(|r| r.attempts >= FAILING_ATTEMPTS).count();
    let mut by_kind = serde_json::Map::new();
    for r in &rows {
        *by_kind.entry(r.kind.clone()).or_insert(json!(0)) =
            json!(by_kind.get(&r.kind).and_then(Value::as_u64).unwrap_or(0) + 1);
    }
    json!({
        "pending": rows.len(),
        // 到期了還在名單上：掃描還沒輪到（正常），或它一直失敗。兩者都看 `failing` 與 `items`。
        "overdue": overdue,
        "failing": failing,
        "by_kind": by_kind,
        "soonest": rows.iter().find(|r| r.due_at.is_some()).map(|r| r.json(&now)),
        // 一直做不成的擺前面：那才是要人看的。
        "items": rows
            .iter()
            .filter(|r| r.attempts >= FAILING_ATTEMPTS)
            .take(20)
            .map(|r| r.json(&now))
            .collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing as tt;

    async fn assignment(app: &Arc<App>, id: &str, status: &str, next: Option<&str>, resume: Option<&str>, attempts: i64) {
        sqlx::query(
            "INSERT INTO supervisor_assignments
               (id, supervisor_id, target_bot_id, client_request_id, text, status, attempts,
                next_attempt_at, resume_at, error, created_at, updated_at)
             VALUES (?, 'agm', 'bot1', ?, 'x', ?, ?, ?, ?, 'boom', ?, ?)",
        )
        .bind(id)
        .bind(format!("crid-{id}"))
        .bind(status)
        .bind(attempts)
        .bind(next)
        .bind(resume)
        .bind(crate::db::now())
        .bind(crate::db::now())
        .execute(&app.db)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn every_durable_deadline_shows_up_in_one_place() {
        let e = tt::env().await;
        let app = &e.app;
        assignment(app, "a-retry", "delivered", Some("2026-09-18T10:00:00Z"), None, 1).await;
        assignment(app, "a-quota", "quota_blocked", None, Some("2026-09-18T09:00:00Z"), 0).await;

        let bot = tt::claude_bot(app, &e.project_id, "q").await;
        let conv = crate::db::conversation_id(&app.db, &bot.id).await.unwrap();
        sqlx::query(
            "INSERT INTO turns (id,conversation_id,origin,status,delivery,prompt_text,next_flush_at,flush_retries,created_at)
             VALUES ('t-queued',?,'web','queued','pending','x','2026-09-18T08:00:00Z',4,?)",
        )
        .bind(&conv)
        .bind(crate::db::now())
        .execute(&app.db)
        .await
        .unwrap();

        let rows = pending(&app.db).await.unwrap();
        let kinds: Vec<&str> = rows.iter().map(|r| r.kind.as_str()).collect();
        assert!(kinds.contains(&"queue_retry"), "{kinds:?}");
        assert!(kinds.contains(&"assignment_retry"), "{kinds:?}");
        assert!(kinds.contains(&"quota_resume"), "{kinds:?}");
        // 最早到期的在最前面，不管它是哪一類。
        assert_eq!(rows[0].entity_id, "t-queued", "最早到期的排前面：{kinds:?}");
        assert_eq!(rows[0].due_at.as_deref(), Some("2026-09-18T08:00:00Z"));
    }

    /// 已經結案的交辦不該還掛在待辦上。
    #[tokio::test]
    async fn closed_work_is_not_pending_any_more() {
        let e = tt::env().await;
        assignment(&e.app, "done", "completed", Some("2026-09-18T10:00:00Z"), None, 2).await;
        let rows = pending(&e.app.db).await.unwrap();
        assert!(rows.iter().all(|r| r.entity_id != "done"), "結案的還在名單上");
    }

    /// 摘要要分得出「在等」跟「一直做不成」——這正是 `attempts: 37` 單看沒有故事的地方。
    #[tokio::test]
    async fn the_summary_separates_waiting_from_failing() {
        let e = tt::env().await;
        let app = &e.app;
        assignment(app, "fine", "delivered", Some("2099-01-01T00:00:00Z"), None, 0).await;
        assignment(app, "stuck", "delivered", Some("2000-01-01T00:00:00Z"), None, FAILING_ATTEMPTS + 2).await;

        let s = snapshot(app).await;
        assert_eq!(s["pending"], 2);
        assert_eq!(s["overdue"], 1, "只有 2000 年那筆到期了");
        assert_eq!(s["failing"], 1, "只有試很多次那筆算一直做不成");
        assert_eq!(s["by_kind"]["assignment_retry"], 2);
        assert_eq!(s["soonest"]["entity_id"], "stuck", "最早到期的是它");
        let items = s["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "items 只擺一直做不成的那些");
        assert_eq!(items[0]["entity_id"], "stuck");
        assert_eq!(items[0]["last_error"], "boom", "要看得到為什麼做不成");
        assert_eq!(items[0]["overdue"], true);
    }

    /// 重啟後那些待辦還在——因為它們一開始就不是存在行程裡的（issue #75 的核心主張）。
    ///
    /// `restart_app` 是同一個資料目錄開一顆新的 `App`：記憶體 timer 全沒了，DB 還在。
    /// 這條同時也是「掃回來不會重複執行」的一半：兩次讀到的是**同一批 id**，不是被複製成兩份。
    #[tokio::test]
    async fn due_actions_survive_a_restart_because_they_never_lived_in_the_process() {
        let e = tt::env().await;
        assignment(&e.app, "a-retry", "delivered", Some("2099-01-01T00:00:00Z"), None, 1).await;
        assignment(&e.app, "a-quota", "quota_blocked", None, Some("2099-01-01T00:00:00Z"), 0).await;
        let before: Vec<String> = pending(&e.app.db).await.unwrap().into_iter().map(|r| r.entity_id).collect();
        assert_eq!(before.len(), 2);

        let restarted = tt::restart_app(&e).await;
        let after: Vec<String> = pending(&restarted.db).await.unwrap().into_iter().map(|r| r.entity_id).collect();
        assert_eq!(after, before, "重啟後同一批待辦還在，而且沒有變成兩份");
    }

    /// 什麼都沒排時是一份空的摘要，不是 null、也不是錯誤。
    #[tokio::test]
    async fn an_idle_daemon_reports_nothing_pending() {
        let e = tt::env().await;
        let s = snapshot(&e.app).await;
        assert_eq!(s["pending"], 0);
        assert_eq!(s["failing"], 0);
        assert_eq!(s["soonest"], Value::Null);
        assert!(s["items"].as_array().unwrap().is_empty());
    }
}
