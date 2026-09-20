//! fork 的持久操作紀錄（issue #348）：`POST /api/bots/:id/fork` 帶 `client_request_id`，同一個 id 重送
//! 拿到同一個目標 bot 與同一個結果，不會再建一顆、也不會再對 provider 分岔一次 session。
//!
//! 狀態：`planned`（已記下目標 bot id 與來源 session，config 還沒寫）→ `created`（bot 已在 config／DB）→
//! `started`（有 run）｜`failed`（建好但啟動失敗，`start_error` 是原因）。每一步都是冪等的，daemon 在任何
//! 兩步之間死掉，用同一個 id 重送就從還沒做完的那一步接下去，不會另配一個目標。
use anyhow::Result;
use sqlx::SqlitePool;

#[derive(Debug, Clone)]
pub struct ForkOp {
    pub client_request_id: String,
    pub source_bot_id: String,
    /// 呼叫端要求的名字（trim 後，沒指定＝空字串）；跟 `source_bot_id` 一起是「同一個請求」的判準。
    pub requested_name: String,
    pub target_bot_id: String,
    /// 第一次請求當下來源的 native session：重送時沿用，來源之後又跑出新 session 也不改分岔點。
    pub session_id: String,
    pub name: String,
    pub state: String,
    pub run_id: Option<String>,
    pub start_error: Option<String>,
}

pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS fork_ops (
           client_request_id TEXT PRIMARY KEY,
           source_bot_id TEXT NOT NULL,
           requested_name TEXT NOT NULL DEFAULT '',
           target_bot_id TEXT NOT NULL,
           session_id TEXT NOT NULL,
           name TEXT NOT NULL,
           state TEXT NOT NULL,
           run_id TEXT,
           start_error TEXT,
           created_at TEXT NOT NULL,
           updated_at TEXT NOT NULL
         )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

type Row = (String, String, String, String, String, String, String, Option<String>, Option<String>);

/// 讀不到（DB 錯）是錯誤，不是「沒有這筆」：呼叫端不能因此當成新請求再建一顆。
pub async fn get(pool: &SqlitePool, client_request_id: &str) -> Result<Option<ForkOp>> {
    let row: Option<Row> = sqlx::query_as(
        "SELECT client_request_id, source_bot_id, requested_name, target_bot_id, session_id, name, state, run_id, start_error
           FROM fork_ops WHERE client_request_id = ?",
    )
    .bind(client_request_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(client_request_id, source_bot_id, requested_name, target_bot_id, session_id, name, state, run_id, start_error)| ForkOp {
        client_request_id,
        source_bot_id,
        requested_name,
        target_bot_id,
        session_id,
        name,
        state,
        run_id,
        start_error,
    }))
}

pub async fn insert(pool: &SqlitePool, op: &ForkOp) -> Result<()> {
    let now = crate::db::now();
    sqlx::query(
        "INSERT INTO fork_ops (client_request_id, source_bot_id, requested_name, target_bot_id, session_id, name, state, run_id, start_error, created_at, updated_at)
         VALUES (?,?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&op.client_request_id)
    .bind(&op.source_bot_id)
    .bind(&op.requested_name)
    .bind(&op.target_bot_id)
    .bind(&op.session_id)
    .bind(&op.name)
    .bind(&op.state)
    .bind(&op.run_id)
    .bind(&op.start_error)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_state(
    pool: &SqlitePool,
    client_request_id: &str,
    state: &str,
    name: &str,
    run_id: Option<&str>,
    start_error: Option<&str>,
) -> Result<()> {
    sqlx::query("UPDATE fork_ops SET state = ?, name = ?, run_id = ?, start_error = ?, updated_at = ? WHERE client_request_id = ?")
        .bind(state)
        .bind(name)
        .bind(run_id)
        .bind(start_error)
        .bind(crate::db::now())
        .bind(client_request_id)
        .execute(pool)
        .await?;
    Ok(())
}
