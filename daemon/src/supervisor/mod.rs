//! AGM — the persistent supervisor layer (docs/goals/agm-supervisor-environment-plan-2026-09-09.md).
//!
//! The manager is an ordinary claude bot with a persona; everything that must survive a
//! restart, a model switch or a lost session is here, in the daemon, not in its context.
//! The model understands and decides. The daemon persists, retries, watches and switches.

pub mod api;
pub mod controller;
pub mod health;
pub mod incidents;
pub mod maintenance;
pub mod policy;
pub mod setup;
pub mod store;
pub mod watchdog;

use crate::lifecycle::LcError;
use crate::state::App;
use serde_json::{json, Value};
use std::sync::Arc;
use std::sync::OnceLock;
use tokio::sync::Mutex;

/// One lock for every state-changing supervisor operation: setup, start, stop, fallback and
/// the dispatch half of an assignment. They all read-modify-write the same one row and the
/// same one bot, and two of them at once is how you get a second AGM or a double send.
fn op_lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

pub async fn lock() -> tokio::sync::MutexGuard<'static, ()> {
    op_lock().lock().await
}

const BOOTSTRAP_REQUEST_ID: &str = "agm-bootstrap-v1";
const BOOTSTRAP_PROMPT: &str = r#"這是 AGM 啟動握手，不是新的工作委派。

請先依你的恢復流程讀取 handoff.md、bin/agm assignments、bin/agm inbox，再用 bin/agm state 查即時狀態。確認你是 agents-manager 的總管，能協助使用者找回適合的既有 bot、提出有證據的分配建議，並在使用者明確交辦後透過 bin/agm 建立與追蹤 assignment。

請用繁體中文回覆一段簡短的「AGM 已就緒」訊息，說明使用者可以直接提出問題、要求尋找相關 bot，或說「交給你推薦的 bot」。不要自行建立工作，也不要把這段握手當成待辦。"#;

/// Bring the manager up. The one start path, shared by `POST /supervisor/start` and the
/// watchdog; the caller holds [`lock`]. `detail` is what `status_detail` should say afterwards
/// (the watchdog writes why it did this; the API clears it).
pub async fn start_manager(app: &Arc<App>, detail: Option<&str>) -> Result<(), LcError> {
    let bot = manager_bot(app)
        .await?
        .ok_or_else(|| LcError::conflict("supervisor is not set up", json!({"reason": "not_configured"})))?;
    if crate::db::active_run(&app.db, &bot.id).await.map_err(up)?.is_none() {
        crate::lifecycle::start_bot(app, &bot.id).await?;
    }
    // A newly spawned CLI is intentionally idle until it receives a first turn. Send one
    // idempotent handshake so the user can immediately see how to use AGM; the stable request
    // id prevents a restart from creating another greeting turn.
    if let Err(e) = crate::lifecycle::prompt(app, &bot.id, BOOTSTRAP_PROMPT, BOOTSTRAP_REQUEST_ID).await {
        tracing::warn!(error = ?e, "AGM bootstrap prompt was not delivered");
    }
    // The bot's args carry `--remote-control AGM`, which is a *request*. Whether a remote
    // session actually came up is something only an observation can say, so the status stays
    // `requested` until something verifies it.
    let _ = store::set_remote(&app.db, "requested", None).await;
    let _ = store::set_status(&app.db, "", detail).await;
    let sup = store::get_or_init(&app.db).await.map_err(up)?;
    let gen = if sup.generation == 0 {
        store::bump_generation(&app.db).await.map_err(up)?
    } else {
        sup.generation
    };
    controller::spawn(app.clone(), gen);
    controller::reconcile(app).await;
    app.emit("supervisor_changed", json!({"started": true})).await;
    Ok(())
}

fn up<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(e.to_string())
}

/// The manager's bot, if it is configured *and* still present (a user can delete it).
pub async fn manager_bot(app: &Arc<App>) -> Result<Option<crate::db::Bot>, LcError> {
    let sup = store::get_or_init(&app.db).await.map_err(up)?;
    let Some(id) = sup.bot_id else { return Ok(None) };
    Ok(crate::db::bot(&app.db, &id).await.map_err(up)?.filter(|b| b.deleted_at.is_none()))
}

