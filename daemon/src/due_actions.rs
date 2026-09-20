//! 「daemon 接下來要做什麼、什麼一直做不成」的單一視窗（issue #75 驗收第 5 條、issue #97）。
//!
//! **這裡不是排程器，一列都不寫。** 到期動作與重試本來就已經落在 DB 上了——每一條都掛在自己那張
//! 表的欄位，記憶體 timer 只是加速，重啟後由掃描或重掛補回來（見下面 [`SOURCES`] 的對照）。
//!
//! 執行端**刻意**沒有統一（issue #97）。三個執行者對應三種不同的延遲與鎖需求，收成一個迴圈只會
//! 在裡面重新長出同樣三套政策：
//!
//! | 執行者 | 節奏 | 為什麼不能併 |
//! |---|---|---|
//! | 總管 tick（`supervisor::controller`） | 10 秒輪詢 | 交辦重送／等額度／協調者補送／看門狗都在這裡，序列化跑是刻意的 |
//! | 排隊 prompt 的重試 | 每顆 bot 一個到期時刻的 timer | 必須在**那顆 bot 的鎖**裡跑，而且要準時：晚 10 秒送就是使用者多等 10 秒 |
//! | hook 收件匣 worker | notify 叫醒（5 秒輪詢只是保險） | Stop hook 要立刻處理，改成 10 秒輪詢等於每個回合的收尾都慢 10 秒 |
//!
//! 所以這個模組只統一**讀取端**：把六處讀成同一份摘要。

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
    pub(crate) fn json(&self, now: &str) -> Value {
        json!({
            "kind": self.kind,
            "entity_id": self.entity_id,
            "due_at": self.due_at,
            // 到期了還在這裡＝掃描還沒輪到它（正常）或它一直失敗（看 attempts／last_error）。
            // 比時刻，不比字串：舊資料的秒格式（`…:00Z`）與現在的毫秒格式混存（issue #101）。
            "overdue": self.due_at.as_deref().is_some_and(|d| crate::db::cmp_ts(d, now).is_le()),
            "attempts": self.attempts,
            "last_error": self.last_error,
        })
    }
}

/// 一種到期動作在 DB 裡長什麼樣。計數、最早一筆、失敗樣本**三種讀法都從這一份長出來**，
/// 所以不會再出現「某一種讀法漏了一個來源」或「數字跟清單對不起來」。
struct Source {
    kind: &'static str,
    /// `FROM` 之後那一段（含別名）。
    from: &'static str,
    id: &'static str,
    due: &'static str,
    attempts: &'static str,
    /// `NULL` = 這張表沒有存失敗原因。
    error: &'static str,
    /// `WHERE` 之後那一段：什麼樣的列算「還沒做完」。
    filter: &'static str,
}

