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
  -- 這一筆是哪一筆任務的續作（追加修改）。NULL = 使用者自己開的第一筆。
  -- 續作是**新的一筆 mission**，不是把舊的打開重跑：舊那筆的 completed_at、result_summary 與
  -- 事件串完全不動，使用者回頭看到的還是當初交付的那一版。
  parent_mission_id TEXT,
  -- 續作請求的正規化指紋（parent + 文字 + 選項）。同一個 project 底下兩個不同 parent 用了同一個
  -- crid 時，只比文字會把別人的續作當成自己的重放回傳。
  request_fingerprint TEXT,
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
  -- instruction | report | verified | round | paused | resumed | cancelled | delivered | completed
  -- | note | question（使用者對成果追問）| answer（回覆：使用者回答暫停，或 AGM 回覆追問）
  kind TEXT NOT NULL,
  text TEXT NOT NULL,
  -- NULL = 使用者本人；bot id = 那顆 bot（多半是 AGM）；'daemon' = daemon 自己記的。
  relay_from TEXT,
  payload_json TEXT NOT NULL DEFAULT '{}',
  -- answer 指回它回答的那則 question 事件 id，成對顯示才不會變成一串對不上的獨白。
  reply_to TEXT,
  -- 同一個請求重送回同一則事件（回答／追問都要冪等，見 api.rs 的 replay 規則）。
  client_request_id TEXT,
  -- 這個 crid 當初代表的**完整請求**（kind/text/來源/reply_to 的正規化字串）。重放只比文字是不夠的：
  -- 一則 question 與一則 answer 可以有同樣的文字，AGM 的回覆換了 reply_to 也還是同一段字。
  request_fingerprint TEXT,
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
    // 既有資料庫的加欄位（跟 db::migrate 同一套做法）。全部是 additive，舊列拿到 NULL：
    // 沒有 parent 的就是使用者自己開的第一筆，沒有 reply_to/crid 的是這個功能之前的事件。
    for (table, col, ddl) in [
        ("missions", "parent_mission_id", "ALTER TABLE missions ADD COLUMN parent_mission_id TEXT"),
        ("mission_events", "reply_to", "ALTER TABLE mission_events ADD COLUMN reply_to TEXT"),
        ("mission_events", "client_request_id", "ALTER TABLE mission_events ADD COLUMN client_request_id TEXT"),
        ("mission_events", "request_fingerprint", "ALTER TABLE mission_events ADD COLUMN request_fingerprint TEXT"),
        ("missions", "request_fingerprint", "ALTER TABLE missions ADD COLUMN request_fingerprint TEXT"),
    ] {
        if !has_column(pool, table, col).await? {
            sqlx::query(ddl).execute(pool).await?;
        }
    }
    // 索引**只在這裡**建，而且一定在 ALTER 之後：新欄位的索引若留在上面的 DDL 段，舊資料庫跑到
    // 那一行時欄位還不存在，整個 migrate 會以 `no such column: parent_mission_id` 中止
    // （2026-09-13 父 review 實測）。
    for stmt in [
        "CREATE INDEX IF NOT EXISTS missions_parent ON missions(parent_mission_id)",
        "CREATE UNIQUE INDEX IF NOT EXISTS missions_one_open_child
           ON missions(parent_mission_id)
           WHERE parent_mission_id IS NOT NULL AND completed_at IS NULL AND cancelled_at IS NULL",
        "CREATE INDEX IF NOT EXISTS mission_events_reply ON mission_events(reply_to)",
        "CREATE UNIQUE INDEX IF NOT EXISTS mission_events_crid
           ON mission_events(mission_id, client_request_id) WHERE client_request_id IS NOT NULL",
    ] {
        sqlx::query(stmt).execute(pool).await?;
    }
    Ok(())
}

async fn has_column(pool: &SqlitePool, table: &str, col: &str) -> Result<bool> {
    let cols: Vec<(i64, String, String, i64, Option<String>, i64)> =
        sqlx::query_as(&format!("PRAGMA table_info({table})")).fetch_all(pool).await?;
    Ok(cols.iter().any(|c| c.1 == col))
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
    pub parent_mission_id: Option<String>,
    pub request_fingerprint: Option<String>,
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
    pub reply_to: Option<String>,
    pub client_request_id: Option<String>,
    pub request_fingerprint: Option<String>,
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
    /// 續作才有：它是哪一筆任務的下一輪。
    pub parent_mission_id: Option<&'a str>,
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
           (id, project_id, client_request_id, text, delivery_mode, executor_kind, on_5h_limit, max_rounds,
            parent_mission_id, created_at, updated_at)
         VALUES (?,?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&id)
    .bind(m.project_id)
    .bind(m.client_request_id)
    .bind(m.text)
    .bind(m.delivery_mode)
    .bind(m.executor_kind)
    .bind(m.on_5h_limit)
    .bind(m.max_rounds)
    .bind(m.parent_mission_id)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    let row = by_crid(pool, m.project_id, m.client_request_id).await?.ok_or_else(|| anyhow::anyhow!("mission vanished after insert"))?;
    Ok((row, res.rows_affected() == 1))
}

