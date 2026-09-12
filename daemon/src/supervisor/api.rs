//! `/api/supervisor/*` — the contract the web UI and the `agm` CLI both code against.
//!
//! Every route sits inside the existing `X-AM-Token` auth layer; the supervisor has no
//! privilege of its own, it simply reuses the daemon's local management authority.

use crate::lifecycle::LcError;
use crate::state::App;
use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use super::{controller, health, setup, store};

fn up<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(e.to_string())
}

pub async fn get_supervisor(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    Ok(Json(super::status_json(&app).await?))
}

pub async fn get_health(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    Ok(Json(health::snapshot(&app).await?))
}

/// Build the environment. Idempotent, and deliberately does **not** start anything: a manager
/// that comes up by itself before a human has looked at it is how you get a Remote Control
/// session nobody verified.
pub async fn post_setup(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let _g = super::lock().await;
    let (_project_id, bot_id, deployed) = setup::ensure_env(&app).await?;
    let sup = store::get_or_init(&app.db).await.map_err(up)?;
    // Bringing the environment back up is a new generation: an old controller (from a config
    // that pointed at a bot which no longer exists) must not keep sending notifications.
    if sup.generation == 0 {
        controller::respawn(&app).await;
    }
    controller::reconcile(&app).await;
    app.emit("supervisor_changed", json!({"bot_id": bot_id})).await;
    let mut out = super::status_json(&app).await?;
    out["deployed"] = serde_json::to_value(&deployed).unwrap_or(Value::Null);
    Ok(Json(out))
}

pub async fn post_start(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let _g = super::lock().await;
    // Choose the configured fallback before launching the CLI, so a low Fable bucket starts
    // directly on Opus instead of briefly opening the wrong session.
    if let Err(e) = controller::apply_quota_policy(&app).await {
        tracing::warn!(error = ?e, "quota policy during start failed");
    }
    super::start_manager(&app, None).await?;
    // From here on the manager is *supposed* to be up: if it dies, the watchdog brings it back.
    let _ = store::set_desired_running(&app.db, true).await;
    Ok(Json(super::status_json(&app).await?))
}

pub async fn post_stop(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let _g = super::lock().await;
    let bot = super::manager_bot(&app)
        .await?
        .ok_or_else(|| LcError::conflict("supervisor is not set up", json!({"reason": "not_configured"})))?;
    // Written *before* the stop: a watchdog tick between the two must not read "wanted, but
    // stopped" and start it straight back up.
    let _ = store::set_desired_running(&app.db, false).await;
    crate::lifecycle::stop_bot(&app, &bot.id).await?;
    // A stopped CLI takes its Remote Control session with it; claiming otherwise would send
    // the user to a dead URL on their phone.
    let _ = store::set_remote(&app.db, "unknown", None).await;
    app.emit("supervisor_changed", json!({"stopped": true})).await;
    Ok(Json(super::status_json(&app).await?))
}

/// Switch to the other fixed candidate. Bounded: see `controller::switch_candidate`.
pub async fn post_fallback(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let _g = super::lock().await;
    let sup = store::get_or_init(&app.db).await.map_err(up)?;
    let next = setup::other_candidate(&sup.active_model);
    let switched = controller::switch_candidate(&app, next, "requested", None).await?;
    let mut out = super::status_json(&app).await?;
    out["switched"] = json!(switched);
    Ok(Json(out))
}

pub async fn get_assignments(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let rows = store::list_assignments(&app.db, 200).await.map_err(up)?;
    Ok(Json(json!({"assignments": rows.iter().map(store::Assignment::to_json).collect::<Vec<_>>()})))
}

#[derive(Deserialize)]
pub struct AssignIn {
    pub target_bot_id: String,
    pub text: String,
    pub client_request_id: String,
    #[serde(default)]
    pub source_turn_id: Option<String>,
    /// Files / modules this assignment is being handed, for conflict reporting (§18.4).
    #[serde(default)]
    pub ownership: Vec<String>,
}

