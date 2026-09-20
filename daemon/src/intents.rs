//! 持久 intent（#355，設計見該票）：多步驟動作「已承諾、可能只做了一半」的紀錄。
//!
//! 動作在第一個不可逆步驟**之前**先 commit 一列 `pending`；正常完成標 `done`。daemon 中途死掉的話，開機由 recovery 讀這張表接續
//! （P2 起逐條路徑接上；**這個模組目前只有表的操作，沒有任何路徑呼叫它**）。
//!
//! 規則（給之後的接線者）：
//! - intent 的語意是「這件事**可能**做了任意一部分」，不是「已經做了」；續做必須同時處理「什麼都還沒做」。
//! - 續做不靠 `step` 決定下一步，靠檢查世界（DB／herdr 的實際狀態）；`step` 只是顯示用的提示。
//! - 認領用 CAS（[`claim`]）：多個行程／重入只有一個做得到。
//! - 讀不到＝重試，不是「不用做」；超過 [`MAX_ATTEMPTS`] 或 `expires_at` 才 `failed`（並由呼叫端推 AGM inbox）。

// P1：只有 `recent`（`GET /api/intents`）被用到，其餘等 P2 起逐條路徑接線。
#![allow(dead_code)]

use anyhow::Result;
use serde_json::Value;
use sqlx::SqlitePool;

/// 補做失敗最多試幾次就放棄（使用者 2026-09-20 裁示：重試幾次就放棄、不無限重試）。
pub const MAX_ATTEMPTS: i64 = 5;

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct Intent {
    pub id: String,
    pub kind: String,
    pub subject_id: String,
    pub host: String,
    pub payload_json: String,
    pub step: Option<String>,
    pub status: String,
    pub owner_boot: Option<String>,
    pub attempts: i64,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub expires_at: String,
}

impl Intent {
    pub fn payload(&self) -> Value {
        serde_json::from_str(&self.payload_json).unwrap_or(Value::Null)
    }
}

/// 同一目標同一種動作已經有一件開著：回那一件（重按＝同一件，天然冪等）。
pub enum Inserted {
    New(Intent),
    AlreadyOpen(Intent),
}

pub async fn get(pool: &SqlitePool, id: &str) -> Result<Option<Intent>> {
    Ok(sqlx::query_as("SELECT * FROM intents WHERE id = ?").bind(id).fetch_optional(pool).await?)
}

async fn open_for(pool: &SqlitePool, kind: &str, subject_id: &str) -> Result<Option<Intent>> {
    Ok(sqlx::query_as("SELECT * FROM intents WHERE kind = ? AND subject_id = ? AND status IN ('pending','running')")
        .bind(kind)
        .bind(subject_id)
        .fetch_optional(pool)
        .await?)
}

/// 寫一列 `pending`。`ttl_secs`＝放棄期限（超過還沒做完就 `failed`）。
pub async fn insert(pool: &SqlitePool, kind: &str, subject_id: &str, host: &str, payload: &Value, ttl_secs: i64) -> Result<Inserted> {
    let now = crate::db::now();
    let expires = (chrono::Utc::now() + chrono::Duration::seconds(ttl_secs)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let id = crate::db::ulid();
    let res = sqlx::query(
        "INSERT INTO intents (id, kind, subject_id, host, payload_json, status, created_at, updated_at, expires_at)
         VALUES (?,?,?,?,?,'pending',?,?,?)",
    )
    .bind(&id)
    .bind(kind)
    .bind(subject_id)
    .bind(host)
    .bind(payload.to_string())
    .bind(&now)
    .bind(&now)
    .bind(&expires)
    .execute(pool)
    .await;
    match res {
        Ok(_) => Ok(Inserted::New(get(pool, &id).await?.expect("just inserted"))),
        Err(e) if e.as_database_error().is_some_and(|d| d.is_unique_violation()) => match open_for(pool, kind, subject_id).await? {
            Some(existing) => Ok(Inserted::AlreadyOpen(existing)),
            // 唯一衝突卻讀不到那一列（剛好被收掉）：當成寫入失敗，讓呼叫端重試，不當成「已經有一件」。
            None => Err(anyhow::anyhow!("intent insert conflicted but the open row vanished; retry")),
        },
        Err(e) => Err(e.into()),
    }
}

/// 認領（CAS）：`pending`，或 `running` 但 owner 不是這次 boot（上個行程死了）。回 `true`＝這次拿到、可以做。
pub async fn claim(pool: &SqlitePool, id: &str, boot: &str) -> Result<bool> {
    let n = sqlx::query(
        "UPDATE intents SET status = 'running', owner_boot = ?1, attempts = attempts + 1, updated_at = ?2
         WHERE id = ?3 AND (status = 'pending' OR (status = 'running' AND COALESCE(owner_boot, '') <> ?1))",
    )
    .bind(boot)
    .bind(crate::db::now())
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n == 1)
}