/// 建續作的四種結果。
#[derive(Debug)]
pub enum ChildCreate {
    Created(Mission),
    /// 同一個 crid、同一個請求：回原本那一筆。
    Replayed(Mission),
    /// 同一個 crid 但請求不一樣（換了 parent、文字或選項）。
    Mismatch(Mission),
    /// 這個 parent 已經有一筆還沒結案的續作。附上它，呼叫端才講得出「去看那一筆」。
    OpenChildExists(Mission),
}

/// 續作請求的正規化指紋：parent ＋ 文字 ＋ 四個選項。
///
/// 不包含快照（原成果的摘要、commit、PR），因為那些會隨原任務變動；用會變的東西當冪等鍵，
/// 同一個請求重送兩次就會被判成兩個不同的請求。
pub fn revise_fingerprint(parent_id: &str, text: &str, delivery: &str, executor: &str, on_5h: &str, max_rounds: i64) -> String {
    format!("{parent_id}\u{1}{text}\u{1}{delivery}\u{1}{executor}\u{1}{on_5h}\u{1}{max_rounds}")
}

/// 目前還沒結案的那筆續作（如果有）。
pub async fn open_child(pool: &SqlitePool, parent_id: &str) -> Result<Option<Mission>> {
    Ok(sqlx::query_as::<_, Mission>(
        "SELECT * FROM missions WHERE parent_mission_id = ? AND completed_at IS NULL AND cancelled_at IS NULL LIMIT 1",
    )
    .bind(parent_id)
    .fetch_optional(pool)
    .await?)
}

/// 建立續作：新任務、它的 instruction 事件、原成果那邊的 note、以及叫醒 AGM 的 inbox，**一次交易**。
///
/// 分成四次寫入的話，中途掛掉會留下一筆沒人知道的 open child（AGM 沒被通知，而重放又會直接回那一筆，
/// 永遠不會補送通知）。要嘛整組成立，要嘛什麼都沒有。
///
/// 併發保護由 `missions_one_open_child`（partial unique index）擔保：兩個並發的 revise 就算 crid
/// 不同也只有一個 INSERT 進得去，輸的那個整個交易回滾，再回頭讀出贏的那一筆。
#[allow(clippy::too_many_arguments)]
pub async fn create_child(
    pool: &SqlitePool,
    m: &NewMission<'_>,
    fingerprint: &str,
    instruction_payload: &serde_json::Value,
    parent_note: &str,
    inbox_payload_of: impl Fn(&str) -> serde_json::Value,
) -> Result<ChildCreate> {
    let parent_id = m.parent_mission_id.ok_or_else(|| anyhow::anyhow!("create_child needs a parent"))?;
    let id = crate::db::ulid();
    let now = crate::db::now();
    let mut tx = pool.begin().await?;
    let inserted = sqlx::query(
        "INSERT INTO missions
           (id, project_id, client_request_id, text, delivery_mode, executor_kind, on_5h_limit, max_rounds,
            parent_mission_id, request_fingerprint, created_at, updated_at)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&id)
    .bind(m.project_id)
    .bind(m.client_request_id)
    .bind(m.text)
    .bind(m.delivery_mode)
    .bind(m.executor_kind)
    .bind(m.on_5h_limit)
    .bind(m.max_rounds)
    .bind(parent_id)
    .bind(fingerprint)
    .bind(&now)
    .bind(&now)
    .execute(&mut *tx)
    .await;
    if let Err(e) = inserted {
        // 撞到索引。是哪一個決定了該回什麼：同 crid（重送或冪等鍵被重用）還是 one_open_child。
        drop(tx);
        if let Some(existing) = by_crid(pool, m.project_id, m.client_request_id).await? {
            return Ok(if existing.request_fingerprint.as_deref() == Some(fingerprint) {
                ChildCreate::Replayed(existing)
            } else {
                ChildCreate::Mismatch(existing)
            });
        }
        if let Some(open) = open_child(pool, parent_id).await? {
            return Ok(ChildCreate::OpenChildExists(open));
        }
        return Err(e.into());
    }
    insert_event(&mut tx, &id, "instruction", m.text, None, instruction_payload, None, None).await?;
    insert_event(
        &mut tx,
        parent_id,
        "note",
        parent_note,
        Some(crate::agent_relay::DAEMON_SENDER),
        &serde_json::json!({"revision_mission_id": id}),
        None,
        None,
    )
    .await?;
    push_inbox_tx(&mut tx, &format!("mission:{id}:created"), "mission_created", &inbox_payload_of(&id), &now).await?;
    tx.commit().await?;
    Ok(ChildCreate::Created(get(pool, &id).await?.ok_or_else(|| anyhow::anyhow!("child vanished after insert"))?))
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
    let mut tx = pool.begin().await?;
    let ev = insert_event(&mut tx, mission_id, kind, text, relay_from, payload, None, None).await?;
    tx.commit().await?;
    Ok(ev)
}

