//! Daemon-owned fast/full CI queues (issue #716).
//!
//! Jobs are persisted independently of bot panes. The worker is deliberately opt-in: the daemon
//! starts it only when a dedicated repository checkout and work root are configured, so an
//! installation can move scheduling off the legacy host timer without racing that timer first.

use crate::api::RequestPrincipal;
use crate::db;
use crate::lifecycle::LcError;
use crate::state::App;
use anyhow::{Context, Result};
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{FromRow, SqlitePool};
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::Command;

const FAST_TIMEOUT_SECS: i64 = 5 * 60;
const FULL_TIMEOUT_SECS: i64 = 45 * 60;
const MAX_OUTPUT_BYTES: usize = 32 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Queue {
    Fast,
    Full,
}

impl Queue {
    fn as_str(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Full => "full",
        }
    }

    fn timeout_secs(self) -> i64 {
        match self {
            Self::Fast => FAST_TIMEOUT_SECS,
            Self::Full => FULL_TIMEOUT_SECS,
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "fast" => Some(Self::Fast),
            "full" => Some(Self::Full),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Route {
    project_id: String,
    requester: String,
}

#[derive(Clone, Debug, FromRow)]
struct StoredJob {
    id: String,
    queue: String,
    sha: String,
    status: String,
    request_routes_json: String,
    timeout_seconds: i64,
    failed_steps_json: String,
    output_tail: String,
    exit_code: Option<i64>,
    created_at: String,
    updated_at: String,
    started_at: Option<String>,
    finished_at: Option<String>,
}

#[derive(Clone, Debug)]
struct CiJob {
    id: String,
    queue: String,
    sha: String,
    status: String,
    routes: Vec<Route>,
    timeout_seconds: i64,
    failed_steps: Vec<String>,
    output_tail: String,
    exit_code: Option<i64>,
    created_at: String,
    updated_at: String,
    started_at: Option<String>,
    finished_at: Option<String>,
}

impl TryFrom<StoredJob> for CiJob {
    type Error = serde_json::Error;

    fn try_from(row: StoredJob) -> std::result::Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            queue: row.queue,
            sha: row.sha,
            status: row.status,
            routes: serde_json::from_str(&row.request_routes_json)?,
            timeout_seconds: row.timeout_seconds,
            failed_steps: serde_json::from_str(&row.failed_steps_json)?,
            output_tail: row.output_tail,
            exit_code: row.exit_code,
            created_at: row.created_at,
            updated_at: row.updated_at,
            started_at: row.started_at,
            finished_at: row.finished_at,
        })
    }
}

impl CiJob {
    fn json(&self) -> Value {
        json!({
            "id": self.id,
            "queue": self.queue,
            "sha": self.sha,
            "status": self.status,
            "routes": self.routes,
            "timeout_seconds": self.timeout_seconds,
            "failed_steps": self.failed_steps,
            "output_tail": self.output_tail,
            "exit_code": self.exit_code,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "started_at": self.started_at,
            "finished_at": self.finished_at,
        })
    }
}

#[derive(Clone, Debug)]
struct JobResult {
    status: &'static str,
    exit_code: Option<i64>,
    failed_steps: Vec<String>,
    output_tail: String,
}

impl JobResult {
    fn success(exit_code: i64, output: &str) -> Self {
        Self {
            status: "success",
            exit_code: Some(exit_code),
            failed_steps: vec![],
            output_tail: output.into(),
        }
    }

    fn failure(exit_code: i64, step: &str, output: &str) -> Self {
        Self {
            status: "failure",
            exit_code: Some(exit_code),
            failed_steps: vec![step.into()],
            output_tail: output.into(),
        }
    }

    fn timed_out(step: &str, output: &str) -> Self {
        Self {
            status: "timed_out",
            exit_code: None,
            failed_steps: vec![step.into()],
            output_tail: output.into(),
        }
    }
}

pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS ci_jobs (
           id TEXT PRIMARY KEY,
           queue TEXT NOT NULL CHECK (queue IN ('fast','full')),
           sha TEXT NOT NULL,
           status TEXT NOT NULL CHECK (status IN ('queued','running','success','failure','timed_out')),
           request_routes_json TEXT NOT NULL DEFAULT '[]',
           timeout_seconds INTEGER NOT NULL,
           failed_steps_json TEXT NOT NULL DEFAULT '[]',
           output_tail TEXT NOT NULL DEFAULT '',
           exit_code INTEGER,
           created_at TEXT NOT NULL,
           updated_at TEXT NOT NULL,
           started_at TEXT,
           finished_at TEXT
         )",
    )
    .execute(pool)
    .await
    .context("create ci_jobs")?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS ci_jobs_queue_state ON ci_jobs(queue, status, created_at)",
    )
    .execute(pool)
    .await
    .context("index ci_jobs_queue_state")?;
    sqlx::query("CREATE INDEX IF NOT EXISTS ci_jobs_sha_state ON ci_jobs(queue, sha, status)")
        .execute(pool)
        .await
        .context("index ci_jobs_sha_state")?;
    Ok(())
}

async fn stored_job(pool: &SqlitePool, id: &str) -> Result<Option<CiJob>> {
    let row = sqlx::query_as::<_, StoredJob>("SELECT * FROM ci_jobs WHERE id=?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    row.map(CiJob::try_from)
        .transpose()
        .context("decode CI job")
}

async fn emit_job(app: &Arc<App>, job: &CiJob) {
    for route in &job.routes {
        app.emit(
            "ci_job_updated",
            json!({
                "job_id": job.id,
                "project_id": route.project_id,
                "requester": route.requester,
                "queue": job.queue,
                "sha": job.sha,
                "status": job.status,
            }),
        )
        .await;
    }
}

fn add_route(routes: &mut Vec<Route>, route: Route) {
    if !routes.contains(&route) {
        routes.push(route);
    }
}

/// Persist a request, coalescing duplicate fast SHAs and keeping only the latest pending full SHA.
async fn submit(
    app: &Arc<App>,
    queue: Queue,
    sha: &str,
    project_id: &str,
    requester: &str,
) -> Result<CiJob> {
    let _guard = app.ci_queue_lock.lock().await;
    expire_due_unlocked(app, Some(queue)).await?;
    let route = Route {
        project_id: project_id.into(),
        requester: requester.into(),
    };

    let existing_id = if queue == Queue::Fast {
        sqlx::query_scalar::<_, String>(
            "SELECT id FROM ci_jobs WHERE queue='fast' AND sha=? AND status IN ('queued','running')
             ORDER BY CASE status WHEN 'running' THEN 0 ELSE 1 END, created_at LIMIT 1",
        )
        .bind(sha)
        .fetch_optional(&app.db)
        .await?
    } else {
        let pending = sqlx::query_scalar::<_, String>(
            "SELECT id FROM ci_jobs WHERE queue='full' AND status='queued' ORDER BY created_at DESC, id DESC LIMIT 1",
        )
        .fetch_optional(&app.db)
        .await?;
        match pending {
            Some(id) => Some(id),
            None => {
                let running = sqlx::query_as::<_, (String, String)>(
                    "SELECT id, sha FROM ci_jobs WHERE queue='full' AND status='running' ORDER BY started_at LIMIT 1",
                )
                .fetch_optional(&app.db)
                .await?;
                running.and_then(|(id, running_sha)| (running_sha == sha).then_some(id))
            }
        }
    };

    if let Some(id) = existing_id {
        let mut job = stored_job(&app.db, &id)
            .await?
            .context("coalesced CI job disappeared")?;
        add_route(&mut job.routes, route);
        let updated = db::now();
        if queue == Queue::Full && job.status == "queued" {
            job.sha = sha.into();
        }
        sqlx::query("UPDATE ci_jobs SET sha=?, request_routes_json=?, updated_at=? WHERE id=? AND status IN ('queued','running')")
            .bind(&job.sha)
            .bind(serde_json::to_string(&job.routes)?)
            .bind(updated)
            .bind(&id)
            .execute(&app.db)
            .await?;
        let job = stored_job(&app.db, &id)
            .await?
            .context("coalesced CI job disappeared after update")?;
        emit_job(app, &job).await;
        return Ok(job);
    }

    let id = db::ulid();
    let now = db::now();
    let routes = serde_json::to_string(&[route])?;
    sqlx::query(
        "INSERT INTO ci_jobs (id,queue,sha,status,request_routes_json,timeout_seconds,created_at,updated_at)
         VALUES (?,?,?,'queued',?,?,?,?)",
    )
    .bind(&id)
    .bind(queue.as_str())
    .bind(sha)
    .bind(routes)
    .bind(queue.timeout_secs())
    .bind(&now)
    .bind(&now)
    .execute(&app.db)
    .await?;
    let job = stored_job(&app.db, &id)
        .await?
        .context("new CI job not found after insert")?;
    emit_job(app, &job).await;
    Ok(job)
}

async fn expire_due_unlocked(app: &Arc<App>, queue: Option<Queue>) -> Result<()> {
    let rows = sqlx::query_as::<_, (String,)>(
        "SELECT id FROM ci_jobs WHERE status='running'
           AND (? IS NULL OR queue=?)
           AND started_at IS NOT NULL
           AND datetime(started_at, '+' || timeout_seconds || ' seconds') <= datetime('now')",
    )
    .bind(queue.map(Queue::as_str))
    .bind(queue.map(Queue::as_str))
    .fetch_all(&app.db)
    .await?;
    for (id,) in rows {
        let now = db::now();
        sqlx::query(
            "UPDATE ci_jobs SET status='timed_out', failed_steps_json='[\"timeout\"]', updated_at=?, finished_at=?
             WHERE id=? AND status='running'",
        )
        .bind(&now)
        .bind(&now)
        .bind(&id)
        .execute(&app.db)
        .await?;
        if let Some(job) = stored_job(&app.db, &id).await? {
            emit_job(app, &job).await;
        }
    }
    Ok(())
}

async fn claim_next(app: &Arc<App>, queue: Queue) -> Result<Option<CiJob>> {
    let _guard = app.ci_queue_lock.lock().await;
    expire_due_unlocked(app, Some(queue)).await?;
    let running: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM ci_jobs WHERE queue=? AND status='running'")
            .bind(queue.as_str())
            .fetch_one(&app.db)
            .await?;
    if running > 0 {
        return Ok(None);
    }
    let id = sqlx::query_scalar::<_, String>(
        "SELECT id FROM ci_jobs WHERE queue=? AND status='queued' ORDER BY created_at, id LIMIT 1",
    )
    .bind(queue.as_str())
    .fetch_optional(&app.db)
    .await?;
    let Some(id) = id else { return Ok(None) };
    let now = db::now();
    sqlx::query("UPDATE ci_jobs SET status='running', started_at=?, updated_at=? WHERE id=? AND status='queued'")
        .bind(&now)
        .bind(&now)
        .bind(&id)
        .execute(&app.db)
        .await?;
    let job = stored_job(&app.db, &id)
        .await?
        .context("claimed CI job disappeared")?;
    emit_job(app, &job).await;
    Ok(Some(job))
}

async fn finish(app: &Arc<App>, id: &str, result: JobResult) -> Result<bool> {
    let _guard = app.ci_queue_lock.lock().await;
    let now = db::now();
    let changed = sqlx::query(
        "UPDATE ci_jobs SET status=?, exit_code=?, failed_steps_json=?, output_tail=?, updated_at=?, finished_at=?
         WHERE id=? AND status='running'",
    )
    .bind(result.status)
    .bind(result.exit_code)
    .bind(serde_json::to_string(&result.failed_steps)?)
    .bind(trim_tail(&result.output_tail, MAX_OUTPUT_BYTES))
    .bind(&now)
    .bind(&now)
    .bind(id)
    .execute(&app.db)
    .await?
    .rows_affected()
        > 0;
    if changed {
        if let Some(job) = stored_job(&app.db, id).await? {
            emit_job(app, &job).await;
        }
    }
    Ok(changed)
}

fn trim_tail(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut start = value.len() - max_bytes;
    while !value.is_char_boundary(start) {
        start += 1;
    }
    value[start..].to_owned()
}

async fn get_job(pool: &SqlitePool, id: &str) -> Result<Option<CiJob>> {
    stored_job(pool, id).await
}

async fn list_jobs(
    app: &Arc<App>,
    project_id: Option<&str>,
    requester: Option<&str>,
    limit: i64,
) -> Result<Vec<CiJob>> {
    {
        let _guard = app.ci_queue_lock.lock().await;
        expire_due_unlocked(app, None).await?;
    }
    let rows = sqlx::query_as::<_, StoredJob>(
        "SELECT * FROM ci_jobs ORDER BY updated_at DESC, id DESC LIMIT ?",
    )
    .bind(limit.clamp(1, 100))
    .fetch_all(&app.db)
    .await?;
    let jobs = rows
        .into_iter()
        .map(CiJob::try_from)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(jobs
        .into_iter()
        .filter(|job| {
            job.routes.iter().any(|route| {
                project_id.is_none_or(|project| route.project_id == project)
                    && requester.is_none_or(|who| route.requester == who)
            })
        })
        .collect())
}

#[derive(Debug, Deserialize)]
pub struct EnqueueIn {
    queue: String,
    sha: String,
    project_id: String,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    project_id: Option<String>,
}

fn requester(principal: &RequestPrincipal) -> Option<String> {
    match principal {
        RequestPrincipal::User => Some("user".into()),
        RequestPrincipal::Bot(id) => Some(format!("bot:{id}")),
        RequestPrincipal::Service(_) => None,
    }
}

async fn authorize_project(
    app: &Arc<App>,
    project_id: &str,
    principal: &RequestPrincipal,
) -> Result<(), LcError> {
    let exists: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM projects WHERE id=? AND deleted_at IS NULL")
            .bind(project_id)
            .fetch_optional(&app.db)
            .await
            .map_err(|e| LcError::Upstream(e.to_string()))?;
    if exists.is_none() {
        return Err(LcError::NotFound("project".into()));
    }
    if let RequestPrincipal::Bot(id) = principal {
        let owner: Option<String> =
            sqlx::query_scalar("SELECT project_id FROM bots WHERE id=? AND deleted_at IS NULL")
                .bind(id)
                .fetch_optional(&app.db)
                .await
                .map_err(|e| LcError::Upstream(e.to_string()))?;
        if owner.as_deref() != Some(project_id) {
            return Err(LcError::Forbidden(json!({"error":"project_forbidden"})));
        }
    }
    Ok(())
}

pub async fn post_job(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<RequestPrincipal>,
    Json(input): Json<EnqueueIn>,
) -> Result<(StatusCode, Json<Value>), LcError> {
    let queue = Queue::parse(&input.queue)
        .ok_or_else(|| LcError::Bad("queue must be fast or full".into()))?;
    if !(40..=64).contains(&input.sha.len()) || !input.sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(LcError::Bad(
            "sha must be a full hexadecimal Git object id".into(),
        ));
    }
    let requester = requester(&principal)
        .ok_or_else(|| LcError::Forbidden(json!({"error":"ci_job_forbidden"})))?;
    authorize_project(&app, &input.project_id, &principal).await?;
    let job = submit(
        &app,
        queue,
        &input.sha.to_ascii_lowercase(),
        &input.project_id,
        &requester,
    )
    .await
    .map_err(|e| LcError::Upstream(e.to_string()))?;
    Ok((StatusCode::ACCEPTED, Json(job.json())))
}