/// 六處到期時間的來源，以及重啟後是誰把它接回來：
///
/// | 動作 | 存在哪 | 重啟後誰接回來 |
/// |---|---|---|
/// | 排隊 prompt 的重試 | `turns.next_flush_at` | `reconcile::rearm_progress` → `lifecycle::rearm_queue_retries` |
/// | 交辦重送 | `supervisor_assignments.next_attempt_at` | 總管 tick（開機先 `reconcile`） |
/// | 等額度回來 | `supervisor_assignments.resume_at` | `controller::resume_quota_blocked` ＋開機回填 |
/// | 協調者補送 | `supervisor_inbox.notify_next_at` | 總管 tick |
/// | 總管看門狗 | `supervisors.watchdog_next_at` | 總管 tick |
/// | hook 事件 | `hook_events.next_attempt_at` | `hook_inbox::spawn_worker`（先 drain 再等） |
/// | 持久 intent | `intents.expires_at`（開著的） | `restart_intents::recover_host`（開機／主機重連對帳後） |
/// | 遠端已刪 bot 目錄的清理 | `remote_bot_dir_purges`（欠著的從 DB 推導） | `remote_purge`：主機連上時掃一次＋每 5 分鐘 |
const SOURCES: &[Source] = &[
    // 排隊中的 prompt：`next_flush_at IS NULL` 的也算——那是 AGM 派工排進來、等回合結束事件送的，
    // 一樣是「還沒發生的事」，而且它正是重啟後最容易被忘記的一種（5fe77d5 那條路）。
    Source {
        kind: "queue_retry",
        from: "turns t",
        id: "t.id",
        due: "t.next_flush_at",
        attempts: "t.flush_retries",
        error: "NULL",
        filter: "t.status = 'queued'",
    },
    // 交辦重送與等額度分成兩種 kind：「送不出去在退避」跟「額度沒了在等」對看的人是兩件事，
    // 混成一個數字就看不出該去修哪個。「未結案」用 `OPEN_STATES`，不自己抄一份清單。
    Source {
        kind: "assignment_retry",
        from: "supervisor_assignments",
        id: "id",
        due: "next_attempt_at",
        attempts: "attempts",
        error: "error",
        filter: "next_attempt_at IS NOT NULL AND status IN ({open}) AND status != 'quota_blocked'",
    },
    Source {
        kind: "quota_resume",
        from: "supervisor_assignments",
        id: "id",
        due: "resume_at",
        attempts: "attempts",
        error: "error",
        filter: "status = 'quota_blocked'",
    },
    Source {
        kind: "responder_notify",
        from: "supervisor_inbox",
        id: "id",
        due: "notify_next_at",
        attempts: "notify_attempts",
        error: "NULL",
        filter: "notify_next_at IS NOT NULL AND state != 'handled'",
    },
    Source {
        kind: "supervisor_watchdog",
        from: "supervisors",
        id: "id",
        due: "watchdog_next_at",
        attempts: "watchdog_attempts",
        error: "NULL",
        filter: "watchdog_next_at IS NOT NULL",
    },
    // 遠端已刪 bot 的目錄還沒清掉（#349）：欠著的清理從 DB 推導，這裡只列已經失敗過、記了原因的那些。
    Source {
        kind: "remote_bot_dir_purge",
        from: "remote_bot_dir_purges",
        id: "bot_id",
        due: "next_attempt_at",
        attempts: "attempts",
        error: "last_error",
        filter: "purged_at IS NULL",
    },
    // 持久 intent（#355）：還沒補完的多步驟動作；到期＝`expires_at`（放置太久就放棄並通知），開機由 `restart_intents::recover_host` 接回。
    Source {
        kind: "intent",
        from: "intents",
        id: "id",
        due: "expires_at",
        attempts: "attempts",
        error: "last_error",
        filter: "status IN ('pending','running')",
    },
    Source {
        kind: "hook_event",
        from: "hook_events",
        id: "id",
        due: "next_attempt_at",
        attempts: "attempts",
        error: "last_error",
        filter: "processed_at IS NULL",
    },
];

impl Source {
    /// `{open}` 佔位換成 `OPEN_STATES`（清單只准有一份，見 `supervisor::store::sql_list`）。
    fn filter_sql(&self) -> String {
        self.filter.replace(
            "{open}",
            &crate::supervisor::store::sql_list(&crate::supervisor::store::OPEN_STATES),
        )
    }

    fn select(&self) -> String {
        format!(
            "SELECT '{}' AS kind, {} AS entity_id, {} AS due_at, {} AS attempts, {} AS last_error
             FROM {} WHERE {}",
            self.kind,
            self.id,
            self.due,
            self.attempts,
            self.error,
            self.from,
            self.filter_sql()
        )
    }
}

/// `items` 最多擺這麼多。這是給人看的樣本，**數字不從它算**。
const SAMPLE: i64 = 20;
/// 試過這麼多次還沒成功就算「一直做不成」。
pub const FAILING_ATTEMPTS: i64 = 3;
/// [`pending`] 每一類最多列這麼多（列清單用；摘要的數字走 [`counts`]，不吃這個上限）。
pub(crate) const PER_KIND: i64 = 50;

/// 每一類的**精確**數量。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub pending: i64,
    pub overdue: i64,
    pub failing: i64,
}

/// 用 SQL 聚合直接算，**不經過任何上限**。
///
/// issue #97：以前這三個數字是從「每類最多 50 筆」的清單裡數出來的，所以積壓一多就會少報——
/// 500 件卡住的 hook 事件在畫面上長得跟 50 件一樣。而且 `queue_retry`／`quota_resume`／`hook_event`
/// 三處是 `ORDER BY COALESCE(due, '')`，空字串排在任何日期前面：沒有排定時間的列超過 50 筆時，
/// **所有有時間的都會被擠掉**，`soonest` 與 `overdue` 剛好在最需要它們的時候變瞎。
pub async fn counts(pool: &SqlitePool) -> Result<std::collections::BTreeMap<&'static str, Counts>> {
    let now = crate::db::now();
    let mut out = std::collections::BTreeMap::new();
    for s in SOURCES {
        let row: (i64, i64, i64) = sqlx::query_as(&format!(
            "SELECT COUNT(*),
                    COALESCE(SUM(CASE WHEN {due} IS NOT NULL AND {due_ts} <= ? THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN {attempts} >= ? THEN 1 ELSE 0 END), 0)
             FROM {from} WHERE {filter}",
            due = s.due,
            due_ts = crate::db::ts_sql(s.due),
            attempts = s.attempts,
            from = s.from,
            filter = s.filter_sql()
        ))
        .bind(&now)
        .bind(FAILING_ATTEMPTS)
        .fetch_one(pool)
        .await?;
        out.insert(s.kind, Counts { pending: row.0, overdue: row.1, failing: row.2 });
    }
    Ok(out)
}

