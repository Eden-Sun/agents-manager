//! codex session 血統的 API（issue #1943 子票 A＋B1；帳本在 `db::lineage`）。
//!
//! - `GET  /api/projects/{id}/lineage[?line=&state=]`：列出。
//! - `POST /api/projects/{id}/lineage`：登記一顆進行中（active）的 session。
//! - `POST /api/projects/{id}/lineage/advance`：把某顆 session 設為該線檢查點，舊檢查點改 retired（可選帶 `claim_id` 做單寫者＋版本比對）。
//! - `POST /api/projects/{id}/lineage/retire`：退役。
//! - `POST /api/projects/{id}/lineage/claim`：認領一條線（`resume` 單寫者獨佔；`fork` 可並存）。
//! - `POST /api/projects/{id}/lineage/claim/{claim_id}/abandon`：放棄認領。
//!
//! 權限：讀＝User 或同專案 Bot（含 child）；寫＝User、同專案的 AGM 角色，expert 專案另含頂層 manager（含代班中的副管理者），
//! 母子模式（parent_child）專案另含同專案任何頂層 bot（child 一律唯讀）。
//! 跨專案的 Bot 在 `authorize_bot_path` 就被擋下（403 `bot_resource_scope`），這裡再核一次專案，不靠路由層。

use std::sync::Arc;
use axum::{extract::{Path, Query, State}, Extension, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use crate::{api::RequestPrincipal, db::{self, lineage::{self, Fields, LineageError}}, lifecycle::LcError, state::App};

const MAX_ID_LEN: usize = 200;
const MAX_LINE_LEN: usize = 64;
const MAX_SUMMARY_CHARS: usize = 500;

#[derive(Debug, Deserialize, Default)]
pub(crate) struct ListQuery {
    pub line: Option<String>,
    pub state: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionBody {
    #[serde(default)]
    pub line: String,
    pub session_id: Option<String>,
    pub claim_id: Option<String>,
    pub role: Option<String>,
    pub kind: Option<String>,
    pub parent_session_id: Option<String>,
    pub context_tokens: Option<i64>,
    pub last_api_at: Option<String>,
    pub head_sha: Option<String>,
    pub summary: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClaimBody {
    pub line: String,
    pub mode: String,
    pub backup_session_id: Option<String>,
    pub work_session_id: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClaimWorkSessionBody {
    pub work_session_id: String,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct RetireBody {
    pub session_id: String,
}

fn up(e: impl std::fmt::Display) -> LcError {
    LcError::Upstream(e.to_string())
}

fn bad(field: &str, message: &str) -> LcError {
    LcError::BadValue(json!({"error": "bad_request", "reason": "invalid_lineage_field", "field": field, "message": message}))
}

fn forbidden(reason: &str) -> LcError {
    LcError::Forbidden(json!({"error": "forbidden", "reason": reason}))
}

/// 模組線名：1–64 個 `[A-Za-z0-9._-]`，開頭是英數（會進指令列與之後的 `docs/notes/<線>.md`）。
pub(crate) fn valid_line(line: &str) -> bool {
    !line.is_empty()
        && line.len() <= MAX_LINE_LEN
        && line.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && line.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// CLI 的 session id：不解析格式（各 kind 不同），只擋空白、控制字元與過長。
pub(crate) fn valid_session_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_ID_LEN && id.chars().all(|c| c.is_ascii_graphic())
}

pub(crate) fn valid_head_sha(sha: &str) -> bool {
    (7..=64).contains(&sha.len()) && sha.chars().all(|c| c.is_ascii_hexdigit())
}

/// 一行摘要：換行與連續空白收成一個空白；空的當沒給；超過 500 字拒絕（不默默截斷）。
pub(crate) fn clean_summary(summary: &str) -> Result<Option<String>, ()> {
    let one_line = summary.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() > MAX_SUMMARY_CHARS {
        return Err(());
    }
    Ok((!one_line.is_empty()).then_some(one_line))
}

fn opt(value: Option<String>) -> Option<String> {
    value.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn fields(b: SessionBody, allow_empty_line: bool) -> Result<(String, Option<String>, Option<String>, Fields), LcError> {
    let line = b.line.trim().to_string();
    if !(allow_empty_line && line.is_empty()) && !valid_line(&line) {
        return Err(bad("line", "1–64 chars of [A-Za-z0-9._-], starting with a letter or digit"));
    }
    let session_id = opt(b.session_id);
    if session_id.as_deref().is_some_and(|s| !valid_session_id(s)) {
        return Err(bad("session_id", "1–200 printable ASCII chars without spaces"));
    }
    let claim_id = opt(b.claim_id);
    if claim_id.as_deref().is_some_and(|c| !valid_session_id(c)) {
        return Err(bad("claim_id", "1–200 printable ASCII chars without spaces"));
    }
    let role = opt(b.role);
    if role.as_deref().is_some_and(|r| !lineage::ROLES.contains(&r)) {
        return Err(bad("role", "worker or thinker"));
    }
    let kind = opt(b.kind);
    if kind.as_deref().is_some_and(|k| !lineage::KINDS.contains(&k)) {
        return Err(bad("kind", "only codex sessions are tracked"));
    }
    let parent_session_id = opt(b.parent_session_id);
    if parent_session_id.as_deref().is_some_and(|p| !valid_session_id(p)) {
        return Err(bad("parent_session_id", "1–200 printable ASCII chars without spaces"));
    }
    if parent_session_id.is_some() && parent_session_id == session_id {
        return Err(bad("parent_session_id", "a session is not its own parent"));
    }
    if b.context_tokens.is_some_and(|n| n < 0) {
        return Err(bad("context_tokens", "must not be negative"));
    }
    let last_api_at = opt(b.last_api_at);
    if last_api_at.as_deref().is_some_and(|t| chrono::DateTime::parse_from_rfc3339(t).is_err()) {
        return Err(bad("last_api_at", "RFC 3339 timestamp"));
    }
    let head_sha = opt(b.head_sha).map(|s| s.to_ascii_lowercase());
    if head_sha.as_deref().is_some_and(|s| !valid_head_sha(s)) {
        return Err(bad("head_sha", "7–64 hex chars"));
    }
    let summary = match b.summary.as_deref().map(clean_summary) {
        None => None,
        Some(Ok(s)) => s,
        Some(Err(())) => return Err(bad("summary", "one line, at most 500 chars")),
    };
    Ok((line, session_id, claim_id, Fields { role, kind, parent_session_id, context_tokens: b.context_tokens, last_api_at, head_sha, summary }))
}

async fn live_project(app: &Arc<App>, project_id: &str) -> Result<db::Project, LcError> {
    db::project(&app.db, project_id).await.map_err(up)?.filter(|p| p.deleted_at.is_none()).ok_or_else(|| LcError::NotFound("project".into()))
}

/// 呼叫端是 Bot 時回它的列（已確認存活且屬於這個專案）；User 回 `None`；其他 principal 一律拒絕。
async fn caller_in_project(app: &Arc<App>, principal: &RequestPrincipal, project_id: &str) -> Result<Option<db::Bot>, LcError> {
    let id = match principal {
        RequestPrincipal::User => return Ok(None),
        RequestPrincipal::Bot(id) | RequestPrincipal::Child { child_id: id, .. } => id,
        _ => return Err(forbidden("user_or_project_bot")),
    };
    let bot = db::bot(&app.db, id).await.map_err(up)?.filter(|b| b.deleted_at.is_none()).ok_or_else(|| forbidden("unknown_bot"))?;
    if bot.project_id != project_id {
        return Err(forbidden("cross_project"));
    }
    Ok(Some(bot))
}

/// 寫入：User、同專案的 AGM 角色；expert 專案的頂層 manager（或代班中的副管理者）；母子模式專案的頂層 bot。
/// expert 專案的執行者、顧問、專家，以及任何模式的 child 只能讀。
async fn require_writer(app: &Arc<App>, principal: &RequestPrincipal, project_id: &str) -> Result<Option<db::Bot>, LcError> {
    let Some(bot) = caller_in_project(app, principal, project_id).await? else { return Ok(None) };
    let denied = || LcError::Forbidden(json!({"error": "forbidden", "reason": "role_required", "message": "session 血統只有管理者（母子模式為頂層 bot）、AGM 角色或使用者能改；其他成員與 child 只能讀"}));
    if !matches!(principal, RequestPrincipal::Bot(_)) || bot.parent_bot_id.is_some() {
        return Err(denied());
    }
    // 母子模式（parent_child）沒有 manager：派工的是頂層 bot（parent），做完由它決定要不要前移，所以同專案頂層 bot 都可寫。
    // expert 專案只認 manager（同 `api::expert_manager_over`）、代班中的副管理者與 AGM 角色。
    let expert = live_project(app, project_id).await?.mode == "expert";
    if !expert && bot.managed_by != "child" {
        return Ok(Some(bot));
    }
    if expert && bot.role.as_deref() == Some("manager") {
        return Ok(Some(bot));
    }
    if crate::supervisor::roles::role_of_bot(&app.db, &bot.id).await.map_err(up)?.is_some() {
        return Ok(Some(bot));
    }
    if matches!(bot.role.as_deref(), Some("advisor" | "specialist")) && crate::runners::relay_deputy::acting(app, &bot.id).await {
        return Ok(Some(bot));
    }
    Err(denied())
}

fn lineage_err(e: LineageError) -> LcError {
    match e {
        LineageError::Exists(row) => LcError::conflict("lineage_session_exists", json!({"session": *row})),
        LineageError::LineMismatch { session_line } => LcError::conflict("lineage_line_mismatch", json!({"session_line": session_line})),
        LineageError::NotFound => LcError::NotFoundValue(json!({"error": "not_found", "reason": "lineage_session_not_found"})),
        LineageError::CheckpointNotFound => LcError::NotFoundValue(json!({"error": "not_found", "reason": "lineage_checkpoint_not_found"})),
        LineageError::LineBusy(claim) => LcError::conflict("lineage_line_busy", json!({"claim": *claim})),
        LineageError::ClaimNotFound => LcError::NotFoundValue(json!({"error": "not_found", "reason": "lineage_claim_not_found"})),
        LineageError::ClaimClosed(claim) => LcError::conflict("lineage_claim_closed", json!({"claim": *claim})),
        LineageError::ClaimLineMismatch { claim_line } => LcError::conflict("lineage_line_mismatch", json!({"claim_line": claim_line})),
        LineageError::ClaimMissingWorkSession => bad("work_session_id", "fork claim has no work_session_id"),
        LineageError::ClaimNotFork => bad("mode", "only fork claims accept work_session_id"),
        LineageError::ClaimSessionMismatch { expected, got } => {
            LcError::conflict("lineage_claim_session_mismatch", json!({"expected_session_id": expected, "session_id": got}))
        }
        LineageError::CheckpointMoved { checkpoint } => LcError::conflict("lineage_checkpoint_moved", json!({"checkpoint": checkpoint})),
        LineageError::Db(e) => up(e),
    }
}

pub(crate) async fn list(
    State(app): State<Arc<App>>,
    Path(project_id): Path<String>,
    Query(q): Query<ListQuery>,
    Extension(principal): Extension<RequestPrincipal>,
) -> Result<Json<Value>, LcError> {
    caller_in_project(&app, &principal, &project_id).await?;
    live_project(&app, &project_id).await?;
    let line = opt(q.line);
    if line.as_deref().is_some_and(|l| !valid_line(l)) {
        return Err(bad("line", "1–64 chars of [A-Za-z0-9._-], starting with a letter or digit"));
    }
    let state = opt(q.state);
    if state.as_deref().is_some_and(|s| !lineage::STATES.contains(&s)) {
        return Err(bad("state", "checkpoint, active or retired"));
    }
    let sessions = lineage::list(&app.db, &project_id, line.as_deref(), state.as_deref()).await.map_err(up)?;
    Ok(Json(json!({"project_id": project_id, "sessions": sessions})))
}

pub(crate) async fn create(
    State(app): State<Arc<App>>,
    Path(project_id): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
    Json(body): Json<SessionBody>,
) -> Result<Json<Value>, LcError> {
    require_writer(&app, &principal, &project_id).await?;
    live_project(&app, &project_id).await?;
    let (line, session_id, claim_id, f) = fields(body, false)?;
    if claim_id.is_some() {
        return Err(bad("claim_id", "only supported on advance"));
    }
    let session_id = session_id.ok_or_else(|| bad("session_id", "1–200 printable ASCII chars without spaces"))?;
    let session = lineage::create(&app.db, &project_id, &line, &session_id, f).await.map_err(lineage_err)?;
    Ok(Json(json!({"session": session})))
}

pub(crate) async fn advance(
    State(app): State<Arc<App>>,
    Path(project_id): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
    Json(body): Json<SessionBody>,
) -> Result<Json<Value>, LcError> {
    require_writer(&app, &principal, &project_id).await?;
    live_project(&app, &project_id).await?;
    let has_claim = body.claim_id.as_deref().is_some_and(|c| !c.trim().is_empty());
    let (line, session_id, claim_id, f) = fields(body, has_claim)?;
    let done = match claim_id {
        Some(cid) => lineage::advance_with_claim(&app.db, &project_id, &line, &cid, session_id.as_deref(), f).await.map_err(lineage_err)?,
        None => {
            let sid = session_id.ok_or_else(|| bad("session_id", "1–200 printable ASCII chars without spaces"))?;
            lineage::advance(&app.db, &project_id, &line, &sid, f).await.map_err(lineage_err)?
        }
    };
    Ok(Json(serde_json::to_value(done).map_err(up)?))
}

pub(crate) async fn claim(
    State(app): State<Arc<App>>,
    Path(project_id): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
    Json(body): Json<ClaimBody>,
) -> Result<Json<Value>, LcError> {
    let holder = require_writer(&app, &principal, &project_id).await?;
    live_project(&app, &project_id).await?;
    let line = body.line.trim().to_string();
    if !valid_line(&line) {
        return Err(bad("line", "1–64 chars of [A-Za-z0-9._-], starting with a letter or digit"));
    }
    let mode = body.mode.trim().to_string();
    if !lineage::CLAIM_MODES.contains(&mode.as_str()) {
        return Err(bad("mode", "resume or fork"));
    }
    let backup_session_id = opt(body.backup_session_id);
    if backup_session_id.as_deref().is_some_and(|s| !valid_session_id(s)) {
        return Err(bad("backup_session_id", "1–200 printable ASCII chars without spaces"));
    }
    let work_session_id = opt(body.work_session_id);
    if work_session_id.as_deref().is_some_and(|s| !valid_session_id(s)) {
        return Err(bad("work_session_id", "1–200 printable ASCII chars without spaces"));
    }
    let claimed = lineage::claim(
        &app.db,
        &project_id,
        &line,
        &mode,
        holder.as_ref().map(|b| b.id.as_str()),
        backup_session_id.as_deref(),
        work_session_id.as_deref(),
    )
    .await
    .map_err(lineage_err)?;
    Ok(Json(json!({"claim": claimed.claim, "checkpoint": claimed.checkpoint})))
}

pub(crate) async fn set_claim_work_session(
    State(app): State<Arc<App>>,
    Path((project_id, claim_id)): Path<(String, String)>,
    Extension(principal): Extension<RequestPrincipal>,
    Json(body): Json<ClaimWorkSessionBody>,
) -> Result<Json<Value>, LcError> {
    require_writer(&app, &principal, &project_id).await?;
    live_project(&app, &project_id).await?;
    let claim_id = claim_id.trim();
    if !valid_session_id(claim_id) {
        return Err(bad("claim_id", "1–200 printable ASCII chars without spaces"));
    }
    let work_session_id = body.work_session_id.trim();
    if !valid_session_id(work_session_id) {
        return Err(bad("work_session_id", "1–200 printable ASCII chars without spaces"));
    }
    let claim = lineage::set_claim_work_session(&app.db, &project_id, claim_id, work_session_id).await.map_err(lineage_err)?;
    Ok(Json(json!({"claim": claim})))
}

pub(crate) async fn abandon(
    State(app): State<Arc<App>>,
    Path((project_id, claim_id)): Path<(String, String)>,
    Extension(principal): Extension<RequestPrincipal>,
) -> Result<Json<Value>, LcError> {
    require_writer(&app, &principal, &project_id).await?;
    live_project(&app, &project_id).await?;
    let claim_id = claim_id.trim();
    if !valid_session_id(claim_id) {
        return Err(bad("claim_id", "1–200 printable ASCII chars without spaces"));
    }
    let claim = lineage::abandon(&app.db, &project_id, claim_id).await.map_err(lineage_err)?;
    let backup_session_id = (claim.mode == "resume").then(|| claim.backup_session_id.clone()).flatten();
    Ok(Json(json!({"claim": claim, "backup_session_id": backup_session_id})))
}

pub(crate) async fn retire(
    State(app): State<Arc<App>>,
    Path(project_id): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
    Json(body): Json<RetireBody>,
) -> Result<Json<Value>, LcError> {
    require_writer(&app, &principal, &project_id).await?;
    live_project(&app, &project_id).await?;
    let session_id = body.session_id.trim().to_string();
    if !valid_session_id(&session_id) {
        return Err(bad("session_id", "1–200 printable ASCII chars without spaces"));
    }
    let session = lineage::retire(&app.db, &project_id, &session_id).await.map_err(lineage_err)?;
    Ok(Json(json!({"session": session})))
}

#[cfg(all(test, not(feature = "daemon-test-harness")))]
mod parse_tests {
    use super::*;

    #[test]
    fn line_session_and_sha_shapes() {
        for ok in ["web", "ci-ops", "lifecycle.v2", "a_b", "9"] {
            assert!(valid_line(ok), "{ok}");
        }
        for no in ["", "-web", ".web", "ci/ops", "we b", "線", &"x".repeat(65)] {
            assert!(!valid_line(no), "{no}");
        }
        assert!(valid_session_id("01a16b82-ea4f-7062-ada9-d3c819e0e738"));
        for no in ["", "a b", "a\nb", "é", &"x".repeat(201)] {
            assert!(!valid_session_id(no), "{no:?}");
        }
        assert!(valid_head_sha("0b43a28") && valid_head_sha(&"a".repeat(64)));
        for no in ["0b43a2", "0b43a2g", &"a".repeat(65)] {
            assert!(!valid_head_sha(no), "{no}");
        }
    }

    #[test]
    fn summary_is_one_line_and_bounded() {
        assert_eq!(clean_summary("  merged\n the  quota fix ").unwrap().as_deref(), Some("merged the quota fix"));
        assert_eq!(clean_summary(" \n ").unwrap(), None);
        assert!(clean_summary(&"字".repeat(500)).is_ok());
        assert!(clean_summary(&"字".repeat(501)).is_err());
    }
}