/// `idle` | `busy`: whether a prompt would be accepted right now. Anything else is not a
/// state we may send work into.
pub async fn manager_liveness(app: &Arc<App>, bot_id: &str) -> Result<&'static str, LcError> {
    let Some(run) = crate::db::active_run(&app.db, bot_id).await.map_err(up)? else { return Ok("stopped") };
    if run.state == "starting" {
        return Ok("starting");
    }
    if run.state != "running" {
        return Ok("stopped");
    }
    if run.agent_status == "working" || run.agent_status == "blocked" {
        return Ok("busy");
    }
    if crate::db::in_flight_turn(&app.db, &run.id).await.map_err(up)?.is_some() {
        return Ok("busy");
    }
    Ok("idle")
}

/// The `GET /api/supervisor` payload — the one shape the web UI and the `agm` CLI both read.
pub async fn status_json(app: &Arc<App>) -> Result<Value, LcError> {
    let sup = store::get_or_init(&app.db).await.map_err(up)?;
    let bot = manager_bot(app).await?;
    let configured = bot.is_some();
    let status = if !configured {
        "not_configured".to_string()
    } else if !sup.status.is_empty() {
        // A sticky `waiting_quota` / `failed` outranks the run: the process may well be up
        // and simply unable to answer.
        sup.status.clone()
    } else {
        manager_liveness(app, bot.as_ref().map(|b| b.id.as_str()).unwrap_or_default()).await?.to_string()
    };
    let assignments = store::list_assignments(&app.db, 50).await.map_err(up)?;
    Ok(json!({
        "configured": configured,
        "bot_id": bot.as_ref().map(|b| b.id.clone()),
        "project_id": bot.as_ref().map(|b| b.project_id.clone()),
        "model": sup.active_model,
        "model_arg": setup::model_arg(&sup.active_model),
        "identity": sup.identity,
        "effort": sup.effort,
        "status": status,
        "status_detail": sup.status_detail,
        "generation": sup.generation,
        "cwd": sup.cwd,
        "quota_reset_at": sup.quota_reset_at,
        "remote": {"status": sup.remote_status, "url": sup.remote_url},
        "pending_count": store::pending_count(&app.db).await.map_err(up)?,
        "assignments": assignments.iter().map(store::Assignment::to_json).collect::<Vec<_>>(),
    }))
}

/// Hand a piece of work to a worker bot.
///
/// Order matters and is the whole point: the assignment row is committed *first*, then the
/// prompt goes out under that row's `client_request_id`. A crash in between leaves a `queued`
/// row the controller retries with the same id, so the worker never gets the job twice.
pub async fn assign(
    app: &Arc<App>,
    target_bot_id: &str,
    text: &str,
    client_request_id: &str,
    source_turn_id: Option<&str>,
    ownership: &[String],
    follow_up_of: Option<&str>,
) -> Result<Value, LcError> {
    if client_request_id.trim().is_empty() {
        return Err(LcError::Bad("client_request_id must not be empty".into()));
    }
    if text.trim().is_empty() {
        return Err(LcError::Bad("text must not be empty".into()));
    }
    let _g = lock().await;

    // A retry of the same request is the same assignment, never a second one.
    if let Some(a) = store::assignment_by_crid(&app.db, client_request_id).await.map_err(up)? {
        if a.target_bot_id != target_bot_id {
            return Err(LcError::conflict(
                "client_request_id already used for another bot",
                json!({"assignment_id": a.id, "target_bot_id": a.target_bot_id}),
            ));
        }
        // Same id, different words: the model would read a 200 here as "my new instruction
        // went out", and it did not. Only a byte-identical retry is idempotent.
        if a.text != text {
            return Err(LcError::conflict(
                "client_request_id already used with different text",
                json!({"assignment_id": a.id, "reason": "text_mismatch"}),
            ));
        }
        return Ok(a.to_json());
    }

    let sup = store::get_or_init(&app.db).await.map_err(up)?;
    let manager_id = sup.bot_id.clone().ok_or_else(|| {
        LcError::conflict("supervisor is not set up", json!({"reason": "not_configured"}))
    })?;
    if target_bot_id == manager_id {
        return Err(LcError::Bad("the supervisor cannot assign work to itself".into()));
    }
    let target = crate::db::bot(&app.db, target_bot_id)
        .await
        .map_err(up)?
        .filter(|b| b.deleted_at.is_none())
        .ok_or_else(|| LcError::NotFound("bot".into()))?;
    // SPEC-team: a team member takes work from its PM through the team relay. Dropping an
    // ordinary prompt on it would race the scheduler, so this is refused, not queued.
    if target.managed_by == "team" {
        return Err(LcError::conflict(
            "target bot is team-managed; coordinate through the team instead",
            json!({"reason": "team_managed", "team_id": target.team_id}),
        ));
    }

    // The user's own words behind this assignment, with a stable id to dedupe on. A text hash
    // would not be one — two identical asks are two asks.
    let (source, source_key, request_text) = source_of(app, &manager_id, source_turn_id, text).await?;
    let request_id = store::upsert_request(&app.db, source, source_key.as_deref(), &request_text)
        .await
        .map_err(up)?;

    // Who else is already holding these files. Reported, never enforced: the daemon cannot
    // know that two modules are really independent, so this goes to AGM to arbitrate rather
    // than refusing work on a string match (SPEC §18.4).
    let conflicts = ownership_conflicts(app, ownership, None).await?;

    let a = store::insert_assignment(
        &app.db,
        Some(&request_id),
        target_bot_id,
        client_request_id,
        text,
        ownership,
        follow_up_of,
    )
    .await
    .map_err(up)?;
    if let Some(parent) = follow_up_of {
        let _ = store::link_followup(&app.db, parent, &a.id).await;
    }
    // Best effort: a failure here leaves the row queued, which is the recoverable state.
    controller::dispatch(app, &a.id).await;
    let a = store::assignment(&app.db, &a.id).await.map_err(up)?.unwrap_or(a);
    app.emit("supervisor_changed", json!({"assignment_id": a.id, "status": a.status})).await;
    let mut out = a.to_json();
    out["ownership_conflicts"] = json!(conflicts);
    Ok(out)
}