pub async fn get_jobs(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<RequestPrincipal>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, LcError> {
    let requester = match requester(&principal) {
        Some(who) if matches!(principal, RequestPrincipal::Bot(_)) => Some(who),
        Some(_) => None,
        None => return Err(LcError::Forbidden(json!({"error":"ci_job_forbidden"}))),
    };
    let jobs = list_jobs(&app, query.project_id.as_deref(), requester.as_deref(), 100)
        .await
        .map_err(|e| LcError::Upstream(e.to_string()))?;
    Ok(Json(
        json!({"jobs": jobs.iter().map(CiJob::json).collect::<Vec<_>>()}),
    ))
}

pub async fn get_job_http(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<RequestPrincipal>,
    Path(id): Path<String>,
) -> Result<Json<Value>, LcError> {
    let job = get_job(&app.db, &id)
        .await
        .map_err(|e| LcError::Upstream(e.to_string()))?
        .ok_or_else(|| LcError::NotFound("CI job".into()))?;
    match requester(&principal) {
        Some(requester) if !job.routes.iter().any(|route| route.requester == requester) => {
            return Err(LcError::Forbidden(json!({"error":"ci_job_forbidden"})));
        }
        None if !matches!(principal, RequestPrincipal::User) => {
            return Err(LcError::Forbidden(json!({"error":"ci_job_forbidden"})));
        }
        _ => {}
    }
    Ok(Json(job.json()))
}

/// Start separate serial workers for fast and full CI. No worker runs on Darwin or without both
/// explicit paths; this keeps the old external timer untouched until a deployment switches over.
pub fn spawn_worker(app: Arc<App>) {
    if !cfg!(target_os = "linux") {
        tracing::info!("daemon CI worker disabled outside Linux; Darwin-specific checks remain on the Mac worker");
        return;
    }
    let Some(repo) = std::env::var_os("AGM_CI_REPO_DIR").map(PathBuf::from) else {
        tracing::info!("daemon CI worker disabled: AGM_CI_REPO_DIR is not configured");
        return;
    };
    let Some(work_root) = std::env::var_os("AGM_CI_WORK_ROOT").map(PathBuf::from) else {
        tracing::info!("daemon CI worker disabled: AGM_CI_WORK_ROOT is not configured");
        return;
    };
    for queue in [Queue::Fast, Queue::Full] {
        let app = app.clone();
        let repo = repo.clone();
        let work_root = work_root.clone();
        tokio::spawn(async move { worker_loop(app, queue, repo, work_root).await });
    }
}

async fn worker_loop(app: Arc<App>, queue: Queue, repo: PathBuf, work_root: PathBuf) {
    loop {
        match claim_next(&app, queue).await {
            Ok(Some(job)) => {
                let result = execute_job(&repo, &work_root, &job).await;
                if let Err(error) = finish(&app, &job.id, result).await {
                    tracing::error!(job_id = %job.id, error = %error, "could not store CI result");
                }
            }
            Ok(None) => tokio::time::sleep(Duration::from_secs(1)).await,
            Err(error) => {
                tracing::error!(queue = queue.as_str(), error = %error, "CI queue poll failed");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

async fn execute_job(repo: &FsPath, work_root: &FsPath, job: &CiJob) -> JobResult {
    let deadline = Instant::now() + Duration::from_secs(job.timeout_seconds.max(1) as u64);
    let worktree_root = work_root.join("worktrees");
    let worktree = worktree_root.join(&job.id);
    if let Err(error) = tokio::fs::create_dir_all(&worktree_root).await {
        return JobResult::failure(1, "prepare", &error.to_string());
    }
    if !repo.join("scripts/check.sh").is_file() {
        return JobResult::failure(
            1,
            "repository",
            "AGM_CI_REPO_DIR must point to a checkout containing scripts/check.sh",
        );
    }
    let fetch_args = vec![
        "-C".into(),
        repo.to_string_lossy().into_owned(),
        "fetch".into(),
        "--quiet".into(),
        "origin".into(),
    ];
    let Some(remaining) = remaining_seconds(deadline) else {
        return JobResult::timed_out("fetch", "CI job timed out before fetch");
    };
    match bounded_command("git", &fetch_args, repo, remaining).await {
        Ok(output) if output.status.success() => {}
        Ok(output) if matches!(output.status.code(), Some(124 | 137)) => {
            return JobResult::timed_out("fetch", &output_text(&output));
        }
        Ok(output) => {
            return JobResult::failure(status_code(&output), "fetch", &output_text(&output))
        }
        Err(error) => return JobResult::failure(1, "fetch", &error.to_string()),
    }
    let add_args = vec![
        "-C".into(),
        repo.to_string_lossy().into_owned(),
        "worktree".into(),
        "add".into(),
        "--detach".into(),
        worktree.to_string_lossy().into_owned(),
        job.sha.clone(),
    ];
    let Some(remaining) = remaining_seconds(deadline) else {
        return JobResult::timed_out("checkout", "CI job timed out before worktree setup");
    };
    match bounded_command("git", &add_args, repo, remaining).await {
        Ok(output) if output.status.success() => {}
        Ok(output) if matches!(output.status.code(), Some(124 | 137)) => {
            return JobResult::timed_out("checkout", &output_text(&output));
        }
        Ok(output) => {
            return JobResult::failure(status_code(&output), "checkout", &output_text(&output))
        }
        Err(error) => return JobResult::failure(1, "checkout", &error.to_string()),
    }

    let target_dir = work_root.join("target");
    let mut output_tail = String::new();
    let mut failed_steps = Vec::new();
    let mut exit_code = 0;
    let mut timed_out = None;
    let (parts, extra_args): (Vec<&str>, Vec<&str>) = if job.queue == "fast" {
        (vec!["changed"], vec!["origin/main"])
    } else {
        (vec!["ob", "ops", "web", "daemon"], vec![])
    };
    for step in parts {
        let Some(remaining) = remaining_seconds(deadline) else {
            timed_out = Some(step.to_owned());
            break;
        };
        let mut args = vec!["scripts/check.sh".to_owned(), step.to_owned()];
        if step == "changed" {
            args.extend(extra_args.iter().map(|s| (*s).to_owned()));
        }
        let envs = vec![("CARGO_TARGET_DIR".to_owned(), target_dir.clone())];
        match bounded_check(&args, &worktree, remaining, &envs).await {
            Ok(output) => {
                append_tail(&mut output_tail, &output_text(&output));
                match output.status.code() {
                    Some(0) => {}
                    Some(124) | Some(137) => {
                        timed_out = Some(step.to_owned());
                        break;
                    }
                    Some(code) => {
                        exit_code = code as i64;
                        failed_steps.push(step.to_owned());
                    }
                    None => {
                        exit_code = 1;
                        failed_steps.push(step.to_owned());
                    }
                }
            }
            Err(error) => {
                exit_code = 1;
                failed_steps.push(step.to_owned());
                append_tail(&mut output_tail, &error.to_string());
            }
        }
    }

    let remove_args = vec![
        "-C".into(),
        repo.to_string_lossy().into_owned(),
        "worktree".into(),
        "remove".into(),
        "--force".into(),
        worktree.to_string_lossy().into_owned(),
    ];
    match bounded_command("git", &remove_args, repo, 30).await {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            if timed_out.is_none() {
                failed_steps.push("cleanup".into());
                exit_code = status_code(&output);
            }
            append_tail(&mut output_tail, &output_text(&output));
        }
        Err(error) => {
            if timed_out.is_none() {
                failed_steps.push("cleanup".into());
                exit_code = 1;
            }
            append_tail(&mut output_tail, &error.to_string());
        }
    }
    if let Some(step) = timed_out {
        return JobResult::timed_out(&step, &output_tail);
    }
    if failed_steps.is_empty() {
        JobResult::success(0, &output_tail)
    } else {
        JobResult {
            status: "failure",
            exit_code: Some(exit_code.max(1)),
            failed_steps,
            output_tail,
        }
    }
}

fn remaining_seconds(deadline: Instant) -> Option<u64> {
    let remaining = deadline.saturating_duration_since(Instant::now()).as_secs();
    (remaining > 0).then_some(remaining)
}

async fn bounded_command(
    program: &str,
    args: &[String],
    cwd: &FsPath,
    timeout_seconds: u64,
) -> std::io::Result<std::process::Output> {
    let mut command = Command::new("timeout");
    command
        .arg("--kill-after=10s")
        .arg(format!("{timeout_seconds}s"))
        .arg(program)
        .args(args)
        .current_dir(cwd)
        .kill_on_drop(true);
    command.output().await
}

async fn bounded_check(
    args: &[String],
    cwd: &FsPath,
    timeout_seconds: u64,
    envs: &[(String, PathBuf)],
) -> std::io::Result<std::process::Output> {
    let mut command = Command::new("timeout");
    command
        .arg("--kill-after=10s")
        .arg(format!("{timeout_seconds}s"))
        .arg("bash")
        .args(args)
        .current_dir(cwd)
        .env_remove("AM_MODEL")
        .env_remove("AM_EFFORT")
        .env_remove("AM_DATA_DIR")
        .env_remove("AM_DAEMON_EXE")
        .env_remove("AM_CONFIG_PATH")
        .kill_on_drop(true);
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().await
}

fn status_code(output: &std::process::Output) -> i64 {
    output.status.code().unwrap_or(1) as i64
}

fn output_text(output: &std::process::Output) -> String {
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    trim_tail(&text, MAX_OUTPUT_BYTES)
}

fn append_tail(target: &mut String, value: &str) {
    target.push_str(value);
    *target = trim_tail(target, MAX_OUTPUT_BYTES);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{env, restart_app};

    #[tokio::test]
    async fn fast_jobs_persist_and_identical_sha_requests_share_a_result() {
        let e = env().await;
        let first = submit(
            &e.app,
            Queue::Fast,
            &"a".repeat(40),
            &e.project_id,
            "bot:alpha",
        )
        .await
        .unwrap();
        let second = submit(
            &e.app,
            Queue::Fast,
            &"a".repeat(40),
            &e.project_id,
            "bot:beta",
        )
        .await
        .unwrap();

        assert_eq!(first.id, second.id);
        assert_eq!(second.status, "queued");
        assert_eq!(second.routes.len(), 2);

        let restarted = restart_app(&e).await;
        let persisted = get_job(&restarted.db, &first.id).await.unwrap().unwrap();
        assert_eq!(
            persisted.routes.len(),
            2,
            "pane/daemon lifecycle must not erase submitted CI work"
        );
    }

    #[tokio::test]
    async fn full_queue_preserves_running_sha_and_coalesces_pending_to_latest() {
        let e = env().await;
        let running = submit(&e.app, Queue::Full, &"a".repeat(40), &e.project_id, "bot:a")
            .await
            .unwrap();
        let claimed = claim_next(&e.app, Queue::Full).await.unwrap().unwrap();
        assert_eq!(claimed.id, running.id);

        let pending_b = submit(&e.app, Queue::Full, &"b".repeat(40), &e.project_id, "bot:b")
            .await
            .unwrap();
        let pending_c = submit(&e.app, Queue::Full, &"c".repeat(40), &e.project_id, "bot:c")
            .await
            .unwrap();
        assert_eq!(
            pending_b.id, pending_c.id,
            "one pending row represents the latest HEAD"
        );
        assert_eq!(pending_c.sha, "c".repeat(40));
        assert_eq!(
            pending_c.routes.len(),
            2,
            "both agents must receive the coalesced result"
        );

        let after = list_jobs(&e.app, None, None, 100).await.unwrap();
        assert_eq!(after.len(), 2, "one running plus one pending full job");
        assert_eq!(
            after.iter().find(|j| j.id == running.id).unwrap().sha,
            "a".repeat(40)
        );

        finish(&e.app, &running.id, JobResult::success(0, "all green"))
            .await
            .unwrap();
        let next = claim_next(&e.app, Queue::Full).await.unwrap().unwrap();
        assert_eq!(next.id, pending_c.id);
        assert_eq!(next.sha, "c".repeat(40));
    }

    #[tokio::test]
    async fn terminal_result_routes_an_event_to_each_project_agent_pair() {
        let e = env().await;
        let mut events = e.app.subscribe();
        let job = submit(
            &e.app,
            Queue::Fast,
            &"d".repeat(40),
            &e.project_id,
            "bot:alpha",
        )
        .await
        .unwrap();
        submit(
            &e.app,
            Queue::Fast,
            &"d".repeat(40),
            &e.project_id,
            "bot:beta",
        )
        .await
        .unwrap();
        let claimed = claim_next(&e.app, Queue::Fast).await.unwrap().unwrap();
        finish(
            &e.app,
            &claimed.id,
            JobResult::failure(1, "ops", "lint failed"),
        )
        .await
        .unwrap();

        let mut requesters = Vec::new();
        while requesters.len() < 2 {
            let event = events.recv().await.unwrap();
            if event.kind == "ci_job_updated" && event.data["status"] == "failure" {
                requesters.push(event.data["requester"].as_str().unwrap().to_owned());
                assert_eq!(event.data["project_id"], e.project_id);
                assert_eq!(event.data["job_id"], job.id);
            }
        }
        requesters.sort_unstable();
        assert_eq!(requesters, ["bot:alpha", "bot:beta"]);
        let failed = get_job(&e.app.db, &job.id).await.unwrap().unwrap();
        assert_eq!(failed.status, "failure");
        assert_eq!(failed.failed_steps, ["ops"]);
        assert_eq!(failed.output_tail, "lint failed");
    }

    #[tokio::test]
    async fn expired_running_job_is_timed_out_before_the_next_job_is_claimed() {
        let e = env().await;
        let old = submit(
            &e.app,
            Queue::Fast,
            &"e".repeat(40),
            &e.project_id,
            "bot:alpha",
        )
        .await
        .unwrap();
        claim_next(&e.app, Queue::Fast).await.unwrap().unwrap();
        let new = submit(
            &e.app,
            Queue::Fast,
            &"f".repeat(40),
            &e.project_id,
            "bot:beta",
        )
        .await
        .unwrap();
        sqlx::query("UPDATE ci_jobs SET started_at='2000-01-01T00:00:00.000Z' WHERE id=?")
            .bind(&old.id)
            .execute(&e.app.db)
            .await
            .unwrap();

        let next = claim_next(&e.app, Queue::Fast).await.unwrap().unwrap();
        assert_eq!(next.id, new.id);
        assert_eq!(
            get_job(&e.app.db, &old.id).await.unwrap().unwrap().status,
            "timed_out"
        );
    }

    #[test]
    fn only_the_last_output_bytes_are_retained_without_splitting_utf8() {
        assert_eq!(trim_tail("abcδεζ", 6), "δεζ");
        assert_eq!(trim_tail("abc", 20), "abc");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_worker_runs_fast_and_full_checks_in_detached_sha_worktrees() {
        let e = env().await;
        let repo = e.dir.join("ci-repo");
        crate::testing::git::init_repo(&repo);
        std::fs::create_dir_all(repo.join("scripts")).unwrap();
        std::fs::write(
            repo.join("scripts/check.sh"),
            "#!/bin/sh\nprintf 'check:%s\\n' \"$*\"\n",
        )
        .unwrap();
        crate::testing::git::run(&repo, &["add", "scripts/check.sh"]);
        crate::testing::git::run(&repo, &["commit", "-q", "-m", "add fake checks"]);
        crate::testing::git::run(&repo, &["remote", "add", "origin", "."]);
        let sha = crate::testing::git::run(&repo, &["rev-parse", "HEAD"]);
        let work_root = e.dir.join("ci-worker");

        submit(&e.app, Queue::Fast, &sha, &e.project_id, "bot:alpha")
            .await
            .unwrap();
        let fast = claim_next(&e.app, Queue::Fast).await.unwrap().unwrap();
        let fast_result = execute_job(&repo, &work_root, &fast).await;
        assert_eq!(fast_result.status, "success");
        assert!(fast_result
            .output_tail
            .contains("check:changed origin/main"));
        finish(&e.app, &fast.id, fast_result).await.unwrap();

        submit(&e.app, Queue::Full, &sha, &e.project_id, "bot:alpha")
            .await
            .unwrap();
        let full = claim_next(&e.app, Queue::Full).await.unwrap().unwrap();
        let full_result = execute_job(&repo, &work_root, &full).await;
        assert_eq!(full_result.status, "success");
        for step in ["ob", "ops", "web", "daemon"] {
            assert!(
                full_result.output_tail.contains(&format!("check:{step}")),
                "missing full check stage {step}"
            );
        }
        finish(&e.app, &full.id, full_result).await.unwrap();
        assert!(!work_root.join("worktrees").join(&fast.id).exists());
        assert!(!work_root.join("worktrees").join(&full.id).exists());
    }

    #[tokio::test]
    async fn bot_job_api_scopes_project_and_results_to_the_authenticated_agent() {
        let e = env().await;
        let alpha = crate::testing::claude_bot(&e.app, &e.project_id, "alpha").await;
        let beta = crate::testing::claude_bot(&e.app, &e.project_id, "beta").await;
        let (_status, Json(body)) = post_job(
            State(e.app.clone()),
            Extension(RequestPrincipal::Bot(alpha.id.clone())),
            Json(EnqueueIn {
                queue: "fast".into(),
                sha: "a".repeat(40),
                project_id: e.project_id.clone(),
            }),
        )
        .await
        .unwrap();
        let id = body["id"].as_str().unwrap().to_owned();
        assert!(get_job_http(
            State(e.app.clone()),
            Extension(RequestPrincipal::Bot(beta.id)),
            Path(id)
        )
        .await
        .is_err());
    }
}