/// 進度提示（顯示用，續做不靠它）。
pub async fn set_step(pool: &SqlitePool, id: &str, step: &str) -> Result<()> {
    sqlx::query("UPDATE intents SET step = ?, updated_at = ? WHERE id = ? AND status IN ('pending','running')")
        .bind(step)
        .bind(crate::db::now())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

async fn finish(pool: &SqlitePool, id: &str, status: &str, err: Option<&str>) -> Result<bool> {
    let n = sqlx::query(
        "UPDATE intents SET status = ?, last_error = COALESCE(?, last_error), updated_at = ? WHERE id = ? AND status IN ('pending','running')",
    )
    .bind(status)
    .bind(err)
    .bind(crate::db::now())
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n == 1)
}

pub async fn complete(pool: &SqlitePool, id: &str) -> Result<bool> {
    finish(pool, id, "done", None).await
}

/// 承諾點之前就死掉、世界沒變：等於沒發生。
pub async fn abandon(pool: &SqlitePool, id: &str, why: &str) -> Result<bool> {
    finish(pool, id, "abandoned", Some(why)).await
}

/// 放棄（重試用完或過期）。呼叫端負責推 AGM inbox。
pub async fn fail(pool: &SqlitePool, id: &str, err: &str) -> Result<bool> {
    finish(pool, id, "failed", Some(err)).await
}

