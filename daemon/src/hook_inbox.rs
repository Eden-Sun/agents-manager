//! Hook 的耐久收件匣（issue #70）。
//!
//! 以前 `POST /hook/{provider}` 是「驗 token → 丟背景 → 立刻回 200」：daemon 在回了 200 之後、
//! 背景處理完之前掛掉，那則事件就等於從來沒發生過。今天「Stop hook 沒來、turn 卡在 in_flight」
//! 那一類問題的根部就在這個空窗。遠端 spool 更直接：drain 的 ssh 腳本讀完就把遠端**唯一**那份
//! 副本刪掉，位元組還沒寫進本機任何地方（§11.4.3）。
//!
//! 這裡把**耐久收下**與**語意處理**分開：
//!
//! ```text
//! 本機 HTTP：  INSERT hook_events → COMMIT → 200      → worker → process()
//! 遠端 spool： claim（只讀不刪）→ INSERT → COMMIT → 刪遠端 → worker → process()
//! ```
//!
//! 換來三件事：
//! - `200` 代表「已經寫進 `hook_events`」。寫不進去就**不回 200**，送端（`hook_cmd::inner`）看到
//!   非 2xx 會把同一份 body 追加到 `hook-spool.jsonl`，之後由 replay 補回來——那就是重試的路。
//! - 遠端那份被刪掉時，本機一定已經 commit。
//! - `process_locked` 失敗不再等於事件消失：列還在，`attempts` / `last_error` 留著，worker 會再試。
//!
//! **StatusLine 不走這裡。** 它是「最新的贏」的單槽訊號：遠端寫的是 `hook-status.json`，
//! `setup.rs` 的腳本裡就寫明「never the spool, which is a queue」；本機送端 `statusline_cmd`
//! 是 fire-and-forget，根本不看回應也不會重送（「下一次重繪本來就更新」）。每次重繪都寫一列進 DB
//! 只會換來大量寫入，換不到任何保證——掉一格的代價就是晚一次重繪。

use crate::db;
use crate::hookrecv::HookBody;
use crate::state::App;
use anyhow::Result;
use sqlx::SqlitePool;
use std::sync::Arc;
use std::time::Duration;

/// 這一列是從哪條路收下的（只作診斷用，不影響處理）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `POST /hook/{provider}`
    Http,
    /// 本機 `hook-spool.jsonl` 重放
    Spool,
    /// 遠端 spool drain（§11.4.3）
    Remote,
}

impl Source {
    fn as_str(self) -> &'static str {
        match self {
            Source::Http => "http",
            Source::Spool => "spool",
            Source::Remote => "remote",
        }
    }
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS hook_events (
  id TEXT PRIMARY KEY,
  bot_id TEXT NOT NULL,
  provider TEXT NOT NULL,
  -- http | spool | remote
  source TEXT NOT NULL,
  -- 同一則事件重送要得到同一把鑰匙（見 `dedupe_key`）。NULL＝認不出身分，一律收下。
  dedupe_key TEXT,
  -- 整個 HookBody，worker 直接反序列化回來走 §6.7。
  body_json TEXT NOT NULL,
  received_at TEXT NOT NULL,
  -- NULL＝還沒處理完。重啟後就是靠這個欄位把上一輪沒做完的補回來。
  processed_at TEXT,
  attempts INTEGER NOT NULL DEFAULT 0,
  last_error TEXT,
  -- 失敗退避：早於這個時間不再試。
  next_attempt_at TEXT
);
"#;

/// 可重入：只有 `CREATE ... IF NOT EXISTS`，沒有 ALTER（這張表是新的，不必補舊欄位）。
pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    for stmt in SCHEMA.split(";\n") {
        let s = stmt.trim();
        if !s.is_empty() {
            sqlx::query(s).execute(pool).await?;
        }
    }
    for stmt in [
        // worker 每次都問「還有誰沒處理」，這是它的索引。
        "CREATE INDEX IF NOT EXISTS hook_events_pending ON hook_events(processed_at, next_attempt_at)",
        // 去重靠它：同一顆 bot 的同一把鑰匙只留一列。`dedupe_key IS NULL` 的不參加。
        "CREATE UNIQUE INDEX IF NOT EXISTS hook_events_dedupe
           ON hook_events(bot_id, dedupe_key) WHERE dedupe_key IS NOT NULL",
    ] {
        sqlx::query(stmt).execute(pool).await?;
    }
    Ok(())
}