/// 最早到期的那一件（只看**有排定時間**的）。每一類各問一筆再比，所以不受上限影響。
pub async fn soonest(pool: &SqlitePool) -> Result<Option<DueAction>> {
    let mut best: Option<DueAction> = None;
    for s in SOURCES {
        // 同一種來源裡新舊格式也會混存：`ORDER BY` 要照時刻排（issue #101）。
        let row: Option<DueAction> = sqlx::query_as(&format!(
            "{} AND {due} IS NOT NULL ORDER BY {due_ts} LIMIT 1",
            s.select(),
            due = s.due,
            due_ts = crate::db::ts_sql(s.due)
        ))
        .fetch_optional(pool)
        .await?;
        if let Some(r) = row {
            let earlier = |r: &DueAction, b: &str| r.due_at.as_deref().is_some_and(|d| crate::db::cmp_ts(d, b).is_lt());
            if best.as_ref().and_then(|b| b.due_at.as_deref()).is_none_or(|b| earlier(&r, b)) {
                best = Some(r);
            }
        }
    }
    Ok(best)
}

/// 一直做不成的那些，試最多次的排前面。**是樣本，不是全部**——全部有多少看 `counts`。
pub async fn failing_sample(pool: &SqlitePool, limit: i64) -> Result<Vec<DueAction>> {
    let mut out: Vec<DueAction> = Vec::new();
    for s in SOURCES {
        out.extend(
            sqlx::query_as::<_, DueAction>(&format!(
                "{} AND {attempts} >= ? ORDER BY {attempts} DESC LIMIT ?",
                s.select(),
                attempts = s.attempts
            ))
            .bind(FAILING_ATTEMPTS)
            .bind(limit)
            .fetch_all(pool)
            .await?,
        );
    }
    out.sort_by(|a, b| b.attempts.cmp(&a.attempts));
    out.truncate(limit as usize);
    Ok(out)
}