/// 寫一則事件（在呼叫端的 transaction 裡）。`reply_to` 讓 answer 指回它回答的 question，
/// `crid` 是冪等鍵。
#[allow(clippy::too_many_arguments)]
async fn insert_event(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    mission_id: &str,
    kind: &str,
    text: &str,
    relay_from: Option<&str>,
    payload: &serde_json::Value,
    reply_to: Option<&str>,
    crid: Option<&str>,
) -> Result<MissionEvent> {
    let ev = MissionEvent {
        id: crate::db::ulid(),
        mission_id: mission_id.into(),
        kind: kind.into(),
        text: text.into(),
        relay_from: relay_from.map(String::from),
        payload_json: payload.to_string(),
        reply_to: reply_to.map(String::from),
        client_request_id: crid.map(String::from),
        request_fingerprint: None,
        created_at: crate::db::now(),
    };
    sqlx::query(
        "INSERT INTO mission_events
           (id, mission_id, kind, text, relay_from, payload_json, reply_to, client_request_id, created_at)
         VALUES (?,?,?,?,?,?,?,?,?)",
    )
    .bind(&ev.id)
    .bind(&ev.mission_id)
    .bind(&ev.kind)
    .bind(&ev.text)
    .bind(&ev.relay_from)
    .bind(&ev.payload_json)
    .bind(&ev.reply_to)
    .bind(&ev.client_request_id)
    .bind(&ev.created_at)
    .execute(&mut **tx)
    .await?;
    sqlx::query("UPDATE missions SET updated_at = ? WHERE id = ?")
        .bind(&ev.created_at)
        .bind(mission_id)
        .execute(&mut **tx)
        .await?;
    Ok(ev)
}

/// 這個任務的續作（新的在前）。
pub async fn children(pool: &SqlitePool, mission_id: &str) -> Result<Vec<Mission>> {
    Ok(sqlx::query_as::<_, Mission>("SELECT * FROM missions WHERE parent_mission_id = ? ORDER BY created_at DESC")
        .bind(mission_id)
        .fetch_all(pool)
        .await?)
}

/// 既有的那一則同 crid 事件（重放判斷用）。
pub async fn event_by_crid(pool: &SqlitePool, mission_id: &str, crid: &str) -> Result<Option<MissionEvent>> {
    Ok(sqlx::query_as::<_, MissionEvent>(
        "SELECT * FROM mission_events WHERE mission_id = ? AND client_request_id = ?",
    )
    .bind(mission_id)
    .bind(crid)
    .fetch_optional(pool)
    .await?)
}

/// 一次寫入的結果。
#[derive(Debug)]
pub struct Written {
    pub event: MissionEvent,
    pub resumed: bool,
}

/// 寫一則回覆／追問會有的四種下場。
#[derive(Debug)]
pub enum ReplyOutcome {
    Written(Written),
    /// 同一個 crid、同一個請求：回原本那一則，什麼都沒再寫。
    Replayed(MissionEvent),
    /// 同一個 crid 但請求不一樣（換了文字、換了 kind、換了來源或 reply_to）。
    Mismatch(MissionEvent),
    /// 任務現在的狀態不允許這個動作。**什麼都沒寫**——事件與 inbox 都不會留下。
    Refused(&'static str),
}

/// 把一個請求正規化成可比對的指紋。
///
/// 重放判斷不能只比文字：一則 `question` 和一則 `answer` 可以是同一段字，AGM 的回覆換了
/// `reply_to` 之後也還是同一段字。這些都是不同的請求，卻共用一個冪等鍵——不比對就會把後者
/// 當成前者的重送靜靜吞掉。
pub fn reply_fingerprint(kind: &str, text: &str, relay_from: Option<&str>, reply_to: Option<&str>) -> String {
    format!("{kind}\u{1}{text}\u{1}{}\u{1}{}", relay_from.unwrap_or("-"), reply_to.unwrap_or("-"))
}

/// 任務現在允許什麼。`write_reply` 在**交易裡**重新讀一次狀態再比對，所以不會有「檢查完才被別人
/// 關掉」的空窗。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requires {
    /// 使用者回答「停下來問人」：任務一定要真的停著。已取消／已完成／還在跑都不該接受。
    Paused,
    /// 追問與 bot 的回覆：任何狀態都可以（對已完成的成果問一句話不會改變任何東西）。
    Anything,
}