pub async fn post_assignment(
    State(app): State<Arc<App>>,
    Json(b): Json<AssignIn>,
) -> Result<Json<Value>, LcError> {
    let a = super::assign(
        &app,
        &b.target_bot_id,
        &b.text,
        &b.client_request_id,
        b.source_turn_id.as_deref(),
        &b.ownership,
        None,
    )
    .await?;
    Ok(Json(a))
}

/// One assignment, with its decision history.
pub async fn get_assignment(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Json<Value>, LcError> {
    let a = store::assignment(&app.db, &id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("assignment".into()))?;
    let mut out = a.to_json();
    out["reviews"] = json!(store::reviews(&app.db, &a.id).await.map_err(up)?);
    Ok(Json(out))
}

#[derive(Deserialize)]
pub struct ReviewIn {
    /// accept | block | followup | fail | cancel
    pub decision: String,
    /// Who decided. AGM names itself; a human acting through the UI says so.
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    /// What the decision rests on — a turn id, a commit, a build log line.
    #[serde(default)]
    pub evidence: Option<String>,
    /// `followup` only: the continuation's text and its own stable request id.
    #[serde(default)]
    pub followup_text: Option<String>,
    #[serde(default)]
    pub followup_request_id: Option<String>,
    #[serde(default)]
    pub followup_bot_id: Option<String>,
    #[serde(default)]
    pub ownership: Vec<String>,
}

/// Accept, block, continue, fail or cancel an assignment.
///
/// This is the only way an assignment is ever called done. A turn ending puts it on
/// `awaiting_review`; what happened to the *work* is a judgement, and a judgement has an author,
/// a reason and evidence attached to it (SPEC §18.3).
///
/// Idempotent: repeating a decision that is already recorded returns the same row without
/// writing a second audit entry, and a `followup` reuses its `followup_request_id`, so a retry
/// after a timeout cannot fan out into two continuations.
pub async fn post_review(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(b): Json<ReviewIn>,
) -> Result<Json<Value>, LcError> {
    let _g = super::lock().await;
    let to_status = store::decision_status(&b.decision).ok_or_else(|| {
        LcError::Bad("decision must be one of accept | block | followup | fail | cancel".into())
    })?;
    let a = store::assignment(&app.db, &id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("assignment".into()))?;

    // Already decided this way: hand back what is on file. A retry after a lost response must
    // not look like a second decision.
    if a.status == to_status && a.review_decision.as_deref() == Some(b.decision.as_str()) {
        let mut out = a.to_json();
        out["reviews"] = json!(store::reviews(&app.db, &a.id).await.map_err(up)?);
        out["idempotent"] = json!(true);
        return Ok(Json(out));
    }
    // A decision is only meaningful on something that is still open. Re-deciding a closed
    // assignment would rewrite history; the follow-up mechanism exists for that.
    if !a.is_open() {
        return Err(LcError::conflict(
            "assignment is already closed; create a follow-up assignment instead of re-deciding this one",
            json!({"assignment_id": a.id, "status": a.status, "reason": "already_closed"}),
        ));
    }
    // The turn is still running: there is nothing to accept yet, and accepting it would be a
    // claim about work we can still watch happening.
    if a.is_executing() && b.decision != "cancel" && b.decision != "block" {
        return Err(LcError::conflict(
            "assignment has not finished executing; only cancel or block apply while it is in flight",
            json!({"assignment_id": a.id, "status": a.status, "reason": "still_executing"}),
        ));
    }

    let actor = b.actor.clone().unwrap_or_else(|| store::SUPERVISOR_ID.to_string());
    let source = b.source.clone().unwrap_or_else(|| "api".to_string());

    // A follow-up is a *new* assignment carrying the unfinished part forward, never an edit of
    // the one already sent: rewriting delivered text is how a bot ends up working from words
    // nobody sent it.
    let followup = if b.decision == "followup" {
        let text = b
            .followup_text
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| LcError::Bad("followup needs followup_text".into()))?;
        let crid = b
            .followup_request_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| LcError::Bad("followup needs a stable followup_request_id".into()))?;
        let target = b.followup_bot_id.clone().unwrap_or_else(|| a.target_bot_id.clone());
        let ownership = if b.ownership.is_empty() { a.ownership() } else { b.ownership.clone() };
        drop(_g);
        Some(super::assign(&app, &target, text, crid, None, &ownership, Some(&a.id)).await?)
    } else {
        None
    };
    let followup_id = followup.as_ref().and_then(|f| f.get("id").and_then(Value::as_str).map(str::to_string));

    let updated = store::review(
        &app.db,
        &a.id,
        &b.decision,
        &actor,
        &source,
        b.reason.as_deref(),
        b.evidence.as_deref(),
        followup_id.as_deref(),
    )
    .await
    .map_err(up)?
    .ok_or_else(|| LcError::NotFound("assignment".into()))?;

    app.emit("supervisor_changed", json!({"assignment_id": updated.id, "status": updated.status})).await;
    let mut out = updated.to_json();
    out["reviews"] = json!(store::reviews(&app.db, &updated.id).await.map_err(up)?);
    if let Some(f) = followup {
        out["followup"] = f;
    }
    Ok(Json(out))
}