/// 列出還沒做完的動作，最早到期的在前；**每一類最多 [`PER_KIND`] 筆**。
///
/// 給「想看有哪些」用。摘要的數字不走這裡（那會少報，見 [`counts`]）。
pub async fn pending(pool: &SqlitePool) -> Result<Vec<DueAction>> {
    let mut out: Vec<DueAction> = Vec::new();
    for s in SOURCES {
        out.extend(
            sqlx::query_as::<_, DueAction>(&format!(
                "{} ORDER BY COALESCE({due_ts}, '') LIMIT ?",
                s.select(),
                due_ts = crate::db::ts_sql(s.due)
            ))
            .bind(PER_KIND)
            .fetch_all(pool)
            .await?,
        );
    }
    // 沒有排定時間的排最後：它們在等事件，不在等時鐘。
    out.sort_by(|a, b| match (&a.due_at, &b.due_at) {
        (Some(x), Some(y)) => crate::db::cmp_ts(x, y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    Ok(out)
}

/// 給健康快照的那一段。讀不到就回 `null` 而不是讓整份快照失敗：這是觀測用的，不該變成新的故障點。
pub async fn snapshot(app: &Arc<App>) -> Value {
    let (by_kind, soon, items) = match tokio::try_join!(
        counts(&app.db),
        soonest(&app.db),
        failing_sample(&app.db, SAMPLE)
    ) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = ?e, "cannot read due actions");
            return Value::Null;
        }
    };
    let now = crate::db::now();
    let total = |f: fn(&Counts) -> i64| by_kind.values().map(f).sum::<i64>();
    let failing = total(|c| c.failing);
    json!({
        "pending": total(|c| c.pending),
        // 到期了還在名單上：掃描還沒輪到（正常），或它一直失敗。兩者都看 `failing` 與 `items`。
        "overdue": total(|c| c.overdue),
        "failing": failing,
        // 這三個數字都是 SQL 聚合算的，不吃 `items` 的上限。
        "by_kind": by_kind
            .iter()
            .map(|(k, c)| (k.to_string(), json!({"pending": c.pending, "overdue": c.overdue, "failing": c.failing})))
            .collect::<serde_json::Map<_, _>>(),
        "soonest": soon.as_ref().map(|r| r.json(&now)),
        // 一直做不成的樣本（試最多次的在前）。`items.len() < failing` 就是還有更多沒列出來。
        "items": items.iter().map(|r| r.json(&now)).collect::<Vec<_>>(),
        "items_truncated": failing > items.len() as i64,
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

    /// 插一筆還沒處理的 hook 事件；`due` 是 `None` 就是「沒有排定時間」。
    async fn hook_event(app: &Arc<App>, id: &str, due: Option<&str>, attempts: i64) {
        sqlx::query(
            "INSERT INTO hook_events (id, bot_id, provider, source, body_json, received_at, attempts, next_attempt_at)
             VALUES (?, 'b1', 'claude', 'http', '{}', ?, ?, ?)",
        )
        .bind(id)
        .bind(crate::db::now())
        .bind(attempts)
        .bind(due)
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
        assert!(kinds.contains(&"queue_retry") && kinds.contains(&"assignment_retry") && kinds.contains(&"quota_resume"), "{kinds:?}");
        assert_eq!(rows[0].entity_id, "t-queued", "最早到期的排前面：{kinds:?}");
    }

    #[tokio::test]
    async fn closed_work_is_not_pending_any_more() {
        let e = tt::env().await;
        assignment(&e.app, "done", "completed", Some("2026-09-18T10:00:00Z"), None, 2).await;
        assert!(pending(&e.app.db).await.unwrap().iter().all(|r| r.entity_id != "done"));
    }

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
        assert_eq!(s["by_kind"]["assignment_retry"]["pending"], 2);
        assert_eq!(s["soonest"]["entity_id"], "stuck", "最早到期的是它");
        let items = s["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["entity_id"], "stuck");
        assert_eq!(items[0]["last_error"], "boom", "要看得到為什麼做不成");
        assert_eq!(s["items_truncated"], false);
    }

    #[tokio::test]
    async fn an_idle_daemon_reports_nothing_pending() {
        let e = tt::env().await;
        let s = snapshot(&e.app).await;
        assert_eq!(s["pending"], 0);
        assert_eq!(s["failing"], 0);
        assert_eq!(s["soonest"], Value::Null);
        assert!(s["items"].as_array().unwrap().is_empty());
        assert_eq!(s["items_truncated"], false);
    }

    /// issue #97：積壓超過每類上限時，數字仍要是**真的**。
    ///
    /// 以前 `pending` 是清單長度，所以上限是多少就報多少——500 件卡住的 hook 事件長得跟 50 件一樣。
    #[tokio::test]
    async fn the_numbers_are_exact_well_past_the_listing_cap() {
        let e = tt::env().await;
        let app = &e.app;
        let n = PER_KIND + 17;
        for i in 0..n {
            hook_event(app, &format!("h{i:04}"), Some("2000-01-01T00:00:00Z"), FAILING_ATTEMPTS).await;
        }
        let s = snapshot(app).await;
        assert_eq!(s["pending"], n, "少報就是在最需要的時候騙人");
        assert_eq!(s["overdue"], n);
        assert_eq!(s["failing"], n);
        assert_eq!(s["by_kind"]["hook_event"]["pending"], n);
        // 樣本仍然有上限，但會講明還有更多。
        assert_eq!(s["items"].as_array().unwrap().len(), SAMPLE as usize);
        assert_eq!(s["items_truncated"], true, "列不完要說");
        // 列清單那支照舊有上限（它是拿來看的，不是拿來數的）。
        assert_eq!(pending(&app.db).await.unwrap().len(), PER_KIND as usize);
    }

    /// 沒有排定時間的列爆量時，**有時間的那筆仍然找得到**。
    ///
    /// `pending` 是 `ORDER BY COALESCE(due, '')`，空字串排在任何日期前面：以前 `soonest` 從那份
    /// 被截斷的清單裡挑，NULL 的超過上限就把所有有時間的擠掉了，剛好在積壓時變瞎。
    #[tokio::test]
    async fn a_pile_of_undated_work_cannot_hide_the_soonest_deadline() {
        let e = tt::env().await;
        let app = &e.app;
        for i in 0..(PER_KIND + 5) {
            hook_event(app, &format!("n{i:04}"), None, 0).await;
        }
        hook_event(app, "dated", Some("2030-01-01T00:00:00Z"), 0).await;

        let soon = soonest(&app.db).await.unwrap().expect("有時間的那筆要找得到");
        assert_eq!(soon.entity_id, "dated");
        let s = snapshot(app).await;
        assert_eq!(s["soonest"]["entity_id"], "dated");
        assert_eq!(s["pending"], PER_KIND + 6, "沒排定時間的也算待辦");
    }

    /// 重啟後那些待辦還在——因為它們一開始就不是存在行程裡的（issue #75 的核心主張）。
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
}
