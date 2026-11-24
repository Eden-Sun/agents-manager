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
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub tokens_include_parent: bool,
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
    pub input_tokens: Option<i64>,
    pub cached_input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
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
    /// 只有 fork claim 可以登記 work_session_id。
    ClaimNotFork,
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
            Self::ClaimNotFork => write!(f, "only fork claims accept work_session_id"),
            Self::ClaimSessionMismatch { expected, got } => write!(f, "claim targets session `{expected}`, got `{got}`"),
            Self::CheckpointMoved { .. } => write!(f, "checkpoint moved since claim was opened"),
            Self::Db(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for LineageError {}

#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct LineTokens {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub hit_ratio: f64,
    pub saved_input_tokens: i64,
    pub includes_parent: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LineStat {
    pub line: String,
    pub checkpoint: Option<LineageSession>,
    pub open_claims_count: i64,
    pub recent_claims: Vec<LineageClaim>,
    pub tokens: LineTokens,
}

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

/// 依 `(project_id, session_id)` 查單一 session。
pub async fn session(pool: &SqlitePool, project_id: &str, session_id: &str) -> Result<Option<LineageSession>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM lineage_sessions WHERE project_id = ? AND session_id = ?").bind(project_id).bind(session_id).fetch_optional(pool).await
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

/// 專案各血統線統計：每條線的檢查點、open claim 數、近期 claim 與 token 節省合計。
pub async fn stats(pool: &SqlitePool, project_id: &str) -> Result<Vec<LineStat>, sqlx::Error> {
    let lines: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT line FROM ( \
            SELECT line FROM lineage_sessions WHERE project_id = ? \
            UNION \
            SELECT line FROM lineage_claims WHERE project_id = ? \
         ) ORDER BY line",
    )
    .bind(project_id)
    .bind(project_id)
    .fetch_all(pool)
    .await?;

    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let cp: Option<LineageSession> = sqlx::query_as(
            "SELECT * FROM lineage_sessions WHERE project_id = ? AND line = ? AND state = 'checkpoint' LIMIT 1",
        )
        .bind(project_id)
        .bind(&line)
        .fetch_optional(pool)
        .await?;

        let open_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM lineage_claims WHERE project_id = ? AND line = ? AND state = 'open'",
        )
        .bind(project_id)
        .bind(&line)
        .fetch_one(pool)
        .await?;

        let recent_claims: Vec<LineageClaim> = sqlx::query_as(
            "SELECT * FROM lineage_claims WHERE project_id = ? AND line = ? ORDER BY created_at DESC, id DESC LIMIT 5",
        )
        .bind(project_id)
        .bind(&line)
        .fetch_all(pool)
        .await?;

        let (input, cached, output, saved, includes_parent_i): (i64, i64, i64, i64, i64) = sqlx::query_as(
            "SELECT COALESCE(SUM(input_tokens), 0), COALESCE(SUM(cached_input_tokens), 0), COALESCE(SUM(output_tokens), 0), \
                    COALESCE(SUM(CASE WHEN tokens_include_parent = 0 THEN cached_input_tokens ELSE 0 END), 0), \
                    COALESCE(MAX(tokens_include_parent), 0) \
             FROM lineage_sessions WHERE project_id = ? AND line = ?",
        )
        .bind(project_id)
        .bind(&line)
        .fetch_one(pool)
        .await?;

        let hit_ratio = if input > 0 {
            (cached as f64) / (input as f64)
        } else {
            0.0
        };

        out.push(LineStat {
            line,
            checkpoint: cp,
            open_claims_count: open_count,
            recent_claims,
            tokens: LineTokens {
                input_tokens: input,
                cached_input_tokens: cached,
                output_tokens: output,
                hit_ratio,
                saved_input_tokens: saved,
                includes_parent: includes_parent_i != 0,
            },
        });
    }

    Ok(out)
}