#[derive(Deserialize, Default)]
pub struct IncidentQuery {
    /// `all=1`: resolved incidents too (newest first). Default: only what is open.
    #[serde(default)]
    pub all: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

pub async fn get_incidents(
    State(app): State<Arc<App>>,
    Query(q): Query<IncidentQuery>,
) -> Result<Json<Value>, LcError> {
    let all = q.all.as_deref().is_some_and(|v| matches!(v, "1" | "true" | "yes"));
    let rows = if all {
        store::incidents(&app.db, q.limit.unwrap_or(100).clamp(1, 1000)).await.map_err(up)?
    } else {
        store::open_incidents(&app.db).await.map_err(up)?
    };
    Ok(Json(json!({
        "incidents": rows.iter().map(store::Incident::to_json).collect::<Vec<_>>(),
        "open": store::open_incidents(&app.db).await.map_err(up)?.len(),
        "all": all,
    })))
}

pub async fn get_handoff(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let sup = store::get_or_init(&app.db).await.map_err(up)?;
    let assignments = store::list_assignments(&app.db, 200).await.map_err(up)?;
    let open = assignments.iter().filter(|a| a.is_open()).count();
    Ok(Json(json!({
        "summary": sup.summary,
        "summary_version": sup.summary_version,
        "updated_at": sup.updated_at,
        "requests": store::requests(&app.db, 100).await.map_err(up)?,
        "assignments": assignments.iter().map(store::Assignment::to_json).collect::<Vec<_>>(),
        "inbox": store::inbox(&app.db, 100).await.map_err(up)?.iter().map(store::InboxEvent::to_json).collect::<Vec<_>>(),
        "open_assignments": open,
        "pending_count": store::pending_count(&app.db).await.map_err(up)?,
    })))
}

#[derive(Deserialize)]
pub struct HandoffIn {
    pub summary: String,
}

pub async fn put_handoff(State(app): State<Arc<App>>, Json(b): Json<HandoffIn>) -> Result<Json<Value>, LcError> {
    let version = store::set_summary(&app.db, &b.summary).await.map_err(up)?;
    // The readable copy in the manager's own directory. The database stays authoritative;
    // this is what the manager reads on a cold start before anything else is available.
    let sup = store::get_or_init(&app.db).await.map_err(up)?;
    if let Some(cwd) = sup.cwd.as_deref() {
        let _ = std::fs::write(
            std::path::Path::new(cwd).join("handoff.md"),
            format!("# AGM 管理摘要（v{version}）\n\n{}\n", b.summary),
        );
    }
    app.emit("supervisor_changed", json!({"summary_version": version})).await;
    Ok(Json(json!({"summary": b.summary, "summary_version": version})))
}

#[derive(Deserialize, Default)]
pub struct InboxQuery {
    /// `all=1`: include handled events too (newest first, the audit view). Default is the
    /// work view: only what is still open, oldest first.
    #[serde(default)]
    pub all: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

pub async fn get_inbox(
    State(app): State<Arc<App>>,
    Query(q): Query<InboxQuery>,
) -> Result<Json<Value>, LcError> {
    let limit = q.limit.unwrap_or(200).clamp(1, 1000);
    let all = q.all.as_deref().is_some_and(|v| matches!(v, "1" | "true" | "yes"));
    let events = if all {
        store::inbox(&app.db, limit).await.map_err(up)?
    } else {
        store::open_inbox(&app.db, limit).await.map_err(up)?
    };
    Ok(Json(json!({
        "events": events.iter().map(store::InboxEvent::to_json).collect::<Vec<_>>(),
        "open": store::open_inbox_count(&app.db).await.map_err(up)?,
        "all": all,
        "limit": limit,
    })))
}

pub async fn post_inbox_ack(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Json<Value>, LcError> {
    if !store::ack_inbox(&app.db, &id).await.map_err(up)? {
        return Err(LcError::NotFound("inbox event".into()));
    }
    app.emit("supervisor_changed", json!({"acked": id})).await;
    Ok(Json(json!({})))
}

pub async fn get_sanitized_state(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    Ok(Json(super::sanitized_state(&app).await?))
}

// ------------------------------------------------------------- approvals & leases

#[derive(Deserialize)]
pub struct ApprovalIn {
    pub requester: String,
    /// `rebuild` | `restart`: what this is permission *for*. A lease can only be taken on the
    /// resource its approval names.
    pub purpose: String,
    pub scope: String,
    #[serde(default)]
    pub target_commit: Option<String>,
    #[serde(default)]
    pub expires_in_secs: Option<i64>,
}

fn iso_in(secs: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::seconds(secs)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Ask for a rebuild / restart window. Creates a `pending` record; AGM decides it.
pub async fn post_approval(State(app): State<Arc<App>>, Json(b): Json<ApprovalIn>) -> Result<Json<Value>, LcError> {
    if !super::maintenance::RESOURCES.contains(&b.purpose.as_str()) {
        return Err(LcError::Bad(format!("purpose must be one of {:?}", super::maintenance::RESOURCES)));
    }
    let expires = b.expires_in_secs.map(iso_in);
    let a = store::create_approval(
        &app.db,
        &b.requester,
        &b.purpose,
        &b.scope,
        b.target_commit.as_deref(),
        expires.as_deref(),
    )
    .await
    .map_err(up)?;
    // AGM decides these itself (CLAUDE.md, 2026-09-12): the request is put in front of it as an
    // inbox event rather than sent to the user to relay.
    let _ = store::push_inbox(
        &app.db,
        &format!("approval:{}:requested", a.id),
        "approval_requested",
        None,
        None,
        None,
        &a.to_json(),
    )
    .await;
    app.emit("supervisor_changed", json!({"approval": a.to_json()})).await;
    Ok(Json(a.to_json()))
}

#[derive(Deserialize)]
pub struct DecisionIn {
    /// approve | deny | revoke
    pub decision: String,
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub expires_in_secs: Option<i64>,
}

pub async fn post_approval_decision(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(b): Json<DecisionIn>,
) -> Result<Json<Value>, LcError> {
    let status = match b.decision.as_str() {
        "approve" => "approved",
        "deny" => "denied",
        "revoke" => "revoked",
        _ => return Err(LcError::Bad("decision must be approve | deny | revoke".into())),
    };
    let current = store::approval(&app.db, &id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("approval".into()))?;
    // A decision that is already recorded is answered from the row: a retry is not a new
    // decision, and re-approving something that was revoked has to be explicit.
    if current.status == status {
        let mut out = current.to_json();
        out["idempotent"] = json!(true);
        return Ok(Json(out));
    }
    if status == "approved" && current.status != "pending" {
        return Err(LcError::conflict(
            "only a pending approval can be approved; ask again with a new request",
            json!({"approval_id": current.id, "status": current.status, "reason": "already_decided"}),
        ));
    }
    let expires = b.expires_in_secs.map(iso_in);
    let actor = b.actor.clone().unwrap_or_else(|| store::SUPERVISOR_ID.to_string());
    let a = store::decide_approval(&app.db, &id, status, &actor, b.reason.as_deref(), expires.as_deref())
        .await
        .map_err(up)?
        .ok_or_else(|| LcError::NotFound("approval".into()))?;
    app.emit("supervisor_changed", json!({"approval": a.to_json()})).await;
    Ok(Json(a.to_json()))
}

pub async fn get_approvals(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let rows = store::approvals(&app.db, 100).await.map_err(up)?;
    Ok(Json(json!({"approvals": rows.iter().map(store::Approval::to_json).collect::<Vec<_>>()})))
}

/// Whether a window would be safe *right now*. A read: poll it while you wait, and take the
/// window with `acquire`, which re-checks this under the lock.
pub async fn get_maintenance_safety(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    Ok(Json(super::maintenance::safety(&app, &[]).await?))
}

pub async fn get_leases(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let rows = store::leases(&app.db).await.map_err(up)?;
    Ok(Json(json!({"leases": rows.iter().map(store::Lease::to_json).collect::<Vec<_>>()})))
}

#[derive(Deserialize)]
pub struct AcquireIn {
    pub owner: String,
    pub approval_id: String,
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub ttl_secs: Option<i64>,
    /// Refuse the window unless nothing is running. Default true: taking a restart window while
    /// a bot is mid-turn is the thing this exists to prevent.
    #[serde(default = "yes")]
    pub require_idle: bool,
    /// Bots to ignore in the idle check — normally just the one doing the maintenance.
    #[serde(default)]
    pub exclude_bot_ids: Vec<String>,
}

fn yes() -> bool {
    true
}

pub async fn post_lease_acquire(
    State(app): State<Arc<App>>,
    Path(resource): Path<String>,
    Json(b): Json<AcquireIn>,
) -> Result<Json<Value>, LcError> {
    Ok(Json(
        super::maintenance::acquire(
            &app,
            &resource,
            &b.owner,
            &b.approval_id,
            b.commit.as_deref(),
            b.ttl_secs.unwrap_or(super::maintenance::DEFAULT_TTL_SECS),
            b.require_idle,
            &b.exclude_bot_ids,
        )
        .await?,
    ))
}

#[derive(Deserialize)]
pub struct LeaseHolderIn {
    pub owner: String,
    /// The fence from `acquire`. A stale one is refused — that is the whole point of it.
    pub fence: i64,
    #[serde(default)]
    pub ttl_secs: Option<i64>,
}

pub async fn post_lease_renew(
    State(app): State<Arc<App>>,
    Path(resource): Path<String>,
    Json(b): Json<LeaseHolderIn>,
) -> Result<Json<Value>, LcError> {
    let ttl = b.ttl_secs.unwrap_or(super::maintenance::DEFAULT_TTL_SECS).clamp(30, super::maintenance::MAX_TTL_SECS);
    if !store::renew_lease(&app.db, &resource, &b.owner, b.fence, &iso_in(ttl)).await.map_err(up)? {
        let held = store::lease(&app.db, &resource).await.map_err(up)?;
        return Err(LcError::conflict(
            "this lease is no longer yours",
            json!({"reason": "lease_lost", "lease": held.map(|l| l.to_json())}),
        ));
    }
    let l = store::lease(&app.db, &resource).await.map_err(up)?.ok_or_else(|| LcError::NotFound("lease".into()))?;
    Ok(Json(l.to_json()))
}

pub async fn post_lease_release(
    State(app): State<Arc<App>>,
    Path(resource): Path<String>,
    Json(b): Json<LeaseHolderIn>,
) -> Result<Json<Value>, LcError> {
    let released = store::release_lease(&app.db, &resource, &b.owner, b.fence).await.map_err(up)?;
    let l = store::lease(&app.db, &resource).await.map_err(up)?.ok_or_else(|| LcError::NotFound("lease".into()))?;
    // Consume the approval with the lease: one yes, one window. Asking again is cheap; a
    // permission that silently stays usable is not.
    if released {
        if let Some(ap) = l.approval_id.as_deref() {
            if let Ok(Some(a)) = store::approval(&app.db, ap).await {
                if a.status == "approved" {
                    let _ = store::decide_approval(&app.db, ap, "consumed", &b.owner, Some("lease released"), None).await;
                }
            }
        }
    }
    app.emit("supervisor_changed", json!({"lease": l.to_json()})).await;
    Ok(Json(json!({"released": released, "lease": l.to_json()})))
}