/// Open assignments whose declared ownership overlaps `paths`.
///
/// A plain prefix match on the declared strings: `daemon/src/supervisor` overlaps
/// `daemon/src/supervisor/store.rs` and itself, and nothing else. Cheap, explainable, and wrong
/// only in the safe direction — it can report an overlap that is not one, which costs AGM a
/// glance; it cannot quietly hand two bots the same file.
pub async fn ownership_conflicts(
    app: &Arc<App>,
    paths: &[String],
    ignore_assignment: Option<&str>,
) -> Result<Vec<Value>, LcError> {
    if paths.is_empty() {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    for other in store::unsettled_assignments(&app.db).await.map_err(up)? {
        if Some(other.id.as_str()) == ignore_assignment {
            continue;
        }
        let held = other.ownership();
        let overlap: Vec<String> = paths
            .iter()
            .filter(|p| held.iter().any(|h| overlaps(p, h)))
            .cloned()
            .collect();
        if !overlap.is_empty() {
            out.push(json!({
                "assignment_id": other.id,
                "target_bot_id": other.target_bot_id,
                "status": other.status,
                "paths": overlap,
            }));
        }
    }
    Ok(out)
}

fn overlaps(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim_end_matches('/'), b.trim_end_matches('/'));
    a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
}

/// Where an assignment came from: `(source, source_key, text)`.
///
/// The phone talks to the manager over Remote Control, which is a native session — its user
/// echo reaches the daemon through the hook / transcript path, and that path is **not**
/// verified to carry every word. So: a caller-supplied `source_turn_id` must belong to the
/// manager (a worker must not be able to name someone else's turn as its authority); with no
/// id we fall back to whatever turn the manager is on right now; and if no user text can be
/// read back off that turn, the source is labelled `assignment_text_fallback` rather than
/// pretending the assignment text is the user's own words.
async fn source_of(
    app: &Arc<App>,
    manager_id: &str,
    source_turn_id: Option<&str>,
    text: &str,
) -> Result<(&'static str, Option<String>, String), LcError> {
    let conv = crate::db::conversation_id(&app.db, manager_id).await.map_err(up)?;
    let turn_id = match source_turn_id.map(str::trim).filter(|s| !s.is_empty()) {
        Some(t) => {
            let owned: Option<String> =
                sqlx::query_scalar("SELECT id FROM turns WHERE id=? AND conversation_id=?")
                    .bind(t)
                    .bind(&conv)
                    .fetch_optional(&app.db)
                    .await
                    .map_err(up)?;
            if owned.is_none() {
                return Err(LcError::Bad("source_turn_id is not a turn of the supervisor".into()));
            }
            Some(t.to_string())
        }
        None => {
            let run = crate::db::active_run(&app.db, manager_id).await.map_err(up)?;
            match run {
                Some(r) => crate::db::in_flight_turn(&app.db, &r.id).await.map_err(up)?.map(|t| t.id),
                None => None,
            }
        }
    };
    let Some(turn_id) = turn_id else {
        return Ok(("assignment_text_fallback", None, text.to_string()));
    };
    let user_text: Option<String> = sqlx::query_scalar(
        "SELECT content FROM messages WHERE turn_id=? AND role='user' ORDER BY created_at ASC LIMIT 1",
    )
    .bind(&turn_id)
    .fetch_optional(&app.db)
    .await
    .map_err(up)?;
    match user_text.filter(|s| !s.trim().is_empty()) {
        Some(t) => Ok(("manager_turn", Some(turn_id), t)),
        // The turn is real, so it is still the right dedupe key; we just do not have the
        // user's wording, and the record says so.
        None => Ok(("assignment_text_fallback", Some(turn_id), text.to_string())),
    }
}