/// 將 rollout 最後一筆 `total_token_usage` 寫回 `lineage_sessions`（覆寫，不累加；#1943 D2）。
///
/// 若該 session 有 `parent_session_id`：
/// - 沿祖先鏈加總已記錄的 token（因為 codex fork 的 rollout `total_token_usage` 含父歷史）；
/// - 若直接父 session 已有記錄（任一欄位 > 0），扣除整個祖先鏈的累計，記為「自己的用量」（`tokens_include_parent = 0`）；
/// - 若直接父尚未記錄（不存在或三欄皆 0），照原值寫入並標記 `tokens_include_parent = 1`，避免 stats 把父歷史當成本線省下的量。
///
/// 找不到該 `(project_id, session_id)` 時回 `Ok(false)`。
pub async fn record_rollout_tokens(
    pool: &SqlitePool,
    project_id: &str,
    session_id: &str,
    total_input: i64,
    total_cached: i64,
    total_output: i64,
    context_tokens: Option<i64>,
) -> Result<bool, sqlx::Error> {
    let mut tx = crate::db::begin_write(pool).await?;
    let Some(row) = by_session(&mut tx, project_id, session_id).await? else {
        return Ok(false);
    };
    let (own_in, own_cached, own_out, include_parent) = match row.parent_session_id.as_deref() {
        None => (total_input.max(0), total_cached.max(0), total_output.max(0), false),
        Some(first_parent) => {
            let mut visited = std::collections::HashSet::new();
            visited.insert(session_id.to_string());
            let mut cur_id = Some(first_parent.to_string());
            let mut anc_in: i64 = 0;
            let mut anc_cached: i64 = 0;
            let mut anc_out: i64 = 0;
            let mut direct_parent_recorded = false;
            let mut is_first = true;
            while let Some(pid) = cur_id {
                if visited.len() >= 64 || !visited.insert(pid.clone()) {
                    break;
                }
                let Some(parent) = by_session(&mut tx, project_id, &pid).await? else {
                    break;
                };
                if is_first && (parent.input_tokens > 0 || parent.cached_input_tokens > 0 || parent.output_tokens > 0) {
                    direct_parent_recorded = true;
                }
                is_first = false;
                anc_in = anc_in.saturating_add(parent.input_tokens.max(0));
                anc_cached = anc_cached.saturating_add(parent.cached_input_tokens.max(0));
                anc_out = anc_out.saturating_add(parent.output_tokens.max(0));
                cur_id = parent.parent_session_id;
            }
            if direct_parent_recorded {
                (
                    total_input.saturating_sub(anc_in).max(0),
                    total_cached.saturating_sub(anc_cached).max(0),
                    total_output.saturating_sub(anc_out).max(0),
                    false,
                )
            } else {
                (total_input.max(0), total_cached.max(0), total_output.max(0), true)
            }
        }
    };
    let ctx = context_tokens.filter(|&n| n >= 0);
    let now = crate::db::now();
    sqlx::query(
        "UPDATE lineage_sessions \
         SET input_tokens = ?, cached_input_tokens = ?, output_tokens = ?, tokens_include_parent = ?, \
             context_tokens = COALESCE(?, context_tokens), updated_at = ? \
         WHERE id = ?",
    )
    .bind(own_in)
    .bind(own_cached)
    .bind(own_out)
    .bind(i64::from(include_parent))
    .bind(ctx)
    .bind(&now)
    .bind(&row.id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
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
        "INSERT INTO lineage_sessions (id, project_id, line, role, kind, session_id, parent_session_id, context_tokens, input_tokens, cached_input_tokens, output_tokens, last_api_at, head_sha, state, summary, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(crate::db::ulid())
    .bind(project_id)
    .bind(line)
    .bind(f.role.as_deref().unwrap_or("worker"))
    .bind(f.kind.as_deref().unwrap_or("codex"))
    .bind(session_id)
    .bind(&f.parent_session_id)
    .bind(f.context_tokens)
    .bind(f.input_tokens.unwrap_or(0))
    .bind(f.cached_input_tokens.unwrap_or(0))
    .bind(f.output_tokens.unwrap_or(0))
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
    if let Some(prev) = &previous {
        sqlx::query("UPDATE lineage_sessions SET state = 'retired', updated_at = ? WHERE id = ?").bind(now).bind(&prev.id).execute(&mut *tx).await?;
    }
    match &existing {
        Some(row) => {
            sqlx::query(
                "UPDATE lineage_sessions SET state = 'checkpoint', role = COALESCE(?, role), kind = COALESCE(?, kind), \
                 parent_session_id = COALESCE(?, parent_session_id), context_tokens = COALESCE(?, context_tokens), \
                 input_tokens = COALESCE(?, input_tokens), cached_input_tokens = COALESCE(?, cached_input_tokens), output_tokens = COALESCE(?, output_tokens), \
                 last_api_at = COALESCE(?, last_api_at), head_sha = COALESCE(?, head_sha), summary = COALESCE(?, summary), updated_at = ? WHERE id = ?",
            )
            .bind(&f.role)
            .bind(&f.kind)
            .bind(&f.parent_session_id)
            .bind(f.context_tokens)
            .bind(f.input_tokens)
            .bind(f.cached_input_tokens)
            .bind(f.output_tokens)
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
    sqlx::query(
        "UPDATE lineage_claims SET state = 'abandoned', updated_at = ? \
         WHERE project_id = ? AND line = ? AND state = 'open' AND base_session_id <> ?",
    )
    .bind(now)
    .bind(project_id)
    .bind(line)
    .bind(session_id)
    .execute(&mut *tx)
    .await?;
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
            if existing.base_session_id == cp.session_id {
                return Err(LineageError::LineBusy(Box::new(existing)));
            }
            sqlx::query("UPDATE lineage_claims SET state = 'abandoned', updated_at = ? WHERE id = ?")
                .bind(crate::db::now())
                .bind(&existing.id)
                .execute(&mut *tx)
                .await?;
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

/// 補記 `fork` claim 的 `work_session_id`（開工認領後才啟動 worker 拿到新 session id 時使用）。
pub async fn set_claim_work_session(
    pool: &SqlitePool,
    project_id: &str,
    claim_id: &str,
    work_session_id: &str,
) -> Result<LineageClaim, LineageError> {
    let mut tx = crate::db::begin_write(pool).await?;
    let row = claim_by_id(&mut tx, project_id, claim_id).await?.ok_or(LineageError::ClaimNotFound)?;
    if row.state != "open" {
        return Err(LineageError::ClaimClosed(Box::new(row)));
    }
    if row.mode != "fork" {
        return Err(LineageError::ClaimNotFork);
    }
    sqlx::query("UPDATE lineage_claims SET work_session_id = ?, updated_at = ? WHERE id = ?")
        .bind(work_session_id)
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
/// `line` 為空字串時直接沿用 claim 記的線名。
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
    let line = if line.is_empty() {
        claim.line.clone()
    } else if claim.line != line {
        return Err(LineageError::ClaimLineMismatch { claim_line: claim.line });
    } else {
        line.to_string()
    };
    if claim.state != "open" {
        return Err(LineageError::ClaimClosed(Box::new(claim)));
    }
    let current = checkpoint_in_tx(&mut tx, project_id, &line).await?;
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
    let (session, retired) = advance_in_tx(&mut tx, project_id, &line, &target, f, &now).await?;
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

#[cfg(test)]
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

    /// fork 把檢查點移走之後，舊檢查點上那筆 open 的 resume 認領不再擋新的 resume 認領（自動改 abandoned）。
    #[tokio::test]
    async fn a_resume_claim_left_on_a_moved_checkpoint_does_not_block_the_line() {
        let pool = pool().await;
        advance(&pool, "p1", "web", "s1", Fields::default()).await.unwrap();
        let stale = claim(&pool, "p1", "web", "resume", None, Some("bak"), None).await.unwrap().claim;
        let fork = claim(&pool, "p1", "web", "fork", None, None, Some("w1")).await.unwrap().claim;
        assert!(matches!(claim(&pool, "p1", "web", "resume", None, None, None).await, Err(LineageError::LineBusy(_))));
        advance_with_claim(&pool, "p1", "web", &fork.id, None, Fields::default()).await.unwrap();
        let fresh = claim(&pool, "p1", "web", "resume", None, None, None).await.unwrap();
        assert_eq!(fresh.checkpoint.session_id, "w1");
        let old: LineageClaim = sqlx::query_as("SELECT * FROM lineage_claims WHERE id = ?").bind(&stale.id).fetch_one(&pool).await.unwrap();
        assert_eq!(old.state, "abandoned");
    }

    #[tokio::test]
    async fn a_line_holds_at_most_one_checkpoint() {
        let pool = pool().await;
        advance(&pool, "p1", "web", "s1", Fields::default()).await.unwrap();
        create(&pool, "p1", "web", "s2", Fields::default()).await.unwrap();
        let err = sqlx::query("UPDATE lineage_sessions SET state='checkpoint' WHERE session_id='s2'").execute(&pool).await.unwrap_err();
        assert!(err.to_string().contains("UNIQUE"), "{err}");
        let err = sqlx::query("INSERT INTO lineage_sessions(id,project_id,line,session_id,state,created_at,updated_at)VALUES('x','p1','web','s3','checkpoint','t','t')").execute(&pool).await.unwrap_err();
        assert!(err.to_string().contains("UNIQUE"), "{err}");
        advance(&pool, "p1", "quota", "s4", Fields::default()).await.unwrap();
        advance(&pool, "p2", "web", "s1", Fields::default()).await.unwrap();
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

        create(&pool, "p1", "web", "s2", Fields { parent_session_id: Some("s1".into()), summary: Some("wip".into()), ..Default::default() }).await.unwrap();
        let second = advance(&pool, "p1", "web", "s2", Fields { head_sha: Some("bbbbbbb".into()), ..Default::default() }).await.unwrap();
        assert_eq!(second.retired.as_ref().map(|r| (r.session_id.as_str(), r.state.as_str())), Some(("s1", "retired")));
        assert_eq!((second.session.state.as_str(), second.session.head_sha.as_deref(), second.session.summary.as_deref(), second.session.parent_session_id.as_deref()), ("checkpoint", Some("bbbbbbb"), Some("wip"), Some("s1")));

        let third = advance(&pool, "p1", "web", "s3", Fields::default()).await.unwrap();
        assert_eq!(third.session.parent_session_id.as_deref(), Some("s2"));
        let fourth = advance(&pool, "p1", "web", "s4", Fields { parent_session_id: Some("s1".into()), ..Default::default() }).await.unwrap();
        assert_eq!(fourth.session.parent_session_id.as_deref(), Some("s1"));

        let again = advance(&pool, "p1", "web", "s4", Fields::default()).await.unwrap();
        assert!(again.retired.is_none());
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

        let busy = claim(&pool, "p1", "web", "resume", Some("bot-2"), Some("b2"), None).await.unwrap_err();
        assert!(matches!(busy, LineageError::LineBusy(ref existing) if existing.id == c1.claim.id), "{busy:?}");
        let sql_err = sqlx::query(
            "INSERT INTO lineage_claims (id, project_id, line, mode, base_session_id, state, created_at, updated_at) VALUES ('x', 'p1', 'web', 'resume', 's1', 'open', 't', 't')",
        )
        .execute(&pool)
        .await
        .unwrap_err();
        assert!(sql_err.to_string().contains("UNIQUE"), "{sql_err}");

        let f1 = claim(&pool, "p1", "web", "fork", None, None, Some("w1")).await.unwrap();
        let f2 = claim(&pool, "p1", "web", "fork", Some("bot-2"), None, Some("w2")).await.unwrap();
        assert_ne!(f1.claim.id, f2.claim.id);
        advance(&pool, "p1", "quota", "q1", Fields::default()).await.unwrap();
        claim(&pool, "p1", "quota", "resume", None, None, None).await.unwrap();
        advance(&pool, "p2", "web", "s1", Fields::default()).await.unwrap();
        claim(&pool, "p2", "web", "resume", None, None, None).await.unwrap();

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

        assert!(matches!(advance_with_claim(&pool, "p1", "quota", &c_res.claim.id, None, Fields::default()).await, Err(LineageError::ClaimLineMismatch { .. })));
        assert!(matches!(advance_with_claim(&pool, "p2", "web", &c_res.claim.id, None, Fields::default()).await, Err(LineageError::ClaimNotFound)));
        assert!(matches!(abandon(&pool, "p2", &c_res.claim.id).await, Err(LineageError::ClaimNotFound)));

        let adv_res = advance_with_claim(&pool, "p1", "web", &c_res.claim.id, None, Fields { head_sha: Some("bbbbbbb".into()), summary: Some("resumed".into()), ..Default::default() }).await.unwrap();
        assert_eq!((adv_res.session.session_id.as_str(), adv_res.session.head_sha.as_deref()), ("s1", Some("bbbbbbb")));
        assert!(adv_res.retired.is_none());
        assert_eq!(adv_res.claim.as_ref().unwrap().state, "advanced");

        assert!(matches!(advance_with_claim(&pool, "p1", "web", &c_res.claim.id, None, Fields::default()).await, Err(LineageError::ClaimClosed(_))));
        assert!(matches!(abandon(&pool, "p1", &c_res.claim.id).await, Err(LineageError::ClaimClosed(_))));

        let adv_f1 = advance_with_claim(&pool, "p1", "web", &c_f1.claim.id, None, Fields { head_sha: Some("ccccccc".into()), ..Default::default() }).await.unwrap();
        assert_eq!(adv_f1.session.session_id, "w1");
        assert_eq!(adv_f1.retired.as_ref().map(|r| r.session_id.as_str()), Some("s1"));
        assert_eq!(adv_f1.claim.as_ref().unwrap().state, "advanced");

        let stale = advance_with_claim(&pool, "p1", "web", &c_f2.claim.id, None, Fields::default()).await.unwrap_err();
        assert!(matches!(stale, LineageError::ClaimClosed(ref c) if c.state == "abandoned"), "{stale:?}");
        assert_eq!(checkpoint(&pool, "p1", "web").await.unwrap().unwrap().session_id, "w1");
        assert!(by_session(&mut *pool.acquire().await.unwrap(), "p1", "w2").await.unwrap().is_none());

        let ab = abandon(&pool, "p1", &c_f2.claim.id).await.unwrap();
        assert_eq!(ab.state, "abandoned");
        assert_eq!(abandon(&pool, "p1", &c_f2.claim.id).await.unwrap().state, "abandoned");
        assert!(matches!(advance_with_claim(&pool, "p1", "web", &c_f2.claim.id, None, Fields::default()).await, Err(LineageError::ClaimClosed(_))));
    }

    #[tokio::test]
    async fn set_claim_work_session_and_inferred_line_advance() {
        let pool = pool().await;
        advance(&pool, "p1", "web", "s1", Fields::default()).await.unwrap();
        let c_res = claim(&pool, "p1", "web", "resume", None, Some("bak"), None).await.unwrap();
        assert!(matches!(set_claim_work_session(&pool, "p1", &c_res.claim.id, "w0").await, Err(LineageError::ClaimNotFork)));
        abandon(&pool, "p1", &c_res.claim.id).await.unwrap();

        let c_fork = claim(&pool, "p1", "web", "fork", None, None, None).await.unwrap();
        let updated = set_claim_work_session(&pool, "p1", &c_fork.claim.id, "w-new").await.unwrap();
        assert_eq!(updated.work_session_id.as_deref(), Some("w-new"));

        let adv = advance_with_claim(&pool, "p1", "", &c_fork.claim.id, None, Fields { head_sha: Some("1234567".into()), ..Default::default() }).await.unwrap();
        assert_eq!(adv.session.session_id, "w-new");
        assert_eq!(adv.session.line, "web");
        assert!(matches!(set_claim_work_session(&pool, "p1", &c_fork.claim.id, "w-late").await, Err(LineageError::ClaimClosed(_))));
    }

    #[tokio::test]
    async fn stale_open_fork_claims_are_abandoned_when_checkpoint_advances() {
        let pool = pool().await;
        advance(&pool, "p1", "web", "s1", Fields::default()).await.unwrap();
        let f1 = claim(&pool, "p1", "web", "fork", None, None, Some("w1")).await.unwrap().claim;
        let f2 = claim(&pool, "p1", "web", "fork", None, None, Some("w2")).await.unwrap().claim;
        advance_with_claim(&pool, "p1", "web", &f1.id, None, Fields::default()).await.unwrap();
        let stale: LineageClaim = sqlx::query_as("SELECT * FROM lineage_claims WHERE id = ?").bind(&f2.id).fetch_one(&pool).await.unwrap();
        assert_eq!(stale.state, "abandoned", "舊檢查點上 open 的 fork claim 應隨前移自動標為 abandoned");
        assert!(matches!(
            advance_with_claim(&pool, "p1", "web", &f2.id, None, Fields::default()).await,
            Err(LineageError::ClaimClosed(ref c)) if c.state == "abandoned"
        ));
    }

    #[tokio::test]
    async fn record_rollout_tokens_subtracts_ancestors_is_idempotent_and_flags_unrecorded_parent() {
        let pool = pool().await;
        // 1. 根檢查點 s0（無父）
        advance(&pool, "p1", "web", "s0", Fields::default()).await.unwrap();
        assert!(record_rollout_tokens(&pool, "p1", "s0", 100_000, 80_000, 2_000, Some(50_000)).await.unwrap());
        let r0 = by_session(&mut *pool.acquire().await.unwrap(), "p1", "s0").await.unwrap().unwrap();
        assert_eq!((r0.input_tokens, r0.cached_input_tokens, r0.output_tokens, r0.tokens_include_parent), (100_000, 80_000, 2_000, false));
        assert_eq!(r0.context_tokens, Some(50_000));

        // 2. 第一層 fork s1（父 s0）：扣掉 s0、只記自己用量
        create(&pool, "p1", "web", "s1", Fields { parent_session_id: Some("s0".into()), ..Default::default() }).await.unwrap();
        assert!(record_rollout_tokens(&pool, "p1", "s1", 120_000, 95_000, 2_500, Some(60_000)).await.unwrap());
        let r1 = by_session(&mut *pool.acquire().await.unwrap(), "p1", "s1").await.unwrap().unwrap();
        assert_eq!((r1.input_tokens, r1.cached_input_tokens, r1.output_tokens, r1.tokens_include_parent), (20_000, 15_000, 500, false));

        // 冪等覆寫：再讀同一筆 total 不會累加
        assert!(record_rollout_tokens(&pool, "p1", "s1", 120_000, 95_000, 2_500, Some(60_000)).await.unwrap());
        let r1_again = by_session(&mut *pool.acquire().await.unwrap(), "p1", "s1").await.unwrap().unwrap();
        assert_eq!((r1_again.input_tokens, r1_again.cached_input_tokens, r1_again.output_tokens), (20_000, 15_000, 500));

        // 3. 第二層 fork s2（父 s1）：扣掉 s1 + s0 = 120k，不重複扣也不漏扣
        create(&pool, "p1", "web", "s2", Fields { parent_session_id: Some("s1".into()), ..Default::default() }).await.unwrap();
        assert!(record_rollout_tokens(&pool, "p1", "s2", 135_000, 108_000, 2_800, Some(65_000)).await.unwrap());
        let r2 = by_session(&mut *pool.acquire().await.unwrap(), "p1", "s2").await.unwrap().unwrap();
        assert_eq!((r2.input_tokens, r2.cached_input_tokens, r2.output_tokens, r2.tokens_include_parent), (15_000, 13_000, 300, false));

        // 4. 父未記錄時照原值寫入並標記 tokens_include_parent=true；stats 不計入 saved_input_tokens
        create(&pool, "p1", "quota", "orphan", Fields { parent_session_id: Some("missing-parent".into()), ..Default::default() }).await.unwrap();
        assert!(record_rollout_tokens(&pool, "p1", "orphan", 50_000, 40_000, 1_000, None).await.unwrap());
        let ro = by_session(&mut *pool.acquire().await.unwrap(), "p1", "orphan").await.unwrap().unwrap();
        assert_eq!((ro.input_tokens, ro.cached_input_tokens, ro.output_tokens, ro.tokens_include_parent), (50_000, 40_000, 1_000, true));

        let st = stats(&pool, "p1").await.unwrap();
        let web_st = st.iter().find(|s| s.line == "web").unwrap();
        assert_eq!((web_st.tokens.input_tokens, web_st.tokens.cached_input_tokens, web_st.tokens.saved_input_tokens, web_st.tokens.includes_parent), (135_000, 108_000, 108_000, false));
        let quota_st = st.iter().find(|s| s.line == "quota").unwrap();
        assert_eq!((quota_st.tokens.input_tokens, quota_st.tokens.cached_input_tokens, quota_st.tokens.saved_input_tokens, quota_st.tokens.includes_parent), (50_000, 40_000, 0, true));

        // 5. 未登記在 lineage_sessions 的 session 回 false
        assert!(!record_rollout_tokens(&pool, "p1", "unregistered", 10_000, 8_000, 100, None).await.unwrap());
    }
}
