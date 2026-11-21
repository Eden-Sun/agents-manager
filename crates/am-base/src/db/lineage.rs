//! codex session 血統表 `lineage_sessions`（issue #1943 子票 A）。
//!
//! 一列＝某專案某條模組線（`line`，例 lifecycle／web／quota）上的一顆 CLI session。每條線最多一顆 `checkpoint`
//! （派工時 fork／resume 的起點，部分唯一索引保證）；做到一半的是 `active`，被取代或放棄的是 `retired`。
//! 這裡只管帳本：不開 session、不碰 pane。前移（[`advance`]）與退役（[`retire`]）都在一個寫交易裡完成。

use serde::Serialize;
use sqlx::SqlitePool;

pub const ROLES: [&str; 2] = ["worker", "thinker"];
/// 血統只管 codex（claude 快取 60 分，照舊，#1943）；表上有同一條 CHECK。
pub const KINDS: [&str; 1] = ["codex"];
pub const STATES: [&str; 3] = ["checkpoint", "active", "retired"];
pub const CLAIM_MODES: [&str; 2] = ["resume", "fork"];
pub const CLAIM_STATES: [&str; 3] = ["open", "advanced", "abandoned"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct LineageSession {
    pub id: String,
    pub project_id: String,
    pub line: String,
    pub role: String,
    pub kind: String,
    pub session_id: String,
    pub parent_session_id: Option<String>,
    pub context_tokens: Option<i64>,
    pub last_api_at: Option<String>,
    pub head_sha: Option<String>,
    pub state: String,
    pub summary: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct LineageClaim {
    pub id: String,
    pub project_id: String,
    pub line: String,
    pub mode: String,
    pub holder_bot_id: Option<String>,
    pub base_session_id: String,
    pub base_head_sha: Option<String>,
    pub backup_session_id: Option<String>,
    pub work_session_id: Option<String>,
    pub state: String,
    pub created_at: String,
    pub updated_at: String,
}

/// 建立或前移時可帶的欄位。`None`＝不動（既有列）或用預設（新列：role=worker、kind=codex）。`kind` 只收 [`KINDS`]。
#[derive(Debug, Clone, Default)]
pub struct Fields {
    pub role: Option<String>,
    pub kind: Option<String>,
    pub parent_session_id: Option<String>,
    pub context_tokens: Option<i64>,
    pub last_api_at: Option<String>,
    pub head_sha: Option<String>,
    pub summary: Option<String>,
}

#[derive(Debug)]
pub enum LineageError {
    /// 這個專案已經登記過這顆 session（`create` 不覆寫）。
    Exists(Box<LineageSession>),
    /// 這顆 session 登記在另一條線上，不能拿來當這條線的檢查點。
    LineMismatch { session_line: String },
    NotFound,
    /// 這條線目前沒有檢查點，不能認領。
    CheckpointNotFound,
    /// 這條線已經有 open 的 resume 認領。
    LineBusy(Box<LineageClaim>),
    ClaimNotFound,
    /// claim 已經不是 open（`advanced` 或 `abandoned`）。
    ClaimClosed(Box<LineageClaim>),
    /// 帶了不屬於這條線的 claim。
    ClaimLineMismatch { claim_line: String },
    /// fork claim 沒有 work_session_id，且呼叫端也沒給 session_id。
    ClaimMissingWorkSession,
    /// 呼叫端給的 session_id 與 claim 綁定的目標不一致。
    ClaimSessionMismatch { expected: String, got: String },
    /// 認領之後檢查點已經被別的寫入者前移。
    CheckpointMoved { checkpoint: Option<Box<LineageSession>> },
    Db(sqlx::Error),
}

impl From<sqlx::Error> for LineageError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

impl std::fmt::Display for LineageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exists(_) => write!(f, "session already recorded in this project"),
            Self::LineMismatch { session_line } => write!(f, "session belongs to line `{session_line}`"),
            Self::NotFound => write!(f, "session not found"),
            Self::CheckpointNotFound => write!(f, "checkpoint not found on this line"),
            Self::LineBusy(_) => write!(f, "line already has an open resume claim"),
            Self::ClaimNotFound => write!(f, "claim not found"),
            Self::ClaimClosed(c) => write!(f, "claim is already {}", c.state),
            Self::ClaimLineMismatch { claim_line } => write!(f, "claim belongs to line `{claim_line}`"),
            Self::ClaimMissingWorkSession => write!(f, "fork claim has no work_session_id"),
            Self::ClaimSessionMismatch { expected, got } => write!(f, "claim targets session `{expected}`, got `{got}`"),
            Self::CheckpointMoved { .. } => write!(f, "checkpoint moved since claim was opened"),
            Self::Db(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for LineageError {}

#[derive(Debug, Serialize)]
pub struct Advanced {
    /// 這條線現在的檢查點。
    pub session: LineageSession,
    /// 被取代而退役的前一顆檢查點（原本沒有、或前移的就是同一顆時為 `None`）。
    pub retired: Option<LineageSession>,
    /// 若本次前移帶了 `claim_id`，回傳更新為 `advanced` 的 claim 列。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claim: Option<LineageClaim>,
}

#[derive(Debug, Serialize)]
pub struct Claimed {
    pub claim: LineageClaim,
    pub checkpoint: LineageSession,
}

async fn by_session(conn: &mut sqlx::SqliteConnection, project_id: &str, session_id: &str) -> Result<Option<LineageSession>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM lineage_sessions WHERE project_id = ? AND session_id = ?").bind(project_id).bind(session_id).fetch_optional(conn).await
}

/// 專案的血統列；`line`／`state` 給了就篩。同一條線內檢查點排最前，其餘新的在前。
pub async fn list(pool: &SqlitePool, project_id: &str, line: Option<&str>, state: Option<&str>) -> Result<Vec<LineageSession>, sqlx::Error> {
    sqlx::query_as(
        "SELECT * FROM lineage_sessions WHERE project_id = ? AND (? IS NULL OR line = ?) AND (? IS NULL OR state = ?) \
         ORDER BY line, CASE state WHEN 'checkpoint' THEN 0 WHEN 'active' THEN 1 ELSE 2 END, updated_at DESC, id DESC",
    )
    .bind(project_id)
    .bind(line)
    .bind(line)
    .bind(state)
    .bind(state)
    .fetch_all(pool)
    .await
}

/// 某條線目前的檢查點。
pub async fn checkpoint(pool: &SqlitePool, project_id: &str, line: &str) -> Result<Option<LineageSession>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM lineage_sessions WHERE project_id = ? AND line = ? AND state = 'checkpoint'").bind(project_id).bind(line).fetch_optional(pool).await
}

async fn checkpoint_in_tx(conn: &mut sqlx::SqliteConnection, project_id: &str, line: &str) -> Result<Option<LineageSession>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM lineage_sessions WHERE project_id = ? AND line = ? AND state = 'checkpoint'").bind(project_id).bind(line).fetch_optional(conn).await
}