/// 這則 hook 的身分：**同一則重送要得到同一把鑰匙，兩則不同的事件不可以撞在一起。**
///
/// 刻意不用雜湊：鑰匙本身在 DB 裡看得懂是哪一則，出事時查得動；payload 可以到 1 MiB，也不該整包進索引。
///
/// 組成是「送端蓋的時間 ＋ 事件名 ＋ session ＋ turn」。重送的是同一份 body，四項全同；兩則不同的
/// 事件至少會差在時間或 turn id 上（同一個 `prompt_id` 就是同一回合，不會是兩件事）。
///
/// `received_at` 是送端蓋的（`hook_cmd` 到毫秒、遠端 `hook.sh` 到秒），不是收到的時間——用收到的時間
/// 當鑰匙，重送就永遠是新的一列，去重會完全失效。
pub fn dedupe_key(body: &HookBody) -> Option<String> {
    let p = &body.payload;
    let pick = |keys: &[&str]| -> String {
        for k in keys {
            if let Some(v) = p.get(*k).and_then(|v| v.as_str()) {
                if !v.is_empty() {
                    return v.to_string();
                }
            }
        }
        String::new()
    };
    let received = body.received_at.as_deref().unwrap_or("");
    // 認不出時間的 body（手寫、舊版）一律收下：寧可多一列，也不要把兩則不同的事件併成一則。
    if received.is_empty() {
        return None;
    }
    let event = pick(&["hook_event_name", "hookEventName", "type"]);
    let session = pick(&["session_id", "sessionId", "thread-id"]);
    let turn = pick(&["prompt_id", "promptId", "turn-id", "turnId"]);
    Some(format!("{}|{}|{}|{}|{}", body.provider, received, event, session, turn))
}

/// 收下的結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accepted {
    /// 新的一列。
    Stored,
    /// 同一把鑰匙已經在收件匣裡了——這是**重送**，不是第二則事件。
    Duplicate,
}

impl Accepted {
    pub fn is_new(self) -> bool {
        self == Accepted::Stored
    }
}

/// 寫進收件匣並 commit。回傳成功＝這則事件已經耐久收下，呼叫端可以回 200 / 刪遠端副本了。
///
/// 重送（同 `dedupe_key`）走 `INSERT OR IGNORE`：不是錯誤，也不會變成第二列——呼叫端照樣回 200，
/// 因為那則事件的確已經在收件匣裡。
pub async fn accept(pool: &SqlitePool, body: &HookBody, source: Source) -> Result<Accepted> {
    let key = dedupe_key(body);
    let res = sqlx::query(
        "INSERT OR IGNORE INTO hook_events
           (id, bot_id, provider, source, dedupe_key, body_json, received_at, attempts)
         VALUES (?,?,?,?,?,?,?,0)",
    )
    .bind(db::ulid())
    .bind(&body.bot_id)
    .bind(&body.provider)
    .bind(source.as_str())
    .bind(&key)
    .bind(serde_json::to_string(body)?)
    .bind(body.received_at.clone().unwrap_or_else(db::now))
    .execute(pool)
    .await?;
    Ok(if res.rows_affected() == 0 { Accepted::Duplicate } else { Accepted::Stored })
}

/// 收件匣裡的一列（worker 用）。
#[derive(Debug, sqlx::FromRow)]
pub struct Pending {
    pub id: String,
    pub body_json: String,
    pub attempts: i64,
}

