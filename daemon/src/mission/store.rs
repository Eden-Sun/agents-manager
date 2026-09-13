//! 群組任務的持久資料（`docs/goals/agm-missions.md`）。
//!
//! `missions` 只存「使用者開任務時決定的事」與結案事實，**不存流程狀態**：規劃／執行／審查／
//! 驗證是 AGM 派出去的 assignments 的投影（P1b 接上 `mission_id`），另存一份狀態就要跟它對帳。
//! 目前能直接判定的只有終態與暫停（見 [`Mission::status`]）。
//!
//! `mission_events` 是任務在群組時間軸上的那一串：使用者下的指示、AGM／bot 的回報、暫停與交付。
//! 群組時間軸原本只合併成員 bot 的訊息，AGM 不是專案成員，它的回報沒有地方放——放這裡。

use anyhow::Result;
use serde::Serialize;
use sqlx::SqlitePool;

const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS missions (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL,
  -- 同一次「交給 AGM」重送回同一筆。
  client_request_id TEXT NOT NULL,
  text TEXT NOT NULL,
  -- D2：push_main | pr
  delivery_mode TEXT NOT NULL,
  -- D7：claude | codex | grok
  executor_kind TEXT NOT NULL,
  -- D5：wait | switch（5h 撞限時原地等，還是直接換下一個身分）
  on_5h_limit TEXT NOT NULL,
  -- review 退回＋驗證失敗合計的輪數上限；用完就停下來問人。
  max_rounds INTEGER NOT NULL DEFAULT 2,
  rounds_used INTEGER NOT NULL DEFAULT 0,
  -- 非 NULL = 停下來等人（max_rounds、no_fable_for_verifier、push_main_failed、waiting_quota…）。
  paused_reason TEXT,
  paused_detail TEXT,
  result_summary TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  completed_at TEXT,
  cancelled_at TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS missions_crid ON missions(project_id, client_request_id);
CREATE INDEX IF NOT EXISTS missions_project ON missions(project_id, created_at);
CREATE TABLE IF NOT EXISTS mission_events (
  id TEXT PRIMARY KEY,
  mission_id TEXT NOT NULL,
  -- instruction | report | verified | round | paused | resumed | cancelled | delivered | completed | note
  kind TEXT NOT NULL,
  text TEXT NOT NULL,
  -- NULL = 使用者本人；bot id = 那顆 bot（多半是 AGM）；'daemon' = daemon 自己記的。
  relay_from TEXT,
  payload_json TEXT NOT NULL DEFAULT '{}',
  created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS mission_events_mission ON mission_events(mission_id, created_at);
-- 身分停用原本只存在瀏覽器 localStorage（web/src/store/quotaHide.ts），daemon 挑身分時看不到。
CREATE TABLE IF NOT EXISTS identity_prefs (
  host TEXT NOT NULL,
  kind TEXT NOT NULL,
  identity TEXT NOT NULL,
  disabled INTEGER NOT NULL DEFAULT 0,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (host, kind, identity)
);
"#;

pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    for stmt in DDL.split(";\n") {
        let s = stmt.trim();
        if !s.is_empty() {
            sqlx::query(s).execute(pool).await?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Mission {
    pub id: String,
    pub project_id: String,
    pub client_request_id: String,
    pub text: String,
    pub delivery_mode: String,
    pub executor_kind: String,
    pub on_5h_limit: String,
    pub max_rounds: i64,
    pub rounds_used: i64,
    pub paused_reason: Option<String>,
    pub paused_detail: Option<String>,
    pub result_summary: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
    pub cancelled_at: Option<String>,
}

impl Mission {
    /// 能從這一列直接判定的狀態。流程中的細分（規劃／執行／審查／驗證）要看它的 assignments。
    pub fn status(&self) -> &'static str {
        if self.cancelled_at.is_some() {
            "cancelled"
        } else if self.completed_at.is_some() {
            "done"
        } else if self.paused_reason.is_some() {
            "paused"
        } else {
            "open"
        }
    }

    pub fn json(&self) -> serde_json::Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        if let Some(o) = v.as_object_mut() {
            o.insert("status".into(), self.status().into());
        }
        v
    }
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct MissionEvent {
    pub id: String,
    pub mission_id: String,
    pub kind: String,
    pub text: String,
    pub relay_from: Option<String>,
    pub payload_json: String,
    pub created_at: String,
}

pub struct NewMission<'a> {
    pub project_id: &'a str,
    pub client_request_id: &'a str,
    pub text: &'a str,
    pub delivery_mode: &'a str,
    pub executor_kind: &'a str,
    pub on_5h_limit: &'a str,
    pub max_rounds: i64,
}

/// 建立任務；同一個 `(project_id, client_request_id)` 已經有了就回那一筆（`created=false`）。
pub async fn create(pool: &SqlitePool, m: &NewMission<'_>) -> Result<(Mission, bool)> {
    if let Some(existing) = by_crid(pool, m.project_id, m.client_request_id).await? {
        return Ok((existing, false));
    }
    let id = crate::db::ulid();
    let now = crate::db::now();
    let res = sqlx::query(
        "INSERT OR IGNORE INTO missions
           (id, project_id, client_request_id, text, delivery_mode, executor_kind, on_5h_limit, max_rounds, created_at, updated_at)
         VALUES (?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&id)
    .bind(m.project_id)
    .bind(m.client_request_id)
    .bind(m.text)
    .bind(m.delivery_mode)
    .bind(m.executor_kind)
    .bind(m.on_5h_limit)
    .bind(m.max_rounds)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    let row = by_crid(pool, m.project_id, m.client_request_id).await?.ok_or_else(|| anyhow::anyhow!("mission vanished after insert"))?;
    Ok((row, res.rows_affected() == 1))
}

async fn by_crid(pool: &SqlitePool, project_id: &str, crid: &str) -> Result<Option<Mission>> {
    Ok(sqlx::query_as::<_, Mission>("SELECT * FROM missions WHERE project_id = ? AND client_request_id = ?")
        .bind(project_id)
        .bind(crid)
        .fetch_optional(pool)
        .await?)
}

pub async fn get(pool: &SqlitePool, id: &str) -> Result<Option<Mission>> {
    Ok(sqlx::query_as::<_, Mission>("SELECT * FROM missions WHERE id = ?").bind(id).fetch_optional(pool).await?)
}

/// `status`：`open`（含 paused）| `done` | `cancelled` | `all`。新的在前。
pub async fn list(pool: &SqlitePool, project_id: &str, status: &str, limit: i64) -> Result<Vec<Mission>> {
    let filter = match status {
        "done" => "AND completed_at IS NOT NULL AND cancelled_at IS NULL",
        "cancelled" => "AND cancelled_at IS NOT NULL",
        "open" => "AND completed_at IS NULL AND cancelled_at IS NULL",
        _ => "",
    };
    Ok(sqlx::query_as::<_, Mission>(&format!(
        "SELECT * FROM missions WHERE project_id = ? {filter} ORDER BY created_at DESC LIMIT ?"
    ))
    .bind(project_id)
    .bind(limit.clamp(1, 500))
    .fetch_all(pool)
    .await?)
}

pub async fn events(pool: &SqlitePool, mission_id: &str) -> Result<Vec<MissionEvent>> {
    Ok(sqlx::query_as::<_, MissionEvent>("SELECT * FROM mission_events WHERE mission_id = ? ORDER BY created_at, id")
        .bind(mission_id)
        .fetch_all(pool)
        .await?)
}

pub async fn add_event(
    pool: &SqlitePool,
    mission_id: &str,
    kind: &str,
    text: &str,
    relay_from: Option<&str>,
    payload: &serde_json::Value,
) -> Result<MissionEvent> {
    let ev = MissionEvent {
        id: crate::db::ulid(),
        mission_id: mission_id.into(),
        kind: kind.into(),
        text: text.into(),
        relay_from: relay_from.map(String::from),
        payload_json: payload.to_string(),
        created_at: crate::db::now(),
    };
    sqlx::query("INSERT INTO mission_events (id, mission_id, kind, text, relay_from, payload_json, created_at) VALUES (?,?,?,?,?,?,?)")
        .bind(&ev.id)
        .bind(&ev.mission_id)
        .bind(&ev.kind)
        .bind(&ev.text)
        .bind(&ev.relay_from)
        .bind(&ev.payload_json)
        .bind(&ev.created_at)
        .execute(pool)
        .await?;
    sqlx::query("UPDATE missions SET updated_at = ? WHERE id = ?").bind(&ev.created_at).bind(mission_id).execute(pool).await?;
    Ok(ev)
}

/// 已結案（完成或取消）的任務不接受任何狀態變更。回傳 `false` = 沒有改到（已結案或不存在）。
pub async fn pause(pool: &SqlitePool, id: &str, reason: &str, detail: Option<&str>) -> Result<bool> {
    let now = crate::db::now();
    let n = sqlx::query(
        "UPDATE missions SET paused_reason = ?, paused_detail = ?, updated_at = ?
         WHERE id = ? AND completed_at IS NULL AND cancelled_at IS NULL",
    )
    .bind(reason)
    .bind(detail)
    .bind(&now)
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n == 1)
}

pub async fn resume(pool: &SqlitePool, id: &str) -> Result<bool> {
    let now = crate::db::now();
    let n = sqlx::query(
        "UPDATE missions SET paused_reason = NULL, paused_detail = NULL, updated_at = ?
         WHERE id = ? AND paused_reason IS NOT NULL AND completed_at IS NULL AND cancelled_at IS NULL",
    )
    .bind(&now)
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n == 1)
}

pub async fn cancel(pool: &SqlitePool, id: &str) -> Result<bool> {
    let now = crate::db::now();
    let n = sqlx::query("UPDATE missions SET cancelled_at = ?, updated_at = ? WHERE id = ? AND completed_at IS NULL AND cancelled_at IS NULL")
        .bind(&now)
        .bind(&now)
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(n == 1)
}

pub async fn complete(pool: &SqlitePool, id: &str, summary: &str) -> Result<bool> {
    let now = crate::db::now();
    let n = sqlx::query(
        "UPDATE missions SET completed_at = ?, result_summary = ?, paused_reason = NULL, paused_detail = NULL, updated_at = ?
         WHERE id = ? AND completed_at IS NULL AND cancelled_at IS NULL",
    )
    .bind(&now)
    .bind(summary)
    .bind(&now)
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n == 1)
}

