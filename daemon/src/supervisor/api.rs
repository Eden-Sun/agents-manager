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
    //
    // `cancel` is the one decision that applies mid-flight, and it has a cost worth stating:
    // the turn is *not* aborted, so whatever the bot says next will not be recorded against
    // this assignment. `block` is refused here on purpose — it would take the row out of the
    // executing set while its turn is still open, and the result would land nowhere. Wait for
    // the turn to end (it will park on `awaiting_review`) and block it then.
    if a.is_executing() && b.decision != "cancel" {
        return Err(LcError::conflict(
            "assignment has not finished executing; only cancel applies while it is in flight",
            json!({"assignment_id": a.id, "status": a.status, "reason": "still_executing"}),
        ));
    }

    let actor = b.actor.clone().unwrap_or_else(|| store::SUPERVISOR_ID.to_string());
    let source = b.source.clone().unwrap_or_else(|| "api".to_string());

    // Cancelling work that is (or may be) already running stops the *tracking*, not the bot.
    // Spelled out here and returned to the caller, because the tempting reading of a cancelled
    // row — "it never went out" — is wrong in two different ways: a `delivered` assignment is
    // running right now, and an `unknown` one may or may not be. The daemon does not abort
    // turns, and it must not let a status imply that it did.
    let still_running = if b.decision != "cancel" {
        None
    } else {
        match a.status.as_str() {
            "delivered" => Some("the turn is still running; cancelling stops tracking it, it does not stop the bot"),
            "unknown" => Some(
                "delivery was never confirmed: this work may or may not be running. Cancelling records your \
                 decision; it does not un-send the prompt and does not abort a turn.",
            ),
            _ => None,
        }
    };

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

    // The caveat goes into the audit row too: whoever reads this decision later should see the
    // same warning the caller got, not just the word `cancelled`.
    let evidence = match (b.evidence.as_deref(), still_running) {
        (Some(e), Some(note)) => Some(format!("{e}｜{note}")),
        (None, Some(note)) => Some(note.to_string()),
        (e, None) => e.map(str::to_string),
    };
    let updated = store::review(
        &app.db,
        &a.id,
        &b.decision,
        &actor,
        &source,
        b.reason.as_deref(),
        evidence.as_deref(),
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
    if let Some(note) = still_running {
        out["warning"] = json!(note);
        // The transport facts stay readable next to the warning: a cancelled `unknown` keeps
        // its `delivery` and `turn_id` precisely so nobody has to guess afterwards.
        out["may_still_be_running"] = json!(true);
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

// ------------------------------------------------------------------------- remote

/// The phone entry point: what is claimed, on what evidence, and when it stops counting.
pub async fn get_remote(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    Ok(Json(super::remote::status(&app).await))
}

#[derive(Deserialize)]
pub struct RemoteObservationIn {
    /// `requested` | `verified` | `unavailable` | `unknown`.
    pub status: String,
    /// `manual` (a person checked) or `provider` (an observation). `argv` is the daemon's own
    /// bookkeeping and is not accepted here.
    pub source: String,
    /// Who is making the claim. Required for anything that asserts the entry point works: an
    /// unattributed "it is fine" is exactly the fiction this endpoint exists to prevent.
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub evidence: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

/// Record an observation of the remote entry point.
///
/// Refuses to let argv, or an anonymous caller, claim the entry point works. The observation is
/// bound to the current session and expires (`remote::OBSERVATION_TTL_SECS`), so it can never
/// harden into a permanent "connected" that nobody rechecked.
pub async fn post_remote_observation(
    State(app): State<Arc<App>>,
    Json(b): Json<RemoteObservationIn>,
) -> Result<Json<Value>, LcError> {
    if !super::remote::STATES.contains(&b.status.as_str()) {
        return Err(LcError::Bad(format!("status must be one of {:?}", super::remote::STATES)));
    }
    let source = super::remote::Source::parse(&b.source)
        .ok_or_else(|| LcError::Bad("source must be `manual` or `provider`".into()))?;
    if source == super::remote::Source::Argv {
        return Err(LcError::Bad("argv is the daemon's own record and cannot report an observation".into()));
    }
    if b.status == "verified" && !source.can_verify() {
        return Err(LcError::conflict(
            "this source cannot verify the remote entry point",
            json!({"reason": "source_cannot_verify", "source": b.source}),
        ));
    }
    let actor = b.actor.as_deref().map(str::trim).filter(|s| !s.is_empty());
    if matches!(b.status.as_str(), "verified" | "unavailable") && actor.is_none() {
        return Err(LcError::Bad("an observation that claims verified or unavailable needs an actor".into()));
    }
    let sup = store::get_or_init(&app.db).await.map_err(up)?;
    let session = match sup.bot_id.as_deref() {
        Some(id) => crate::db::active_run(&app.db, id).await.map_err(up)?.map(|r| r.id),
        None => None,
    };
    store::set_remote_observed(
        &app.db,
        &b.status,
        b.url.as_deref(),
        source.as_str(),
        session.as_deref(),
        actor,
        b.evidence.as_deref(),
    )
    .await
    .map_err(up)?;
    tracing::info!(status = %b.status, source = source.as_str(), actor = actor.unwrap_or(""), "remote entry observation recorded");
    app.emit("supervisor_changed", json!({"remote": true})).await;
    Ok(Json(super::remote::status(&app).await))
}

// ------------------------------------------------------------------------ persona

/// Which copy is which, and what the running session can honestly be said to have.
pub async fn get_persona(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let (text, _) = setup::effective_persona(&app).await?;
    let sup = store::get_or_init(&app.db).await.map_err(up)?;
    let embedded = setup::persona_body();
    let embedded_hash = super::persona::hash(&embedded);
    let run_started = match sup.bot_id.as_deref() {
        Some(id) => crate::db::active_run(&app.db, id).await.map_err(up)?.map(|r| r.started_at),
        None => None,
    };
    let loaded = super::persona::loaded_state(run_started.as_deref(), sup.persona_updated_at.as_deref());
    Ok(Json(json!({
        "stored": {
            "version": sup.persona_version,
            "hash": sup.persona_hash,
            "source": sup.persona_source,
            "updated_at": sup.persona_updated_at,
            "seeded_from": sup.persona_seed_hash,
            "length": text.chars().count(),
            "text": text,
        },
        "embedded": {"hash": embedded_hash, "length": embedded.chars().count()},
        // Never `verified`. The daemon passes the persona when it starts the CLI and cannot see
        // what the session holds now, so the only claims here are "nothing is running",
        // "the session predates this text" and "it was started with this text".
        "loaded": {
            "status": loaded.as_str(),
            "run_started_at": run_started,
            "evidence": "the daemon passes the persona at start; it cannot observe the live session",
        },
        // The embedded text moved on. Reported, never applied on its own — that is
        // `POST /api/supervisor/persona/adopt-embedded`.
        "upgrade_available": sup.persona_seed_hash.as_deref().is_some_and(|h| h != embedded_hash),
        "needs_restart": loaded.needs_restart(),
    })))
}

#[derive(Deserialize)]
pub struct PersonaIn {
    pub text: String,
    /// Optimistic concurrency: refuse if somebody else has written since you read.
    #[serde(default)]
    pub expected_version: Option<i64>,
}

/// Set the persona. This is the supported path — not editing `config.toml` by hand, and not
/// waiting for some binary's default to win.
pub async fn put_persona(State(app): State<Arc<App>>, Json(b): Json<PersonaIn>) -> Result<Json<Value>, LcError> {
    let text = b.text.trim();
    if text.is_empty() {
        return Err(LcError::Bad("persona text must not be empty".into()));
    }
    let _g = super::lock().await;
    let sup = store::get_or_init(&app.db).await.map_err(up)?;
    if let Some(expected) = b.expected_version {
        if expected != sup.persona_version {
            return Err(LcError::conflict(
                "the persona changed since you read it",
                json!({"reason": "version_mismatch", "expected": expected, "current": sup.persona_version}),
            ));
        }
    }
    let version = store::set_persona(&app.db, text, "api", None).await.map_err(up)?;
    apply_persona(&app, text).await?;
    drop(_g);
    app.emit("supervisor_changed", json!({"persona_version": version})).await;
    Ok(Json(get_persona(State(app.clone())).await?.0))
}

#[derive(Deserialize)]
pub struct AdoptIn {
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Take the binary's persona deliberately. The only way the embedded text replaces a stored one
/// — a migration somebody asked for, recorded as such, never a side effect of running `setup`.
pub async fn post_persona_adopt(
    State(app): State<Arc<App>>,
    Json(b): Json<AdoptIn>,
) -> Result<Json<Value>, LcError> {
    let embedded = setup::persona_body();
    let _g = super::lock().await;
    let sup = store::get_or_init(&app.db).await.map_err(up)?;
    let embedded_hash = super::persona::hash(&embedded);
    if sup.persona_hash.as_deref() == Some(embedded_hash.as_str()) {
        return Ok(Json(json!({"changed": false, "reason": "already_identical", "version": sup.persona_version})));
    }
    let version = store::set_persona(&app.db, &embedded, "embedded", Some(&embedded_hash)).await.map_err(up)?;
    apply_persona(&app, &embedded).await?;
    drop(_g);
    tracing::info!(
        version,
        actor = b.actor.as_deref().unwrap_or("AGM"),
        reason = b.reason.as_deref().unwrap_or(""),
        "AGM persona migrated to the embedded version"
    );
    app.emit("supervisor_changed", json!({"persona_version": version})).await;
    Ok(Json(json!({"changed": true, "version": version, "hash": embedded_hash})))
}

/// Push the stored persona into the derived copies: the bot's config entry and `persona.md`.
/// Neither is authoritative; both are rewritten from the stored text so they cannot drift.
async fn apply_persona(app: &Arc<App>, text: &str) -> Result<(), LcError> {
    let sup = store::get_or_init(&app.db).await.map_err(up)?;
    let Some(bot_id) = sup.bot_id.clone() else { return Ok(()) };
    let t = text.to_string();
    let bid = bot_id.clone();
    app.cfg
        .update(move |cfg| {
            for p in cfg.projects.iter_mut() {
                if let Some(b) = p.bots.iter_mut().find(|b| b.id.as_deref() == Some(bid.as_str())) {
                    b.persona = Some(t.clone());
                }
            }
            Ok(())
        })
        .await
        .map_err(up)?;
    let _ = crate::projection::project_config(&app.cfg, &app.db).await;
    let _ = std::fs::write(setup::agm_dir(app).join("persona.md"), text);
    Ok(())
}

/// What is compiled into the binary, so the update script can tell "the release is behind" from
/// "only docs moved" without having to know about `include_str!` itself.
pub async fn get_build_inputs(State(_app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    Ok(Json(super::persona::build_inputs_json()))
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
