//! 持久 intent（#355，設計見該票）：多步驟動作「已承諾、可能只做了一半」的紀錄。
//!
//! 動作在第一個不可逆步驟**之前**先 commit 一列 `pending`；正常完成標 `done`。daemon 中途死掉的話，開機由各路徑的 recovery 讀這張表接續：
//! `restart_intents`（重啟）、`delete_intents`（delete_bot／delete_project）、`promote_intents`（promote）。這個模組只有表的操作。
//!
//! 規則（給之後的接線者）：
//! - intent 的語意是「這件事**可能**做了任意一部分」，不是「已經做了」；續做必須同時處理「什麼都還沒做」。
//! - 續做靠檢查世界（DB／herdr 的實際狀態）決定下一步，不靠 intent 記到哪一步（`step` 欄位沒人寫，保留給之後顯示用）。
//! - 認領用 CAS（[`claim`]）：多個行程／重入只有一個做得到。
//! - 讀不到＝重試，不是「不用做」；超過 [`MAX_ATTEMPTS`] 或 `expires_at` 才 `failed`（並由呼叫端推 AGM inbox）。

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

/// 不管補不補得完，開著超過這麼久一律收掉（`created_at` 起算）。見 [`expire_overdue`] 的第二條。
pub const HARD_TTL_SECS: i64 = 7 * 24 * 3600;