async fn claim_by_id(conn: &mut sqlx::SqliteConnection, project_id: &str, claim_id: &str) -> Result<Option<LineageClaim>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM lineage_claims WHERE id = ? AND project_id = ?").bind(claim_id).bind(project_id).fetch_optional(conn).await
}

/// 登記一顆進行中（`active`）的 session。同專案已登記過同一顆 session → [`LineageError::Exists`]。
pub async fn create(pool: &SqlitePool, project_id: &str, line: &str, session_id: &str, f: Fields) -> Result<LineageSession, LineageError> {
    let mut tx = crate::db::begin_write(pool).await?;
    if let Some(existing) = by_session(&mut tx, project_id, session_id).await? {
        return Err(LineageError::Exists(Box::new(existing)));
    }
    insert(&mut tx, project_id, line, session_id, "active", &f).await?;
    let row = by_session(&mut tx, project_id, session_id).await?.ok_or(LineageError::NotFound)?;
    tx.commit().await?;
    Ok(row)
}

async fn insert(conn: &mut sqlx::SqliteConnection, project_id: &str, line: &str, session_id: &str, state: &str, f: &Fields) -> Result<(), sqlx::Error> {
    let now = crate::db::now();
    sqlx::query(
        "INSERT INTO lineage_sessions (id, project_id, line, role, kind, session_id, parent_session_id, context_tokens, last_api_at, head_sha, state, summary, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(crate::db::ulid())
    .bind(project_id)
    .bind(line)
    .bind(f.role.as_deref().unwrap_or("worker"))
    .bind(f.kind.as_deref().unwrap_or("codex"))
    .bind(session_id)
    .bind(&f.parent_session_id)
    .bind(f.context_tokens)
    .bind(&f.last_api_at)
    .bind(&f.head_sha)
    .bind(state)
    .bind(&f.summary)
    .bind(&now)
    .bind(&now)
    .execute(conn)
    .await?;
    Ok(())
}

async fn advance_in_tx(
    tx: &mut sqlx::SqliteConnection,
    project_id: &str,
    line: &str,
    session_id: &str,
    f: Fields,
    now: &str,
) -> Result<(LineageSession, Option<LineageSession>), LineageError> {
    let existing = by_session(tx, project_id, session_id).await?;
    if let Some(row) = &existing {
        if row.line != line {
            return Err(LineageError::LineMismatch { session_line: row.line.clone() });
        }
    }
    let previous: Option<LineageSession> =
        sqlx::query_as("SELECT * FROM lineage_sessions WHERE project_id = ? AND line = ? AND state = 'checkpoint' AND session_id <> ?")
            .bind(project_id)
            .bind(line)
            .bind(session_id)
            .fetch_optional(&mut *tx)
            .await?;
    // 先退役舊的再升新的：部分唯一索引不容許同一條線同時有兩顆檢查點。
    if let Some(prev) = &previous {
        sqlx::query("UPDATE lineage_sessions SET state = 'retired', updated_at = ? WHERE id = ?").bind(now).bind(&prev.id).execute(&mut *tx).await?;
    }
    match &existing {
        Some(row) => {
            sqlx::query(
                "UPDATE lineage_sessions SET state = 'checkpoint', role = COALESCE(?, role), kind = COALESCE(?, kind), \
                 parent_session_id = COALESCE(?, parent_session_id), context_tokens = COALESCE(?, context_tokens), \
                 last_api_at = COALESCE(?, last_api_at), head_sha = COALESCE(?, head_sha), summary = COALESCE(?, summary), updated_at = ? WHERE id = ?",
            )
            .bind(&f.role)
            .bind(&f.kind)
            .bind(&f.parent_session_id)
            .bind(f.context_tokens)
            .bind(&f.last_api_at)
            .bind(&f.head_sha)
            .bind(&f.summary)
            .bind(now)
            .bind(&row.id)
            .execute(&mut *tx)
            .await?;
        }
        None => {
            let mut f = f;
            if f.parent_session_id.is_none() {
                f.parent_session_id = previous.as_ref().map(|p| p.session_id.clone());
            }
            insert(tx, project_id, line, session_id, "checkpoint", &f).await?;
        }
    }
    let session = by_session(tx, project_id, session_id).await?.ok_or(LineageError::NotFound)?;
    let retired = match &previous {
        Some(prev) => by_session(tx, project_id, &prev.session_id).await?,
        None => None,
    };
    Ok((session, retired))
}

/// 把 `session_id` 設為 `line` 的檢查點；原本的檢查點（若是另一顆）改 `retired`。
///
/// - 沒登記過的 session 當場建一列；沒給 `parent_session_id` 時記前一顆檢查點（血統鏈接得起來）。
/// - 已登記的列只更新有給的欄位；已退役的也能再前移（＝退回舊檢查點）。
/// - 已登記在別條線 → [`LineageError::LineMismatch`]，什麼都不改。
/// - 對同一顆重複前移是冪等的（`retired` 為 `None`）。
pub async fn advance(pool: &SqlitePool, project_id: &str, line: &str, session_id: &str, f: Fields) -> Result<Advanced, LineageError> {
    let mut tx = crate::db::begin_write(pool).await?;
    let now = crate::db::now();
    let (session, retired) = advance_in_tx(&mut tx, project_id, line, session_id, f, &now).await?;
    tx.commit().await?;
    Ok(Advanced { session, retired, claim: None })
}

/// 認領一條血統線（`resume` 單寫者獨佔，`fork` 可多筆並存）。該線沒有檢查點 → [`LineageError::CheckpointNotFound`]。
pub async fn claim(
    pool: &SqlitePool,
    project_id: &str,
    line: &str,
    mode: &str,
    holder_bot_id: Option<&str>,
    backup_session_id: Option<&str>,
    work_session_id: Option<&str>,
) -> Result<Claimed, LineageError> {
    let mut tx = crate::db::begin_write(pool).await?;
    let cp = checkpoint_in_tx(&mut tx, project_id, line).await?.ok_or(LineageError::CheckpointNotFound)?;
    if mode == "resume" {
        let busy: Option<LineageClaim> = sqlx::query_as(
            "SELECT * FROM lineage_claims WHERE project_id = ? AND line = ? AND mode = 'resume' AND state = 'open'",
        )
        .bind(project_id)
        .bind(line)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(existing) = busy {
            return Err(LineageError::LineBusy(Box::new(existing)));
        }
    }
    let id = crate::db::ulid();
    let now = crate::db::now();
    sqlx::query(
        "INSERT INTO lineage_claims (id, project_id, line, mode, holder_bot_id, base_session_id, base_head_sha, backup_session_id, work_session_id, state, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'open', ?, ?)",
    )
    .bind(&id)
    .bind(project_id)
    .bind(line)
    .bind(mode)
    .bind(holder_bot_id)
    .bind(&cp.session_id)
    .bind(&cp.head_sha)
    .bind(backup_session_id)
    .bind(work_session_id)
    .bind(&now)
    .bind(&now)
    .execute(&mut *tx)
    .await?;
    let claim = claim_by_id(&mut tx, project_id, &id).await?.ok_or(LineageError::ClaimNotFound)?;
    tx.commit().await?;
    Ok(Claimed { claim, checkpoint: cp })
}

/// 放棄一筆認領（`state = 'abandoned'`）；不自動改檢查點。已放棄的原樣回傳；已前移 → [`LineageError::ClaimClosed`]。
pub async fn abandon(pool: &SqlitePool, project_id: &str, claim_id: &str) -> Result<LineageClaim, LineageError> {
    let mut tx = crate::db::begin_write(pool).await?;
    let row = claim_by_id(&mut tx, project_id, claim_id).await?.ok_or(LineageError::ClaimNotFound)?;
    if row.state == "abandoned" {
        return Ok(row);
    }
    if row.state != "open" {
        return Err(LineageError::ClaimClosed(Box::new(row)));
    }
    sqlx::query("UPDATE lineage_claims SET state = 'abandoned', updated_at = ? WHERE id = ?")
        .bind(crate::db::now())
        .bind(&row.id)
        .execute(&mut *tx)
        .await?;
    let updated = claim_by_id(&mut tx, project_id, claim_id).await?.ok_or(LineageError::ClaimNotFound)?;
    tx.commit().await?;
    Ok(updated)
}

/// 帶 `claim_id` 前移：同一交易內核對 claim 屬於同專案同線、狀態為 `open`、且目前檢查點仍等於 `base_session_id`；
/// 通過後 `resume` 前移 `base_session_id` 本身（冪等更新），`fork` 前移 `work_session_id`，並把 claim 標為 `advanced`。
pub async fn advance_with_claim(
    pool: &SqlitePool,
    project_id: &str,
    line: &str,
    claim_id: &str,
    session_id: Option<&str>,
    f: Fields,
) -> Result<Advanced, LineageError> {
    let mut tx = crate::db::begin_write(pool).await?;
    let claim = claim_by_id(&mut tx, project_id, claim_id).await?.ok_or(LineageError::ClaimNotFound)?;
    if claim.line != line {
        return Err(LineageError::ClaimLineMismatch { claim_line: claim.line });
    }
    if claim.state != "open" {
        return Err(LineageError::ClaimClosed(Box::new(claim)));
    }
    let current = checkpoint_in_tx(&mut tx, project_id, line).await?;
    if current.as_ref().map(|c| c.session_id.as_str()) != Some(claim.base_session_id.as_str()) {
        return Err(LineageError::CheckpointMoved { checkpoint: current.map(Box::new) });
    }
    let target = match claim.mode.as_str() {
        "resume" => claim.base_session_id.clone(),
        "fork" => match (&claim.work_session_id, session_id) {
            (Some(w), _) => w.clone(),
            (None, Some(s)) => s.to_string(),
            (None, None) => return Err(LineageError::ClaimMissingWorkSession),
        },
        _ => return Err(LineageError::ClaimNotFound),
    };
    if let Some(given) = session_id {
        if given != target {
            return Err(LineageError::ClaimSessionMismatch { expected: target, got: given.to_string() });
        }
    }
    let now = crate::db::now();
    let (session, retired) = advance_in_tx(&mut tx, project_id, line, &target, f, &now).await?;
    sqlx::query("UPDATE lineage_claims SET state = 'advanced', updated_at = ? WHERE id = ?")
        .bind(&now)
        .bind(&claim.id)
        .execute(&mut *tx)
        .await?;
    let updated_claim = claim_by_id(&mut tx, project_id, claim_id).await?.ok_or(LineageError::ClaimNotFound)?;
    tx.commit().await?;
    Ok(Advanced { session, retired, claim: Some(updated_claim) })
}

/// 退役一顆 session（檢查點退役後那條線就沒有檢查點）。已退役的原樣回傳；沒登記 → [`LineageError::NotFound`]。
pub async fn retire(pool: &SqlitePool, project_id: &str, session_id: &str) -> Result<LineageSession, LineageError> {
    let mut tx = crate::db::begin_write(pool).await?;
    let row = by_session(&mut tx, project_id, session_id).await?.ok_or(LineageError::NotFound)?;
    if row.state == "retired" {
        return Ok(row);
    }
    sqlx::query("UPDATE lineage_sessions SET state = 'retired', updated_at = ? WHERE id = ?").bind(crate::db::now()).bind(&row.id).execute(&mut *tx).await?;
    let row = by_session(&mut tx, project_id, session_id).await?.ok_or(LineageError::NotFound)?;
    tx.commit().await?;
    Ok(row)
}

#[cfg(all(test, not(feature = "daemon-test-harness")))]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        sqlx::query("PRAGMA foreign_keys=ON").execute(&pool).await.unwrap();
        super::super::migrate_with(&pool, &[]).await.unwrap();
        for id in ["p1", "p2"] {
            sqlx::query("INSERT INTO projects(id,path,label,created_at)VALUES(?,?,?,?)").bind(id).bind(format!("/test/{id}")).bind(id).bind(crate::db::now()).execute(&pool).await.unwrap();
        }
        pool
    }

    async fn states(pool: &SqlitePool, project: &str, line: &str) -> Vec<(String, String)> {
        sqlx::query_as("SELECT session_id, state FROM lineage_sessions WHERE project_id = ? AND line = ? ORDER BY session_id").bind(project).bind(line).fetch_all(pool).await.unwrap()
    }

    fn pair(a: &str, b: &str) -> (String, String) {
        (a.to_string(), b.to_string())
    }

    #[tokio::test]
    async fn a_line_holds_at_most_one_checkpoint() {
        let pool = pool().await;
        advance(&pool, "p1", "web", "s1", Fields::default()).await.unwrap();
        create(&pool, "p1", "web", "s2", Fields::default()).await.unwrap();
        // 繞過 advance 直接把第二顆改成 checkpoint：部分唯一索引擋下。
        let err = sqlx::query("UPDATE lineage_sessions SET state='checkpoint' WHERE session_id='s2'").execute(&pool).await.unwrap_err();
        assert!(err.to_string().contains("UNIQUE"), "{err}");
        let err = sqlx::query("INSERT INTO lineage_sessions(id,project_id,line,session_id,state,created_at,updated_at)VALUES('x','p1','web','s3','checkpoint','t','t')").execute(&pool).await.unwrap_err();
        assert!(err.to_string().contains("UNIQUE"), "{err}");
        // 別條線、別的專案各自有自己的檢查點。
        advance(&pool, "p1", "quota", "s4", Fields::default()).await.unwrap();
        advance(&pool, "p2", "web", "s1", Fields::default()).await.unwrap();
        // retired／active 不限數量。
        create(&pool, "p1", "web", "s5", Fields::default()).await.unwrap();
        retire(&pool, "p1", "s5").await.unwrap();
        retire(&pool, "p1", "s2").await.unwrap();
        assert_eq!(states(&pool, "p1", "web").await, vec![pair("s1", "checkpoint"), pair("s2", "retired"), pair("s5", "retired")]);
    }

    #[tokio::test]
    async fn schema_rejects_unknown_role_state_and_duplicate_session() {
        let pool = pool().await;
        for (role, state) in [("boss", "active"), ("worker", "paused")] {
            let r = sqlx::query("INSERT INTO lineage_sessions(id,project_id,line,role,session_id,state,created_at,updated_at)VALUES(?,'p1','web',?,'s',?,'t','t')")
                .bind(crate::db::ulid()).bind(role).bind(state).execute(&pool).await;
            assert!(r.unwrap_err().to_string().contains("CHECK"), "{role}/{state}");
        }
        // 血統只給 codex：其他 kind 在 DB 就擋（直接 INSERT、UPDATE、經 create／advance 都一樣）。
        for kind in ["claude", "grok", "agy", "claude-api", ""] {
            let r = sqlx::query("INSERT INTO lineage_sessions(id,project_id,line,kind,session_id,created_at,updated_at)VALUES(?,'p1','web',?,'k','t','t')").bind(crate::db::ulid()).bind(kind).execute(&pool).await;
            assert!(r.unwrap_err().to_string().contains("CHECK"), "kind {kind:?}");
        }
        assert!(matches!(create(&pool, "p1", "web", "k", Fields { kind: Some("claude".into()), ..Default::default() }).await, Err(LineageError::Db(_))));
        assert!(matches!(advance(&pool, "p1", "web", "k", Fields { kind: Some("claude".into()), ..Default::default() }).await, Err(LineageError::Db(_))));
        create(&pool, "p1", "web", "s1", Fields::default()).await.unwrap();
        assert!(sqlx::query("UPDATE lineage_sessions SET kind='claude' WHERE session_id='s1'").execute(&pool).await.unwrap_err().to_string().contains("CHECK"));
        assert!(matches!(advance(&pool, "p1", "web", "s1", Fields { kind: Some("claude".into()), ..Default::default() }).await, Err(LineageError::Db(_))));
        assert_eq!(states(&pool, "p1", "web").await, vec![pair("s1", "active")], "被擋的寫入不留痕跡");
        let r = sqlx::query("INSERT INTO lineage_sessions(id,project_id,line,session_id,created_at,updated_at)VALUES('y','p1','quota','s1','t','t')").execute(&pool).await;
        assert!(r.unwrap_err().to_string().contains("UNIQUE"), "同專案同一顆 session 只有一列");
        let r = sqlx::query("INSERT INTO lineage_sessions(id,project_id,line,session_id,created_at,updated_at)VALUES('z','nope','web','s9','t','t')").execute(&pool).await;
        assert!(r.is_err(), "專案要存在");
    }

    #[tokio::test]
    async fn advance_moves_the_checkpoint_and_retires_the_old_one() {
        let pool = pool().await;
        let first = advance(&pool, "p1", "web", "s1", Fields { head_sha: Some("aaaaaaa".into()), summary: Some("seed".into()), ..Default::default() }).await.unwrap();
        assert_eq!((first.session.state.as_str(), first.session.role.as_str(), first.session.kind.as_str()), ("checkpoint", "worker", "codex"));
        assert!(first.retired.is_none() && first.session.parent_session_id.is_none());

        // 進行中的 child 做完 → 前移：舊的退役、新的沿用登記時的 parent，只更新有給的欄位。
        create(&pool, "p1", "web", "s2", Fields { parent_session_id: Some("s1".into()), summary: Some("wip".into()), ..Default::default() }).await.unwrap();
        let second = advance(&pool, "p1", "web", "s2", Fields { head_sha: Some("bbbbbbb".into()), ..Default::default() }).await.unwrap();
        assert_eq!(second.retired.as_ref().map(|r| (r.session_id.as_str(), r.state.as_str())), Some(("s1", "retired")));
        assert_eq!((second.session.state.as_str(), second.session.head_sha.as_deref(), second.session.summary.as_deref(), second.session.parent_session_id.as_deref()), ("checkpoint", Some("bbbbbbb"), Some("wip"), Some("s1")));

        // 沒登記過、也沒給 parent：記前一顆檢查點。
        let third = advance(&pool, "p1", "web", "s3", Fields::default()).await.unwrap();
        assert_eq!(third.session.parent_session_id.as_deref(), Some("s2"));
        // 給了 parent 就照給的。
        let fourth = advance(&pool, "p1", "web", "s4", Fields { parent_session_id: Some("s1".into()), ..Default::default() }).await.unwrap();
        assert_eq!(fourth.session.parent_session_id.as_deref(), Some("s1"));

        // 冪等：同一顆再前移一次，不退役任何東西。
        let again = advance(&pool, "p1", "web", "s4", Fields::default()).await.unwrap();
        assert!(again.retired.is_none());
        // 退回舊檢查點：已退役的可以再前移。
        let back = advance(&pool, "p1", "web", "s2", Fields::default()).await.unwrap();
        assert_eq!(back.retired.as_ref().map(|r| r.session_id.as_str()), Some("s4"));
        assert_eq!(states(&pool, "p1", "web").await, vec![pair("s1", "retired"), pair("s2", "checkpoint"), pair("s3", "retired"), pair("s4", "retired")]);
        assert_eq!(checkpoint(&pool, "p1", "web").await.unwrap().unwrap().session_id, "s2");
    }

    #[tokio::test]
    async fn advance_refuses_a_session_of_another_line_and_changes_nothing() {
        let pool = pool().await;
        advance(&pool, "p1", "web", "s1", Fields::default()).await.unwrap();
        advance(&pool, "p1", "quota", "q1", Fields::default()).await.unwrap();
        let err = advance(&pool, "p1", "web", "q1", Fields::default()).await.unwrap_err();
        assert!(matches!(err, LineageError::LineMismatch { ref session_line } if session_line == "quota"), "{err:?}");
        assert_eq!(states(&pool, "p1", "web").await, vec![pair("s1", "checkpoint")]);
        assert_eq!(states(&pool, "p1", "quota").await, vec![pair("q1", "checkpoint")]);
    }

    #[tokio::test]
    async fn create_retire_and_list_stay_inside_the_project() {
        let pool = pool().await;
        create(&pool, "p1", "web", "s1", Fields { role: Some("thinker".into()), context_tokens: Some(1200), ..Default::default() }).await.unwrap();
        assert!(matches!(create(&pool, "p1", "quota", "s1", Fields::default()).await, Err(LineageError::Exists(_))));
        // 別的專案的同名 session 是另一列；p2 退役不到 p1 的。
        create(&pool, "p2", "web", "s1", Fields::default()).await.unwrap();
        assert!(matches!(retire(&pool, "p2", "nope").await, Err(LineageError::NotFound)));
        let r = retire(&pool, "p2", "s1").await.unwrap();
        assert_eq!(r.state, "retired");
        assert_eq!(retire(&pool, "p2", "s1").await.unwrap(), r, "已退役的原樣回傳");
        let p1 = list(&pool, "p1", None, None).await.unwrap();
        assert_eq!(p1.iter().map(|s| (s.session_id.as_str(), s.state.as_str(), s.role.as_str(), s.context_tokens)).collect::<Vec<_>>(), vec![("s1", "active", "thinker", Some(1200))]);

        advance(&pool, "p1", "web", "s2", Fields::default()).await.unwrap();
        advance(&pool, "p1", "quota", "q1", Fields::default()).await.unwrap();
        let web: Vec<_> = list(&pool, "p1", Some("web"), None).await.unwrap().into_iter().map(|s| s.session_id).collect();
        assert_eq!(web, vec!["s2", "s1"], "檢查點排最前");
        let cps: Vec<_> = list(&pool, "p1", None, Some("checkpoint")).await.unwrap().into_iter().map(|s| (s.line, s.session_id)).collect();
        assert_eq!(cps, vec![pair("quota", "q1"), pair("web", "s2")]);
    }

    #[tokio::test]
    async fn claim_resume_is_exclusive_per_line_and_fork_claims_coexist() {
        let pool = pool().await;
        assert!(matches!(claim(&pool, "p1", "web", "resume", Some("bot-1"), Some("b1"), None).await, Err(LineageError::CheckpointNotFound)));

        advance(&pool, "p1", "web", "s1", Fields { head_sha: Some("aaaaaaa".into()), ..Default::default() }).await.unwrap();
        let c1 = claim(&pool, "p1", "web", "resume", Some("bot-1"), Some("b1"), None).await.unwrap();
        assert_eq!((c1.claim.mode.as_str(), c1.claim.state.as_str(), c1.claim.base_session_id.as_str()), ("resume", "open", "s1"));
        assert_eq!((c1.claim.holder_bot_id.as_deref(), c1.claim.backup_session_id.as_deref(), c1.claim.base_head_sha.as_deref()), (Some("bot-1"), Some("b1"), Some("aaaaaaa")));
        assert_eq!(c1.checkpoint.session_id, "s1");

        // 第二筆 open resume 認領被拒，且 DB 部分唯一索引也擋。
        let busy = claim(&pool, "p1", "web", "resume", Some("bot-2"), Some("b2"), None).await.unwrap_err();
        assert!(matches!(busy, LineageError::LineBusy(ref existing) if existing.id == c1.claim.id), "{busy:?}");
        let sql_err = sqlx::query(
            "INSERT INTO lineage_claims (id, project_id, line, mode, base_session_id, state, created_at, updated_at) VALUES ('x', 'p1', 'web', 'resume', 's1', 'open', 't', 't')",
        )
        .execute(&pool)
        .await
        .unwrap_err();
        assert!(sql_err.to_string().contains("UNIQUE"), "{sql_err}");

        // fork 認領可多筆並存，不受 open resume 影響；別條線、別的專案各自獨立。
        let f1 = claim(&pool, "p1", "web", "fork", None, None, Some("w1")).await.unwrap();
        let f2 = claim(&pool, "p1", "web", "fork", Some("bot-2"), None, Some("w2")).await.unwrap();
        assert_ne!(f1.claim.id, f2.claim.id);
        advance(&pool, "p1", "quota", "q1", Fields::default()).await.unwrap();
        claim(&pool, "p1", "quota", "resume", None, None, None).await.unwrap();
        advance(&pool, "p2", "web", "s1", Fields::default()).await.unwrap();
        claim(&pool, "p2", "web", "resume", None, None, None).await.unwrap();

        // abandon 後可再認領 resume，且檢查點不動。
        let ab = abandon(&pool, "p1", &c1.claim.id).await.unwrap();
        assert_eq!(ab.state, "abandoned");
        assert_eq!(checkpoint(&pool, "p1", "web").await.unwrap().unwrap().session_id, "s1");
        let c2 = claim(&pool, "p1", "web", "resume", None, Some("b3"), None).await.unwrap();
        assert_ne!(c2.claim.id, c1.claim.id);
    }

    #[tokio::test]
    async fn advance_with_claim_checks_state_line_and_checkpoint_version() {
        let pool = pool().await;
        advance(&pool, "p1", "web", "s1", Fields { head_sha: Some("aaaaaaa".into()), ..Default::default() }).await.unwrap();
        let c_res = claim(&pool, "p1", "web", "resume", Some("bot-1"), Some("b1"), None).await.unwrap();
        let c_f1 = claim(&pool, "p1", "web", "fork", None, None, Some("w1")).await.unwrap();
        let c_f2 = claim(&pool, "p1", "web", "fork", None, None, Some("w2")).await.unwrap();

        // 跨線、跨專案拒絕。
        assert!(matches!(advance_with_claim(&pool, "p1", "quota", &c_res.claim.id, None, Fields::default()).await, Err(LineageError::ClaimLineMismatch { .. })));
        assert!(matches!(advance_with_claim(&pool, "p2", "web", &c_res.claim.id, None, Fields::default()).await, Err(LineageError::ClaimNotFound)));
        assert!(matches!(abandon(&pool, "p2", &c_res.claim.id).await, Err(LineageError::ClaimNotFound)));

        // resume 模式前移 base session 本身（冪等更新），claim 變 advanced。
        let adv_res = advance_with_claim(&pool, "p1", "web", &c_res.claim.id, None, Fields { head_sha: Some("bbbbbbb".into()), summary: Some("resumed".into()), ..Default::default() }).await.unwrap();
        assert_eq!((adv_res.session.session_id.as_str(), adv_res.session.head_sha.as_deref()), ("s1", Some("bbbbbbb")));
        assert!(adv_res.retired.is_none());
        assert_eq!(adv_res.claim.as_ref().unwrap().state, "advanced");

        // claim 關閉後不能再前移也不能 abandon。
        assert!(matches!(advance_with_claim(&pool, "p1", "web", &c_res.claim.id, None, Fields::default()).await, Err(LineageError::ClaimClosed(_))));
        assert!(matches!(abandon(&pool, "p1", &c_res.claim.id).await, Err(LineageError::ClaimClosed(_))));

        // fork 模式前移 work_session_id：s1 退役、w1 成新檢查點。
        let adv_f1 = advance_with_claim(&pool, "p1", "web", &c_f1.claim.id, None, Fields { head_sha: Some("ccccccc".into()), ..Default::default() }).await.unwrap();
        assert_eq!(adv_f1.session.session_id, "w1");
        assert_eq!(adv_f1.retired.as_ref().map(|r| r.session_id.as_str()), Some("s1"));
        assert_eq!(adv_f1.claim.as_ref().unwrap().state, "advanced");

        // 過期的 fork claim（base=s1，但檢查點已經移到 w1）：被拒且什麼都沒改。
        let stale = advance_with_claim(&pool, "p1", "web", &c_f2.claim.id, None, Fields::default()).await.unwrap_err();
        assert!(matches!(stale, LineageError::CheckpointMoved { checkpoint: Some(ref cp) } if cp.session_id == "w1"), "{stale:?}");
        assert_eq!(checkpoint(&pool, "p1", "web").await.unwrap().unwrap().session_id, "w1");
        assert!(by_session(&mut *pool.acquire().await.unwrap(), "p1", "w2").await.unwrap().is_none());

        // abandon 後不能再前移；重複 abandon 冪等。
        let ab = abandon(&pool, "p1", &c_f2.claim.id).await.unwrap();
        assert_eq!(ab.state, "abandoned");
        assert_eq!(abandon(&pool, "p1", &c_f2.claim.id).await.unwrap().state, "abandoned");
        assert!(matches!(advance_with_claim(&pool, "p1", "web", &c_f2.claim.id, None, Fields::default()).await, Err(LineageError::ClaimClosed(_))));
    }
}