/// `GET /api/supervisor/state` — what the manager is allowed to see about the world.
///
/// Sanitized on purpose: no env, no hook tokens, no args (an identity's args can name config
/// directories), no persona text. Enough to pick a bot; nothing that helps exfiltrate an account.
pub async fn sanitized_state(app: &Arc<App>) -> Result<Value, LcError> {
    let projects = crate::db::live_projects(&app.db).await.map_err(up)?;
    let bots = crate::db::live_bots(&app.db).await.map_err(up)?;
    let sup = store::get_or_init(&app.db).await.map_err(up)?;
    let hosts: std::collections::HashMap<String, String> =
        projects.iter().map(|p| (p.id.clone(), p.host.clone())).collect();
    let mut connected: std::collections::HashSet<String> = Default::default();
    for h in hosts.values() {
        if app.host_connected(h).await {
            connected.insert(h.clone());
        }
    }
    let mut out = Vec::new();
    for b in &bots {
        let run = crate::db::active_run(&app.db, &b.id).await.map_err(up)?;
        // Work already waiting on this bot: a bot with a queue is available, but not free.
        let queued: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM turns WHERE conversation_id=(SELECT id FROM conversations WHERE bot_id=?)
               AND status='queued'",
        )
        .bind(&b.id)
        .fetch_one(&app.db)
        .await
        .unwrap_or(0);
        out.push(json!({
            "id": b.id,
            "project_id": b.project_id,
            "name": b.name,
            "kind": b.kind,
            "model": b.model,
            "effort": b.effort,
            "identity": b.identity,
            "managed_by": b.managed_by,
            "team_id": b.team_id,
            "team_role": b.team_role,
            "parent_bot_id": b.parent_bot_id,
            "is_supervisor": Some(&b.id) == sup.bot_id.as_ref(),
            "cwd": b.cwd,
            "host": hosts.get(&b.project_id).cloned(),
            "host_connected": hosts.get(&b.project_id).map(|h| connected.contains(h)),
            "queued_turns": queued,
            "run": run.as_ref().map(|r| json!({
                "id": r.id,
                "state": r.state,
                // The lamp the sidebar shows: idle / working / blocked / unknown.
                "agent_status": r.agent_status,
                "agent_title": r.agent_title,
                // Whether the original conversation can be resumed at all, and on what the
                // CLI is *actually* running — which is not always what the bot is configured with.
                "native_session_id": r.native_session_id,
                "runtime_model": r.runtime_model,
                "runtime_effort": r.runtime_effort,
                "pane_id": r.pane_id,
                "started_at": r.started_at,
            })),
        }));
    }
    Ok(json!({
        "supervisor_id": store::SUPERVISOR_ID,
        "projects": projects.iter().map(|p| json!({
            "id": p.id, "label": p.label, "path": p.path, "host": p.host,
        })).collect::<Vec<_>>(),
        "bots": out,
        // Everything still owed, `awaiting_review` included: the point of the acceptance state
        // is that a finished turn stays in front of the manager until it decides.
        "open_assignments": store::unsettled_assignments(&app.db).await.map_err(up)?
            .iter().map(store::Assignment::to_json).collect::<Vec<_>>(),
        "pending_inbox": store::pending_inbox(&app.db).await.map_err(up)?
            .iter().map(store::InboxEvent::to_json).collect::<Vec<_>>(),
        "open_incidents": store::open_incidents(&app.db).await.map_err(up)?
            .iter().map(store::Incident::to_json).collect::<Vec<_>>(),
    }))
}