/// 使用者的回答／追問，連同喚醒 AGM 的 inbox 事件，一次交易寫完。
///
/// 三件事非得綁在一起不可：事件（AGM 接回去要讀得到這句話）、`paused → open`（不放行它就永遠停著）、
/// inbox（沒有它 AGM 根本不知道有人回答了）。拆開由前端串的話，中間任何一步失敗都會留下半套。
///
/// **重放判斷也在交易裡**：先插事件，撞到 `mission_events_crid` 才回頭讀那一筆比對指紋。在交易外
/// 先查一次的話，兩個同 crid 的並發請求會雙雙通過檢查，然後其中一個撞索引變成 500。
#[allow(clippy::too_many_arguments)]
pub async fn write_reply(
    pool: &SqlitePool,
    mission_id: &str,
    kind: &str,
    text: &str,
    relay_from: Option<&str>,
    reply_to: Option<&str>,
    crid: &str,
    requires: Requires,
    resume_mission: bool,
    inbox: Option<(String, &str, &serde_json::Value)>,
) -> Result<ReplyOutcome> {
    let fingerprint = reply_fingerprint(kind, text, relay_from, reply_to);
    let mut tx = pool.begin().await?;

    // 重放**優先於**狀態：重送多半發生在任務已經被放行之後（網路慢、使用者連點），那時候它已經
    // 不是 paused 了。先看狀態就會把一個正確的重送擋成 not_paused。查詢在交易內做，所以它跟下面的
    // INSERT 之間沒有別人能插進來；真的同時撞進來的那種，最後會被 INSERT 的唯一索引攔下。
    if let Some(existing) = sqlx::query_as::<_, MissionEvent>(
        "SELECT * FROM mission_events WHERE mission_id = ? AND client_request_id = ?",
    )
    .bind(mission_id)
    .bind(crid)
    .fetch_optional(&mut *tx)
    .await?
    {
        return Ok(if existing.request_fingerprint.as_deref() == Some(fingerprint.as_str()) {
            ReplyOutcome::Replayed(existing)
        } else {
            ReplyOutcome::Mismatch(existing)
        });
    }

    // 狀態守衛在交易內重讀，所以「檢查時還停著、寫入時已被取消」不會發生。
    let m: Option<Mission> = sqlx::query_as::<_, Mission>("SELECT * FROM missions WHERE id = ?")
        .bind(mission_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(m) = m else { return Ok(ReplyOutcome::Refused("not_found")) };
    if requires == Requires::Paused {
        if m.cancelled_at.is_some() {
            return Ok(ReplyOutcome::Refused("cancelled"));
        }
        if m.completed_at.is_some() {
            return Ok(ReplyOutcome::Refused("already_closed"));
        }
        if m.paused_reason.is_none() {
            return Ok(ReplyOutcome::Refused("not_paused"));
        }
    }

    let ev = MissionEvent {
        id: crate::db::ulid(),
        mission_id: mission_id.into(),
        kind: kind.into(),
        text: text.into(),
        relay_from: relay_from.map(String::from),
        payload_json: "{}".into(),
        reply_to: reply_to.map(String::from),
        client_request_id: Some(crid.to_string()),
        request_fingerprint: Some(fingerprint.clone()),
        created_at: crate::db::now(),
    };
    let inserted = sqlx::query(
        "INSERT INTO mission_events
           (id, mission_id, kind, text, relay_from, payload_json, reply_to, client_request_id, request_fingerprint, created_at)
         VALUES (?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&ev.id)
    .bind(&ev.mission_id)
    .bind(&ev.kind)
    .bind(&ev.text)
    .bind(&ev.relay_from)
    .bind(&ev.payload_json)
    .bind(&ev.reply_to)
    .bind(&ev.client_request_id)
    .bind(&ev.request_fingerprint)
    .bind(&ev.created_at)
    .execute(&mut *tx)
    .await;
    if let Err(e) = inserted {
        // 撞到冪等鍵：這個 crid 已經有人寫過了。回頭比指紋——一樣就是重送，不一樣是冪等鍵被重用。
        drop(tx);
        let Some(existing) = event_by_crid(pool, mission_id, crid).await? else { return Err(e.into()) };
        return Ok(if existing.request_fingerprint.as_deref() == Some(fingerprint.as_str()) {
            ReplyOutcome::Replayed(existing)
        } else {
            ReplyOutcome::Mismatch(existing)
        });
    }

    let mut resumed = false;
    if resume_mission {
        resumed = sqlx::query(
            "UPDATE missions SET paused_reason = NULL, paused_detail = NULL, updated_at = ?
             WHERE id = ? AND paused_reason IS NOT NULL AND completed_at IS NULL AND cancelled_at IS NULL",
        )
        .bind(&ev.created_at)
        .bind(mission_id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        if resumed {
            insert_event(&mut tx, mission_id, "resumed", "繼續", Some(crate::agent_relay::DAEMON_SENDER), &serde_json::json!({}), None, None)
                .await?;
        }
    }
    if let Some((key, ikind, payload)) = inbox {
        push_inbox_tx(&mut tx, &key, ikind, payload, &ev.created_at).await?;
    }
    sqlx::query("UPDATE missions SET updated_at = ? WHERE id = ?").bind(&ev.created_at).bind(mission_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(ReplyOutcome::Written(Written { event: ev, resumed }))
}

/// 在呼叫端的交易裡塞一則 AGM inbox 事件。
///
/// 直接寫 `supervisor_inbox`（同一個資料庫）是刻意的：喚醒必須跟它要通知的那件事**同生共死**，
/// 拆成兩次寫入就會出現「事情發生了但沒人被叫醒」或反過來。`INSERT OR IGNORE` 讓 event_key 去重。
async fn push_inbox_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    event_key: &str,
    kind: &str,
    payload: &serde_json::Value,
    now: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO supervisor_inbox
           (id, supervisor_id, event_key, assignment_id, bot_id, turn_id, kind, payload_json, state, created_at, updated_at)
         VALUES (?,?,?,NULL,NULL,NULL,?,?, 'pending', ?, ?)",
    )
    .bind(crate::db::ulid())
    .bind(crate::supervisor::store::SUPERVISOR_ID)
    .bind(event_key)
    .bind(kind)
    .bind(payload.to_string())
    .bind(now)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// 「不回答直接繼續」：放行、記事件、叫醒 AGM，一次交易。
///
/// 回傳 `false` = 這次沒有發生 `paused → open`（本來就沒停著），那就什麼都不寫，也不通知——
/// AGM 自己呼叫 resume 因此不會把自己叫醒。
pub async fn resume_and_wake(pool: &SqlitePool, mission_id: &str, payload_of: impl Fn(&str) -> serde_json::Value) -> Result<bool> {
    let now = crate::db::now();
    let mut tx = pool.begin().await?;
    let resumed = sqlx::query(
        "UPDATE missions SET paused_reason = NULL, paused_detail = NULL, updated_at = ?
         WHERE id = ? AND paused_reason IS NOT NULL AND completed_at IS NULL AND cancelled_at IS NULL",
    )
    .bind(&now)
    .bind(mission_id)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if !resumed {
        return Ok(false);
    }
    let ev = insert_event(&mut tx, mission_id, "resumed", "繼續", Some(crate::agent_relay::DAEMON_SENDER), &serde_json::json!({}), None, None).await?;
    let payload = payload_of(&ev.id);
    push_inbox_tx(&mut tx, &format!("mission:{mission_id}:resumed:{}", ev.id), "mission_resumed", &payload, &now).await?;
    tx.commit().await?;
    Ok(true)
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
    use serde_json::json;

    async fn pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        migrate(&pool).await.unwrap();
        // 喚醒寫的是 supervisor_inbox，跟任務同一個資料庫；不建它就測不到「事件與喚醒同生共死」。
        crate::supervisor::store::migrate(&pool).await.unwrap();
        pool
    }

    /// 真正能並發的池：多條連線共用同一個 in-memory 資料庫。
    ///
    /// `sqlite::memory:` 每條連線都是**各自獨立**的空資料庫，用它跑併發測試只會兩邊都成功而且
    /// 什麼都沒驗到。要 shared-cache 的 URI 才是同一個 DB。
    async fn shared_pool(name: &str) -> SqlitePool {
        let url = format!("sqlite:file:{name}?mode=memory&cache=shared");
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(4)
            // 併發寫入時 SQLite 會回 SQLITE_BUSY；讓它等一下而不是直接失敗。
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(&url)
            .await
            .unwrap();
        sqlx::query("PRAGMA busy_timeout = 5000").execute(&pool).await.unwrap();
        migrate(&pool).await.unwrap();
        crate::supervisor::store::migrate(&pool).await.unwrap();
        pool
    }

    fn revision<'a>(parent: &'a str, crid: &'a str, text: &'a str) -> NewMission<'a> {
        NewMission {
            project_id: "p1",
            client_request_id: crid,
            text,
            delivery_mode: "pr",
            executor_kind: "claude",
            on_5h_limit: "wait",
            max_rounds: 2,
            parent_mission_id: Some(parent),
        }
    }

    async fn done_parent(pool: &SqlitePool, crid: &str) -> Mission {
        let (m, _) = create(pool, &new(crid)).await.unwrap();
        complete(pool, &m.id, "第一版").await.unwrap();
        get(pool, &m.id).await.unwrap().unwrap()
    }

    fn fp(parent: &str, text: &str) -> String {
        revise_fingerprint(parent, text, "pr", "claude", "wait", 2)
    }

    /// 兩個**真正同時**送出的 revise（不同 crid）只會建出一筆續作。
    ///
    /// 這是 partial unique index 在擋，不是應用層的「先查再寫」——所以兩條連線同時進來也成立。
    #[tokio::test]
    async fn two_concurrent_revises_create_exactly_one_child() {
        let pool = shared_pool("revise_race").await;
        let parent = done_parent(&pool, "r1").await;
        // 借用要活過 join!，所以先把參數綁成變數。
        let (ra, rb) = (revision(&parent.id, "crid-A", "把 A 做完"), revision(&parent.id, "crid-B", "另一個方向"));
        let (fa, fb) = (fp(&parent.id, "把 A 做完"), fp(&parent.id, "另一個方向"));
        let empty = json!({});
        let (a, b) = tokio::join!(
            create_child(&pool, &ra, &fa, &empty, "note", |_| json!({})),
            create_child(&pool, &rb, &fb, &empty, "note", |_| json!({})),
        );
        let outcomes = [a.unwrap(), b.unwrap()];
        let created = outcomes.iter().filter(|o| matches!(o, ChildCreate::Created(_))).count();
        let blocked = outcomes.iter().filter(|o| matches!(o, ChildCreate::OpenChildExists(_))).count();
        assert_eq!((created, blocked), (1, 1), "一個贏、一個被擋，沒有第三種下場");

        let children: Vec<Mission> = sqlx::query_as("SELECT * FROM missions WHERE parent_mission_id = ?")
            .bind(&parent.id)
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(children.len(), 1, "資料庫裡真的只有一筆續作");
        // 輸的那個什麼都沒留下：沒有 instruction 事件、沒有 inbox。
        let events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mission_events WHERE kind = 'instruction' AND mission_id != ?")
            .bind(&parent.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(events, 1);
        let inbox: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE kind = 'mission_created'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(inbox, 1, "只有一次喚醒");
    }

    /// 續作的四樣東西（新任務、instruction、parent 的 note、inbox）同生共死。
    #[tokio::test]
    async fn a_revision_writes_all_four_things_or_none() {
        let pool = pool().await;
        let parent = done_parent(&pool, "r1").await;
        // inbox 表不見了 → 整筆交易必須回滾，不能留下一筆沒人知道的 open child。
        sqlx::query("DROP TABLE supervisor_inbox").execute(&pool).await.unwrap();
        let err = create_child(&pool, &revision(&parent.id, "c1", "續作"), &fp(&parent.id, "續作"), &json!({}), "note", |_| json!({})).await;
        assert!(err.is_err());
        let children: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM missions WHERE parent_mission_id = ?")
            .bind(&parent.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(children, 0, "沒有孤兒續作");
        let notes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mission_events WHERE mission_id = ? AND kind = 'note'")
            .bind(&parent.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(notes, 0, "原成果那邊也沒有留下半句話");
    }

    /// 同一個 crid 換了請求內容就不是重送。指紋比的是「請求」，不是會變的快照。
    #[tokio::test]
    async fn the_same_request_id_with_a_different_request_is_not_a_replay() {
        let pool = pool().await;
        let p1 = done_parent(&pool, "r1").await;
        let first = create_child(&pool, &revision(&p1.id, "same", "做 X"), &fp(&p1.id, "做 X"), &json!({}), "n", |_| json!({}))
            .await
            .unwrap();
        assert!(matches!(first, ChildCreate::Created(_)));
        // 同 crid 同請求 → 重送。
        let again = create_child(&pool, &revision(&p1.id, "same", "做 X"), &fp(&p1.id, "做 X"), &json!({}), "n", |_| json!({}))
            .await
            .unwrap();
        assert!(matches!(again, ChildCreate::Replayed(_)));
        // 同 crid 換文字 → 不是重送。
        let changed = create_child(&pool, &revision(&p1.id, "same", "做 Y"), &fp(&p1.id, "做 Y"), &json!({}), "n", |_| json!({}))
            .await
            .unwrap();
        assert!(matches!(changed, ChildCreate::Mismatch(_)), "換了文字就不是同一個請求");

        // 同一個 project 的**另一個** parent 用了同一個 crid：不能把別人的續作當成自己的重放回傳。
        let p2 = done_parent(&pool, "r2").await;
        let other = create_child(&pool, &revision(&p2.id, "same", "做 X"), &fp(&p2.id, "做 X"), &json!({}), "n", |_| json!({}))
            .await
            .unwrap();
        match other {
            ChildCreate::Mismatch(m) => assert_eq!(m.parent_mission_id.as_deref(), Some(p1.id.as_str()), "指紋含 parent，認得出不是自己的"),
            o => panic!("expected a mismatch, got {o:?}"),
        }
    }

    /// 回答的三件事同生共死，而且無效狀態下一個字都不會留。
    #[tokio::test]
    async fn an_answer_writes_everything_or_nothing() {
        let pool = pool().await;
        let (m, _) = create(&pool, &new("r1")).await.unwrap();
        // 沒有暫停 → 使用者的回答不成立，事件與 inbox 都不該出現。
        let out = write_reply(&pool, &m.id, "answer", "好", None, None, "a1", Requires::Paused, true,
            Some((format!("mission:{}:answer:a1", m.id), "mission_answered", &json!({})))).await.unwrap();
        assert!(matches!(out, ReplyOutcome::Refused("not_paused")));
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mission_events WHERE mission_id = ?").bind(&m.id).fetch_one(&pool).await.unwrap();
        assert_eq!(n, 0, "拒絕時不留事件");
        let inbox: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox").fetch_one(&pool).await.unwrap();
        assert_eq!(inbox, 0, "也不留喚醒");

        // 取消掉的任務同樣不接受（這條以前會寫進去）。
        cancel(&pool, &m.id).await.unwrap();
        let out = write_reply(&pool, &m.id, "answer", "好", None, None, "a2", Requires::Paused, true, None).await.unwrap();
        assert!(matches!(out, ReplyOutcome::Refused("cancelled")));

        // 真的停著才寫；而且三件事一起成立。
        let (m2, _) = create(&pool, &new("r2")).await.unwrap();
        pause(&pool, &m2.id, "clarify", None).await.unwrap();
        let payload = json!({"x": 1});
        let out = write_reply(&pool, &m2.id, "answer", "照你說的做", None, None, "b1", Requires::Paused, true,
            Some((format!("mission:{}:answer:b1", m2.id), "mission_answered", &payload))).await.unwrap();
        match out {
            ReplyOutcome::Written(w) => assert!(w.resumed),
            o => panic!("expected a write, got {o:?}"),
        }
        assert_eq!(get(&pool, &m2.id).await.unwrap().unwrap().status(), "open");
        let inbox: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE event_key = ?")
            .bind(format!("mission:{}:answer:b1", m2.id))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(inbox, 1);

        // 重送發生在放行之後：回原結果，不是 not_paused。
        let again = write_reply(&pool, &m2.id, "answer", "照你說的做", None, None, "b1", Requires::Paused, true, None).await.unwrap();
        assert!(matches!(again, ReplyOutcome::Replayed(_)), "重放優先於狀態");
        // 同 crid 換內容／換 kind／換來源都不是重送。
        for (kind, text, from, reply) in [("answer", "不一樣", None, None), ("question", "照你說的做", None, None),
                                          ("answer", "照你說的做", Some("daemon"), None)] {
            let out = write_reply(&pool, &m2.id, kind, text, from, reply, "b1", Requires::Anything, false, None).await.unwrap();
            assert!(matches!(out, ReplyOutcome::Mismatch(_)), "{kind}/{text} 應該要被認出不是同一個請求");
        }
    }

    /// 兩個同 crid 的回答真正同時進來：只會有一則事件、一次喚醒，而且沒有人拿到 500。
    #[tokio::test]
    async fn two_concurrent_answers_with_one_request_id_write_once() {
        let pool = shared_pool("answer_race").await;
        let (m, _) = create(&pool, &new("r1")).await.unwrap();
        pause(&pool, &m.id, "clarify", None).await.unwrap();
        let key = format!("mission:{}:answer:same", m.id);
        let payload = json!({});
        let (a, b) = tokio::join!(
            write_reply(&pool, &m.id, "answer", "好", None, None, "same", Requires::Paused, true, Some((key.clone(), "mission_answered", &payload))),
            write_reply(&pool, &m.id, "answer", "好", None, None, "same", Requires::Paused, true, Some((key.clone(), "mission_answered", &payload))),
        );
        let outs = [a.unwrap(), b.unwrap()];
        assert!(outs.iter().all(|o| matches!(o, ReplyOutcome::Written(_) | ReplyOutcome::Replayed(_))), "沒有人該拿到錯誤");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mission_events WHERE mission_id = ? AND kind = 'answer'")
            .bind(&m.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 1, "只有一則回答");
        let inbox: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE event_key = ?").bind(&key).fetch_one(&pool).await.unwrap();
        assert_eq!(inbox, 1, "只有一次喚醒");
    }

    /// 「不回答直接繼續」也是一個交易，而且只有真的 paused→open 才通知。
    #[tokio::test]
    async fn plain_resume_is_atomic_and_only_notifies_on_a_real_transition() {
        let pool = pool().await;
        let (m, _) = create(&pool, &new("r1")).await.unwrap();
        assert!(!resume_and_wake(&pool, &m.id, |_| json!({})).await.unwrap(), "本來就沒停著");
        let inbox: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox").fetch_one(&pool).await.unwrap();
        assert_eq!(inbox, 0, "沒有轉移就沒有通知（AGM 自己 resume 不會叫醒自己）");

        pause(&pool, &m.id, "clarify", None).await.unwrap();
        assert!(resume_and_wake(&pool, &m.id, |_| json!({"answered": false})).await.unwrap());
        let inbox: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE kind = 'mission_resumed'").fetch_one(&pool).await.unwrap();
        assert_eq!(inbox, 1);
        assert!(!resume_and_wake(&pool, &m.id, |_| json!({})).await.unwrap(), "再按一次沒有轉移");
        let inbox: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE kind = 'mission_resumed'").fetch_one(&pool).await.unwrap();
        assert_eq!(inbox, 1, "不會變成第二次");
    }

    /// 舊資料庫升級：1715872 當時的 DDL 建的表 + 一筆資料，migrate 要能跑完且不動到那筆資料。
    #[tokio::test]
    async fn an_old_database_upgrades_without_losing_anything() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        // 新欄位出現之前的形狀（只列這次會動到的兩張表的關鍵欄位）。
        for stmt in [
            "CREATE TABLE missions (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, client_request_id TEXT NOT NULL,
               text TEXT NOT NULL, delivery_mode TEXT NOT NULL, executor_kind TEXT NOT NULL, on_5h_limit TEXT NOT NULL,
               max_rounds INTEGER NOT NULL DEFAULT 2, rounds_used INTEGER NOT NULL DEFAULT 0, paused_reason TEXT,
               paused_detail TEXT, result_summary TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
               completed_at TEXT, cancelled_at TEXT)",
            "CREATE UNIQUE INDEX missions_crid ON missions(project_id, client_request_id)",
            "CREATE TABLE mission_events (id TEXT PRIMARY KEY, mission_id TEXT NOT NULL, kind TEXT NOT NULL,
               text TEXT NOT NULL, relay_from TEXT, payload_json TEXT NOT NULL DEFAULT '{}', created_at TEXT NOT NULL)",
        ] {
            sqlx::query(stmt).execute(&pool).await.unwrap();
        }
        sqlx::query("INSERT INTO missions (id,project_id,client_request_id,text,delivery_mode,executor_kind,on_5h_limit,max_rounds,created_at,updated_at,completed_at)
                     VALUES ('m1','p1','r1','舊任務','pr','claude','wait',2,'t','t','t')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO mission_events (id,mission_id,kind,text,created_at) VALUES ('e1','m1','completed','做完了','t')")
            .execute(&pool)
            .await
            .unwrap();

        migrate(&pool).await.unwrap();
        migrate(&pool).await.unwrap(); // 冪等
        crate::supervisor::store::migrate(&pool).await.unwrap();

        let m = get(&pool, "m1").await.unwrap().unwrap();
        assert_eq!((m.text.as_str(), m.status()), ("舊任務", "done"), "舊資料原封不動");
        assert!(m.parent_mission_id.is_none());
        let e = events(&pool, "m1").await.unwrap();
        assert_eq!(e.len(), 1);
        assert!(e[0].reply_to.is_none() && e[0].client_request_id.is_none());
        // 新索引是在 ALTER 之後才建的，所以這時候它們都在。
        let idx: Vec<String> = sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type='index' AND name LIKE 'missions_one_open_child'")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(idx.len(), 1, "一個 parent 一個 open child 的索引建起來了");
        // 而且升級後照樣能開續作。
        let child = create_child(&pool, &revision("m1", "c1", "續作"), &fp("m1", "續作"), &json!({}), "n", |_| json!({})).await.unwrap();
        assert!(matches!(child, ChildCreate::Created(_)));
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
            parent_mission_id: None,
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