/// 用掉一輪（review 退回或驗證失敗）。回傳用掉之後的輪數；超過上限時**不**加、回 `Err(used)`，
/// 呼叫端負責把任務停下來問人。
pub async fn use_round(pool: &SqlitePool, id: &str) -> Result<Result<i64, i64>> {
    let now = crate::db::now();
    let n = sqlx::query(
        "UPDATE missions SET rounds_used = rounds_used + 1, updated_at = ?
         WHERE id = ? AND rounds_used < max_rounds AND completed_at IS NULL AND cancelled_at IS NULL",
    )
    .bind(&now)
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected();
    let used: i64 = sqlx::query_scalar("SELECT rounds_used FROM missions WHERE id = ?").bind(id).fetch_one(pool).await?;
    Ok(if n == 1 { Ok(used) } else { Err(used) })
}

pub async fn has_event(pool: &SqlitePool, mission_id: &str, kind: &str) -> Result<bool> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mission_events WHERE mission_id = ? AND kind = ?")
        .bind(mission_id)
        .bind(kind)
        .fetch_one(pool)
        .await?;
    Ok(n > 0)
}

pub async fn disabled_identities(pool: &SqlitePool, host: &str, kind: &str) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar("SELECT identity FROM identity_prefs WHERE host = ? AND kind = ? AND disabled = 1")
        .bind(host)
        .bind(kind)
        .fetch_all(pool)
        .await?)
}