/// 還沒處理、而且退避時間到了的列，**照寫入順序**（`rowid`）。
///
/// 第二鍵不是 `id`：`id` 是 ULID，同一毫秒內的亂數段不保證遞增，事件順序會偶爾翻過來
/// （a4605b2 在 `mission_events` 上就是被這個咬的）。`rowid` 就是寫入順序。
pub async fn pending(pool: &SqlitePool, now: &str, limit: i64) -> Result<Vec<Pending>> {
    Ok(sqlx::query_as::<_, Pending>(
        "SELECT id, body_json, attempts FROM hook_events
         WHERE processed_at IS NULL AND (next_attempt_at IS NULL OR next_attempt_at <= ?)
         ORDER BY rowid LIMIT ?",
    )
    .bind(now)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

pub async fn mark_done(pool: &SqlitePool, id: &str) -> Result<()> {
    sqlx::query("UPDATE hook_events SET processed_at = ?, last_error = NULL WHERE id = ?")
        .bind(db::now())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// 處理失敗：列留著、記下原因與退避時間，下一輪再試。**不刪、不標成處理完**——
/// 「`process_locked()` 暫時失敗不等於事件消失」就是靠這裡。
pub async fn mark_failed(pool: &SqlitePool, id: &str, attempts: i64, err: &str) -> Result<()> {
    let wait = backoff(attempts);
    let next = chrono::Utc::now() + chrono::Duration::from_std(wait).unwrap_or_default();
    sqlx::query("UPDATE hook_events SET attempts = ?, last_error = ?, next_attempt_at = ? WHERE id = ?")
        .bind(attempts)
        .bind(clip(err))
        .bind(next.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// 再試幾次都不會變好的列（body 根本解不開）：記下原因收掉，不要無限重試佔住佇列前面。
async fn mark_dead(pool: &SqlitePool, id: &str, err: &str) -> Result<()> {
    sqlx::query("UPDATE hook_events SET processed_at = ?, last_error = ? WHERE id = ?")
        .bind(db::now())
        .bind(clip(err))
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

fn clip(s: &str) -> String {
    const MAX: usize = 500;
    if s.len() <= MAX {
        return s.to_string();
    }
    let mut end = MAX;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// 1s、2s、4s…最多 5 分鐘。
fn backoff(attempts: i64) -> Duration {
    let secs = 1u64 << attempts.clamp(0, 8) as u32;
    Duration::from_secs(secs.min(300))
}

/// 處理完的列留多久。留著是為了「這則到底進來過沒有」查得到；久了沒人看，而且 hook 量很大。
const KEEP_PROCESSED: Duration = Duration::from_secs(24 * 3600);
const POLL_EVERY: Duration = Duration::from_secs(5);
const BATCH: i64 = 64;

/// 唯一的消費者。`receive` / drain 只負責 commit 之後叫醒它，不自己處理——單一消費者才不必為
/// 「同一列被兩邊同時處理」另外加 claim 欄位。
pub fn spawn_worker(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            // 先做一輪再等：daemon 重啟後把上一輪沒處理完的補回來，就是這一行。
            match drain_once(&app).await {
                Ok(n) if n > 0 => tracing::info!(processed = n, "hook inbox drained"),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = ?e, "hook inbox drain failed"),
            }
            if let Err(e) = prune(&app.db).await {
                tracing::debug!(error = ?e, "hook inbox prune failed");
            }
            tokio::select! {
                _ = app.hook_inbox_wake.notified() => {}
                _ = tokio::time::sleep(POLL_EVERY) => {}
            }
        }
    });
}

/// 處理一批。回傳這一輪真的處理完幾列。
pub async fn drain_once(app: &Arc<App>) -> Result<usize> {
    let mut done = 0usize;
    loop {
        let rows = pending(&app.db, &db::now(), BATCH).await?;
        if rows.is_empty() {
            return Ok(done);
        }
        let batch = rows.len();
        for row in rows {
            match serde_json::from_str::<HookBody>(&row.body_json) {
                Ok(body) => match crate::hookrecv::process(app, &body).await {
                    Ok(()) => {
                        mark_done(&app.db, &row.id).await?;
                        done += 1;
                    }
                    Err(e) => {
                        let attempts = row.attempts + 1;
                        tracing::warn!(id = %row.id, attempts, error = ?e, "hook event failed; will retry");
                        mark_failed(&app.db, &row.id, attempts, &format!("{e:#}")).await?;
                    }
                },
                Err(e) => {
                    tracing::error!(id = %row.id, error = %e, "hook event body unparseable; dropped");
                    mark_dead(&app.db, &row.id, &format!("unparseable body: {e}")).await?;
                }
            }
        }
        // 這一批沒滿就沒有下一批了；滿了就繼續，避免一次喚醒只吃 BATCH 列。
        if batch < BATCH as usize {
            return Ok(done);
        }
    }
}

async fn prune(pool: &SqlitePool) -> Result<()> {
    let cutoff = chrono::Utc::now() - chrono::Duration::from_std(KEEP_PROCESSED).unwrap_or_default();
    sqlx::query("DELETE FROM hook_events WHERE processed_at IS NOT NULL AND processed_at < ?")
        .bind(cutoff.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn pool() -> SqlitePool {
        let p = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        migrate(&p).await.unwrap();
        p
    }

    fn body(prompt_id: &str, at: &str) -> HookBody {
        HookBody {
            bot_id: "b1".into(),
            provider: "claude".into(),
            payload: json!({"hook_event_name": "Stop", "session_id": "s1", "prompt_id": prompt_id}),
            received_at: Some(at.into()),
            truncated: false,
        }
    }

    #[tokio::test]
    async fn migrate_is_reentrant() {
        let p = pool().await;
        migrate(&p).await.unwrap();
        migrate(&p).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM hook_events").fetch_one(&p).await.unwrap();
        assert_eq!(n, 0);
    }

    /// 同一則重送不會變成兩列——這是 issue #70 的「duplicate hook 不造成 duplicate completion」
    /// 在收件匣這一層的保證。
    #[tokio::test]
    async fn the_same_event_sent_twice_is_stored_once() {
        let p = pool().await;
        let b = body("p1", "2026-09-17T12:00:00.000Z");
        assert_eq!(accept(&p, &b, Source::Http).await.unwrap(), Accepted::Stored);
        assert_eq!(accept(&p, &b, Source::Http).await.unwrap(), Accepted::Duplicate, "重送");
        // 來源不同也一樣：HTTP 收過的那則，spool 重放回來還是同一則。
        assert_eq!(accept(&p, &b, Source::Spool).await.unwrap(), Accepted::Duplicate);
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM hook_events").fetch_one(&p).await.unwrap();
        assert_eq!(n, 1);
    }

    /// 兩則**不同**的事件不可以被去重併成一則。
    #[tokio::test]
    async fn two_different_events_are_both_stored() {
        let p = pool().await;
        // 同一毫秒、不同回合。
        accept(&p, &body("p1", "2026-09-17T12:00:00.000Z"), Source::Http).await.unwrap();
        accept(&p, &body("p2", "2026-09-17T12:00:00.000Z"), Source::Http).await.unwrap();
        // 同一回合 id、不同時間（重繪類事件）。
        accept(&p, &body("p1", "2026-09-17T12:00:01.000Z"), Source::Http).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM hook_events").fetch_one(&p).await.unwrap();
        assert_eq!(n, 3);
    }

    /// 認不出時間的 body 不參加去重：寧可多一列，也不要把兩則不同的事件併掉。
    #[tokio::test]
    async fn bodies_without_a_timestamp_are_never_deduped() {
        let p = pool().await;
        let mut b = body("p1", "x");
        b.received_at = None;
        assert!(dedupe_key(&b).is_none());
        accept(&p, &b, Source::Http).await.unwrap();
        accept(&p, &b, Source::Http).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM hook_events").fetch_one(&p).await.unwrap();
        assert_eq!(n, 2);
    }

    /// 失敗的列留在收件匣裡等下一輪，不會被標成處理完——「暫時失敗不等於事件消失」。
    #[tokio::test]
    async fn a_failed_event_stays_pending_and_backs_off() {
        let p = pool().await;
        accept(&p, &body("p1", "2026-09-17T12:00:00.000Z"), Source::Http).await.unwrap();
        let row = pending(&p, &db::now(), 10).await.unwrap().pop().unwrap();
        mark_failed(&p, &row.id, 1, "boom").await.unwrap();

        // 退避還沒到：這一刻不會被挑出來。
        assert!(pending(&p, &db::now(), 10).await.unwrap().is_empty(), "退避中");
        // 但列還在，而且還是 pending（processed_at 仍是 NULL）。
        let (processed, attempts, err): (Option<String>, i64, Option<String>) =
            sqlx::query_as("SELECT processed_at, attempts, last_error FROM hook_events WHERE id = ?")
                .bind(&row.id)
                .fetch_one(&p)
                .await
                .unwrap();
        assert!(processed.is_none(), "沒有被當成處理完");
        assert_eq!(attempts, 1);
        assert_eq!(err.as_deref(), Some("boom"));
        // 退避到了就再挑得到。
        let later = "2099-01-01T00:00:00.000Z";
        assert_eq!(pending(&p, later, 10).await.unwrap().len(), 1, "退避過了要再試");
    }

    /// 收件匣照**寫入順序**出列（`rowid`），不看 ULID。
    #[tokio::test]
    async fn events_come_out_in_write_order() {
        let p = pool().await;
        for i in 0..3 {
            accept(&p, &body(&format!("p{i}"), "2026-09-17T12:00:00.000Z"), Source::Http).await.unwrap();
        }
        let ids: Vec<String> = pending(&p, &db::now(), 10)
            .await
            .unwrap()
            .iter()
            .map(|r| serde_json::from_str::<HookBody>(&r.body_json).unwrap().payload["prompt_id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids, vec!["p0", "p1", "p2"]);
    }

    #[tokio::test]
    async fn processed_rows_are_pruned_but_pending_ones_are_kept() {
        let p = pool().await;
        accept(&p, &body("old", "2026-09-17T12:00:00.000Z"), Source::Http).await.unwrap();
        accept(&p, &body("new", "2026-09-17T12:00:01.000Z"), Source::Http).await.unwrap();
        sqlx::query("UPDATE hook_events SET processed_at = '2000-01-01T00:00:00.000Z' WHERE dedupe_key LIKE '%|old'")
            .execute(&p)
            .await
            .unwrap();
        prune(&p).await.unwrap();
        let left: Vec<String> = sqlx::query_scalar("SELECT dedupe_key FROM hook_events").fetch_all(&p).await.unwrap();
        assert_eq!(left.len(), 1, "只剩還沒處理的那列");
        assert!(left[0].ends_with("|new"));
    }

    #[test]
    fn the_backoff_grows_and_is_capped() {
        assert_eq!(backoff(0), Duration::from_secs(1));
        assert_eq!(backoff(3), Duration::from_secs(8));
        assert_eq!(backoff(99), Duration::from_secs(256), "8 次之後就到頂");
    }
}