/// 放棄並**同一個交易**推 AGM inbox（`intent_failed`）：補做用完次數或過期了，不能只寫 log（使用者 2026-09-20 裁示）；
/// 標記與通知同生共死，不會有「失敗了卻沒人知道」。回 `true`＝這次真的由它標成 failed。
pub async fn fail_and_notify(pool: &SqlitePool, id: &str, err: &str) -> Result<bool> {
    let Some(i) = get(pool, id).await? else { return Ok(false) };
    let mut tx = pool.begin().await?;
    let n = sqlx::query("UPDATE intents SET status = 'failed', last_error = ?, updated_at = ? WHERE id = ? AND status IN ('pending','running')")
        .bind(err)
        .bind(crate::db::now())
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n == 1 {
        crate::supervisor::store::push_inbox_tx(
            &mut tx,
            &format!("intent_failed:{id}"),
            "intent_failed",
            None,
            Some(&i.subject_id),
            None,
            &serde_json::json!({
                "intent_id": i.id, "kind": i.kind, "subject_id": i.subject_id, "host": i.host,
                "attempts": i.attempts, "last_error": err,
                "message": format!("{} 沒能補完（已試 {} 次）：{err}。請人工確認 {} 的狀態。", i.kind, i.attempts, i.subject_id),
            }),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(n == 1)
}

/// 這一次補做失敗了：記下原因、放回 `pending`（下一輪 recovery 再認領）；用完 [`MAX_ATTEMPTS`] 就直接 `failed`。
/// 回 `true`＝已經放棄（呼叫端該推 AGM inbox）。
pub async fn record_failure(pool: &SqlitePool, id: &str, err: &str) -> Result<bool> {
    let attempts: Option<i64> = sqlx::query_scalar("SELECT attempts FROM intents WHERE id = ?").bind(id).fetch_optional(pool).await?;
    if attempts.unwrap_or(0) >= MAX_ATTEMPTS {
        return fail_and_notify(pool, id, err).await;
    }
    sqlx::query("UPDATE intents SET status = 'pending', last_error = ?, updated_at = ? WHERE id = ? AND status = 'running'")
        .bind(err)
        .bind(crate::db::now())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(false)
}

/// 開著的（`pending`／`running`），舊的先。
pub async fn open(pool: &SqlitePool) -> Result<Vec<Intent>> {
    Ok(sqlx::query_as("SELECT * FROM intents WHERE status IN ('pending','running') ORDER BY created_at").fetch_all(pool).await?)
}

/// 開著卻過了 `expires_at` 的標成 `failed`，回它們（呼叫端推 AGM inbox）。
pub async fn expire_overdue(pool: &SqlitePool, now: &str) -> Result<Vec<Intent>> {
    let overdue: Vec<Intent> = sqlx::query_as(&format!(
        "SELECT * FROM intents WHERE status IN ('pending','running') AND {} <= ?",
        crate::db::ts_sql("expires_at")
    ))
    .bind(now)
        .fetch_all(pool)
        .await?;
    let mut out = Vec::new();
    for i in overdue {
        if fail_and_notify(pool, &i.id, "expired before it could be completed").await? {
            out.push(i);
        }
    }
    Ok(out)
}

/// 清收尾很久的：`done`／`abandoned` 留 24 小時，`failed` 留 30 天（要看得到哪件事沒做完）。回刪了幾列。
pub async fn sweep_finished(pool: &SqlitePool) -> Result<u64> {
    let ago = |secs: i64| (chrono::Utc::now() - chrono::Duration::seconds(secs)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let ts = crate::db::ts_sql("updated_at");
    let a = sqlx::query(&format!("DELETE FROM intents WHERE status IN ('done','abandoned') AND {ts} <= ?")).bind(ago(24 * 3600)).execute(pool).await?.rows_affected();
    let b = sqlx::query(&format!("DELETE FROM intents WHERE status = 'failed' AND {ts} <= ?")).bind(ago(30 * 24 * 3600)).execute(pool).await?.rows_affected();
    Ok(a + b)
}

/// 最近的 intent（含已結束的），新的先；給 `GET /api/intents`。
pub async fn recent(pool: &SqlitePool, limit: i64) -> Result<Vec<Intent>> {
    Ok(sqlx::query_as("SELECT * FROM intents ORDER BY created_at DESC LIMIT ?").bind(limit).fetch_all(pool).await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn pool() -> (SqlitePool, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("am-intents-{}", crate::db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        (crate::db::open(&dir.join("t.sqlite")).await.unwrap(), dir)
    }

    #[tokio::test]
    async fn one_open_intent_per_target_and_kind_and_a_retry_is_the_same_one() {
        let (p, dir) = pool().await;
        let Inserted::New(a) = insert(&p, "restart", "bot-1", "local", &json!({"resume": true}), 900).await.unwrap() else { panic!("first is new") };
        let Inserted::AlreadyOpen(again) = insert(&p, "restart", "bot-1", "local", &json!({}), 900).await.unwrap() else { panic!("second is the same") };
        assert_eq!(again.id, a.id);
        assert!(matches!(insert(&p, "delete_bot", "bot-1", "local", &json!({}), 900).await.unwrap(), Inserted::New(_)), "另一種動作不衝突");
        assert!(matches!(insert(&p, "restart", "bot-2", "local", &json!({}), 900).await.unwrap(), Inserted::New(_)), "另一個目標不衝突");
        assert_eq!(a.payload()["resume"], true);
        // 結束之後同一個目標可以再開一件。
        assert!(complete(&p, &a.id).await.unwrap());
        assert!(matches!(insert(&p, "restart", "bot-1", "local", &json!({}), 900).await.unwrap(), Inserted::New(_)));
        p.close().await;
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn claim_is_a_compare_and_swap_that_only_takes_over_a_dead_boots_intent() {
        let (p, dir) = pool().await;
        let Inserted::New(a) = insert(&p, "promote", "bot-1", "local", &json!({}), 900).await.unwrap() else { panic!() };
        assert!(claim(&p, &a.id, "boot-A").await.unwrap(), "pending 可以認領");
        assert!(!claim(&p, &a.id, "boot-A").await.unwrap(), "同一個 boot 不能重複認領（重入）");
        assert!(claim(&p, &a.id, "boot-B").await.unwrap(), "上個 boot 認領到一半死了：下一個 boot 接手");
        assert_eq!(get(&p, &a.id).await.unwrap().unwrap().attempts, 2);
        p.close().await;
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn a_failing_intent_is_retried_a_few_times_then_given_up() {
        let (p, dir) = pool().await;
        let Inserted::New(a) = insert(&p, "delete_bot", "bot-1", "local", &json!({}), 900).await.unwrap() else { panic!() };
        for n in 1..=MAX_ATTEMPTS {
            assert!(claim(&p, &a.id, &format!("boot-{n}")).await.unwrap());
            let gave_up = record_failure(&p, &a.id, "disk full").await.unwrap();
            assert_eq!(gave_up, n == MAX_ATTEMPTS, "第 {n} 次");
        }
        let done = get(&p, &a.id).await.unwrap().unwrap();
        assert_eq!((done.status.as_str(), done.last_error.as_deref()), ("failed", Some("disk full")));
        assert!(!claim(&p, &a.id, "boot-x").await.unwrap(), "failed 不再被認領");
        p.close().await;
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn overdue_open_intents_fail_and_finished_ones_are_swept_later() {
        let (p, dir) = pool().await;
        let Inserted::New(late) = insert(&p, "restart", "bot-1", "local", &json!({}), -5).await.unwrap() else { panic!() };
        let Inserted::New(fine) = insert(&p, "restart", "bot-2", "local", &json!({}), 900).await.unwrap() else { panic!() };
        let expired = expire_overdue(&p, &crate::db::now()).await.unwrap();
        assert_eq!(expired.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), vec![late.id.as_str()]);
        assert_eq!(get(&p, &late.id).await.unwrap().unwrap().status, "failed");
        assert_eq!(get(&p, &fine.id).await.unwrap().unwrap().status, "pending");
        assert_eq!(sweep_finished(&p).await.unwrap(), 0, "剛結束的不清");
        sqlx::query("UPDATE intents SET updated_at = '2020-01-01T00:00:00.000Z' WHERE id = ?").bind(&late.id).execute(&p).await.unwrap();
        assert_eq!(sweep_finished(&p).await.unwrap(), 1, "failed 超過 30 天才清");
        p.close().await;
        std::fs::remove_dir_all(dir).ok();
    }
}