pub async fn set_identity_disabled(pool: &SqlitePool, host: &str, kind: &str, identity: &str, disabled: bool) -> Result<()> {
    sqlx::query(
        "INSERT INTO identity_prefs (host, kind, identity, disabled, updated_at) VALUES (?,?,?,?,?)
         ON CONFLICT(host, kind, identity) DO UPDATE SET disabled = excluded.disabled, updated_at = excluded.updated_at",
    )
    .bind(host)
    .bind(kind)
    .bind(identity)
    .bind(disabled as i64)
    .bind(crate::db::now())
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        migrate(&pool).await.unwrap();
        pool
    }

    fn new<'a>(crid: &'a str) -> NewMission<'a> {
        NewMission {
            project_id: "p1",
            client_request_id: crid,
            text: "做 X",
            delivery_mode: "pr",
            executor_kind: "claude",
            on_5h_limit: "wait",
            max_rounds: 2,
        }
    }

    #[tokio::test]
    async fn the_same_request_id_returns_the_same_mission() {
        let pool = pool().await;
        let (a, created) = create(&pool, &new("r1")).await.unwrap();
        assert!(created);
        let (b, again) = create(&pool, &new("r1")).await.unwrap();
        assert!(!again);
        assert_eq!(a.id, b.id);
        assert_eq!(a.status(), "open");
    }

    #[tokio::test]
    async fn rounds_stop_at_the_cap_and_closed_missions_do_not_change() {
        let pool = pool().await;
        let (m, _) = create(&pool, &new("r1")).await.unwrap();
        assert_eq!(use_round(&pool, &m.id).await.unwrap(), Ok(1));
        assert_eq!(use_round(&pool, &m.id).await.unwrap(), Ok(2));
        assert_eq!(use_round(&pool, &m.id).await.unwrap(), Err(2), "上限到了不能再加");

        assert!(pause(&pool, &m.id, "max_rounds", None).await.unwrap());
        assert_eq!(get(&pool, &m.id).await.unwrap().unwrap().status(), "paused");
        assert!(resume(&pool, &m.id).await.unwrap());
        assert!(complete(&pool, &m.id, "ok").await.unwrap());
        assert_eq!(get(&pool, &m.id).await.unwrap().unwrap().status(), "done");
        assert!(!pause(&pool, &m.id, "late", None).await.unwrap(), "已完成的任務不能再暫停");
        assert!(!cancel(&pool, &m.id).await.unwrap());
        assert_eq!(list(&pool, "p1", "done", 10).await.unwrap().len(), 1);
        assert_eq!(list(&pool, "p1", "open", 10).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn identity_disable_is_per_host_and_kind() {
        let pool = pool().await;
        set_identity_disabled(&pool, "local", "claude", "cc1", true).await.unwrap();
        set_identity_disabled(&pool, "local", "claude", "cc2", true).await.unwrap();
        set_identity_disabled(&pool, "local", "claude", "cc2", false).await.unwrap();
        assert_eq!(disabled_identities(&pool, "local", "claude").await.unwrap(), ["cc1"]);
        assert!(disabled_identities(&pool, "remote", "claude").await.unwrap().is_empty());
    }
}