/// 開著、而且該放棄的標成 `failed`，回它們（呼叫端推 AGM inbox）。三條各自獨立，中一條就收：
///
/// 1. **`owner_boot = boot` 且過了 `expires_at`**（issue #508）：`expires_at` 是建立時刻加 TTL，時鐘在
///    daemon 死著的時候照走——而 daemon 死著正是 intent 存在的唯一理由。以前開機對帳的第一步就是這一支，
///    於是「刪除定案之後 daemon 停超過 TTL」的那件會在**任何一次補完嘗試之前**被收成 `failed`，
///    `delete_intents::recover_host` 的 [`open`] 再也撈不到它：母 bot 已軟刪、child 還活著（#298 的
///    「看不到的活 bot」），再按一次刪除只得 404。加上 owner 條件之後，期限等於「只算這顆 daemon 在線的
///    時間」：上一顆 boot 留下的先讓這一輪 recovery 認領（`claim` 把 `owner_boot` 換成自己）、真的補一次，
///    補不完才輪到下一次對帳收掉。「試幾次就放棄」照舊由 [`MAX_ATTEMPTS`]／[`record_failure`] 負責，
///    而 `claim` 每次都加 `attempts`，所以每次 boot 都死在半路也會在 5 次內收斂。
/// 2. **`created_at` 早於 `now - `[`HARD_TTL_SECS`]**：第 1 條的 owner 條件有一種會永遠 pending（#508 複看）——
///    認領只發生在 `delete_intents::recover_host`，而它唯一的呼叫點（`reconcile::autostart_after_reconcile`）
///    前面有「對帳沒成功就不跑」。那台主機再也沒有成功對帳過（機器報廢、從 `config.toml` 拿掉、改名）的話，
///    永遠不 claim → `owner_boot` 停在死掉的 boot → 第 1 條一輩子撈不到 → `attempts` 不增加 →
///    [`MAX_ATTEMPTS`] 那條收斂路徑也到不了，沒有 `failed`、沒有通知。這一條是最後的收斂，不看 owner。
/// 3. **`host` 已經不在 `known_hosts` 裡、而且過了 `expires_at`**：主機從 config 拿掉之後那件 intent 不會再有人
///    補，直接收掉並通知，不必等第 2 條的七天。`known_hosts` 是空的（還沒 `apply_config`）就整條不算——
///    寧可晚一輪，也不要在設定重載的空窗裡把每一件遠端 intent 都判死。
///
/// 三條都走 [`fail_and_notify`]：標記與 AGM inbox 同生共死，不會有「放棄了卻沒人知道」。還開著、卻已經
/// 過了 `expires_at` 的那些（等下一輪 recovery）由 `due_actions` 的 `intent` 那一列看得到。
pub async fn expire_overdue(pool: &SqlitePool, now: &str, boot: &str, known_hosts: &[String]) -> Result<Vec<Intent>> {
    let hard_cutoff = (chrono::Utc::now() - chrono::Duration::seconds(HARD_TTL_SECS)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut out = Vec::new();
    for i in open(pool).await? {
        let overdue = crate::db::cmp_ts(&i.expires_at, now).is_le();
        let why = if crate::db::cmp_ts(&i.created_at, &hard_cutoff).is_le() {
            Some(format!("still not completed {} days after it was committed; giving up", HARD_TTL_SECS / 86_400))
        } else if !overdue {
            None
        } else if i.owner_boot.as_deref() == Some(boot) {
            Some("expired before it could be completed".to_string())
        } else if !known_hosts.is_empty() && !known_hosts.iter().any(|h| h == &i.host) {
            Some(format!("host `{}` is no longer configured; nobody will ever complete this", i.host))
        } else {
            None
        };
        let Some(why) = why else { continue };
        if fail_and_notify(pool, &i.id, &why).await? {
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
        assert!(claim(&p, &late.id, "boot-A").await.unwrap());
        assert!(claim(&p, &fine.id, "boot-A").await.unwrap());
        let expired = expire_overdue(&p, &crate::db::now(), "boot-A", &["local".into()]).await.unwrap();
        assert_eq!(expired.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), vec![late.id.as_str()]);
        assert_eq!(get(&p, &late.id).await.unwrap().unwrap().status, "failed");
        assert_eq!(get(&p, &fine.id).await.unwrap().unwrap().status, "running");
        assert_eq!(sweep_finished(&p).await.unwrap(), 0, "剛結束的不清");
        sqlx::query("UPDATE intents SET updated_at = '2020-01-01T00:00:00.000Z' WHERE id = ?").bind(&late.id).execute(&p).await.unwrap();
        assert_eq!(sweep_finished(&p).await.unwrap(), 1, "failed 超過 30 天才清");
        p.close().await;
        std::fs::remove_dir_all(dir).ok();
    }

    /// **#508**：`expires_at` 的時鐘在 daemon 死著的時候照走，而 daemon 死著正是 intent 存在的理由。
    /// 上一顆 boot 留下的、早就過期的那件，這一顆 boot **還沒認領過**就不准收——不然開機對帳的第一步
    /// 就把它收成 `failed`，後面的 recovery 連一次補完都跑不到。認領（真的試過一次）之後才輪到 TTL。
    #[tokio::test]
    async fn an_intent_left_by_a_dead_boot_is_not_expired_before_this_boot_has_tried_it() {
        let (p, dir) = pool().await;
        let Inserted::New(i) = insert(&p, "delete_bot", "bot-1", "local", &json!({}), -3600).await.unwrap() else { panic!() };
        assert!(claim(&p, &i.id, "boot-dead").await.unwrap(), "上一顆 daemon 自己認領過");

        // 開機對帳的第一步：這顆 boot 還沒碰過它，一件都不收。
        assert!(expire_overdue(&p, &crate::db::now(), "boot-new", &["local".into()]).await.unwrap().is_empty());
        assert_eq!(get(&p, &i.id).await.unwrap().unwrap().status, "running", "還開著，recovery 撈得到");
        assert!(open(&p).await.unwrap().iter().any(|x| x.id == i.id));

        // recovery 認領（＝真的補了一次）之後才輪到 TTL。
        assert!(claim(&p, &i.id, "boot-new").await.unwrap());
        let expired = expire_overdue(&p, &crate::db::now(), "boot-new", &["local".into()]).await.unwrap();
        assert_eq!(expired.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(), vec![i.id.as_str()]);
        assert_eq!(get(&p, &i.id).await.unwrap().unwrap().status, "failed");
        p.close().await;
        std::fs::remove_dir_all(dir).ok();
    }

    /// **#508 複看**：第 1 條的 owner 條件有一種會永遠 pending——認領只發生在對帳成功之後的 recovery，
    /// 那台主機再也沒有成功對帳過（機器報廢、從 config 拿掉、改名）就永遠不 claim，`owner_boot` 停在
    /// 死掉的 boot、`attempts` 不增加，連 `MAX_ATTEMPTS` 那條收斂路徑都到不了：沒有 `failed`、沒有通知。
    /// 兩條後路各釘一次：主機不在 config 就直接收；主機還在、但放了超過 `HARD_TTL_SECS` 也一定收。
    #[tokio::test]
    async fn an_intent_nobody_will_ever_claim_still_converges_to_failed_and_notifies() {
        let (p, dir) = pool().await;
        let host_gone = {
            let Inserted::New(i) = insert(&p, "delete_bot", "bot-1", "rusty-box", &json!({}), -5).await.unwrap() else { panic!() };
            assert!(claim(&p, &i.id, "boot-dead").await.unwrap());
            i
        };
        let still_configured = {
            let Inserted::New(i) = insert(&p, "delete_bot", "bot-2", "local", &json!({}), -5).await.unwrap() else { panic!() };
            assert!(claim(&p, &i.id, "boot-dead").await.unwrap());
            i
        };

        // 主機清單還沒建起來（空的）：一件都不判死，寧可晚一輪。
        assert!(expire_overdue(&p, &crate::db::now(), "boot-new", &[]).await.unwrap().is_empty());

        // `rusty-box` 已經不在 config 裡：沒有人會再來補它，現在就收掉並通知。
        let expired = expire_overdue(&p, &crate::db::now(), "boot-new", &["local".into()]).await.unwrap();
        assert_eq!(expired.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(), vec![host_gone.id.as_str()]);
        assert!(get(&p, &host_gone.id).await.unwrap().unwrap().last_error.unwrap().contains("no longer configured"));
        assert_eq!(get(&p, &still_configured.id).await.unwrap().unwrap().status, "running", "主機還在＝等 recovery，不收");

        // 主機還在、卻放了超過絕對期限：最後的收斂，不看 owner 也不看誰會來補。
        let long_ago = (chrono::Utc::now() - chrono::Duration::seconds(HARD_TTL_SECS + 60)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        sqlx::query("UPDATE intents SET created_at = ? WHERE id = ?").bind(&long_ago).bind(&still_configured.id).execute(&p).await.unwrap();
        let expired = expire_overdue(&p, &crate::db::now(), "boot-new", &["local".into()]).await.unwrap();
        assert_eq!(expired.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(), vec![still_configured.id.as_str()]);
        assert_eq!(get(&p, &still_configured.id).await.unwrap().unwrap().status, "failed");

        // 兩件都有人被通知（放棄不會沒人知道）。
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE kind = 'intent_failed'").fetch_one(&p).await.unwrap();
        assert_eq!(n, 2);
        p.close().await;
        std::fs::remove_dir_all(dir).ok();
    }
}
