//! Approvals and execution leases for rebuild / restart windows.
//!
//! Before this, "AGM said yes" lived in a chat message and the safety check was a snapshot: the
//! runtime update script read `working` bots once, decided the coast was clear, and then spent
//! several minutes building and swapping a binary during which anything could start. Two bots
//! could both be told "go when it is quiet" and both believe it was quiet.
//!
//! So a window has two halves, and they are deliberately separate calls:
//!
//! 1. **Wait for a safe window** — [`safety`] is a read. Poll it as often as you like; it
//!    promises nothing about the next second.
//! 2. **Take the window** — [`acquire`] re-checks the same conditions *and* takes the lease in
//!    one locked step, so nothing can slip in between the check and the hold. While a `restart`
//!    lease is held new assignments are not dispatched (they stay queued), which is the half
//!    the snapshot approach could never do.
//!
//! The honest limit, stated because it cannot be fixed here: an arbitrary shell on this machine
//! can still `kill` the daemon without asking anybody. The lease binds the paths that go through
//! this API and the operational scripts in `scripts/ops/`; it is not an OS-level mutex.

use crate::lifecycle::LcError;
use crate::state::App;
use serde_json::{json, Value};
use std::sync::Arc;

use super::store;

/// Resources a lease can be taken on. Anything else is refused: a typo must not silently create
/// a private lock that protects nothing.
pub const RESOURCES: [&str; 2] = ["rebuild", "restart"];

/// Holding this one pauses assignment dispatch. A rebuild does not interrupt anybody; a restart
/// does, and handing a bot new work while waiting to kill its session is the race this closes.
pub const EXCLUSIVE: [&str; 1] = ["restart"];

/// Default and maximum lease lifetime. Long enough for a release build and a restart, short
/// enough that a crashed holder does not block the next window for an afternoon.
pub const DEFAULT_TTL_SECS: i64 = 900;
pub const MAX_TTL_SECS: i64 = 3600;

fn iso_in(secs: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::seconds(secs)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Is a restart window currently held by someone? Used by the dispatcher to hold work back.
pub async fn dispatch_paused(app: &Arc<App>) -> Option<String> {
    for resource in EXCLUSIVE {
        if let Ok(Some(l)) = store::lease(&app.db, resource).await {
            if l.held_at(&crate::db::now()) {
                return l.expires_at;
            }
        }
    }
    None
}

/// What is going on that a restart would interrupt.
///
/// `blocked` panes are counted but are *not* a reason to refuse: a pane waiting for a human can
/// wait for hours, and the restart path skips blocked panes anyway. What blocks a window is work
/// actually running.
pub async fn safety(app: &Arc<App>, exclude: &[String]) -> Result<Value, LcError> {
    let bots = crate::db::live_bots(&app.db).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    let sup = store::get_or_init(&app.db).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    let mut working = Vec::new();
    let mut blocked = Vec::new();
    let mut in_flight = Vec::new();
    for b in &bots {
        if exclude.contains(&b.id) {
            continue;
        }
        let Ok(Some(run)) = crate::db::active_run(&app.db, &b.id).await else { continue };
        // AGM's own turn is protected by the same rule as everyone's: you do not restart the
        // daemon out from under the session the user is talking to.
        if run.agent_status == "working" {
            working.push(json!({"bot_id": b.id, "name": b.name, "is_supervisor": Some(&b.id) == sup.bot_id.as_ref()}));
        } else if run.agent_status == "blocked" {
            blocked.push(json!({"bot_id": b.id, "name": b.name}));
        }
        if let Ok(Some(t)) = crate::db::in_flight_turn(&app.db, &run.id).await {
            in_flight.push(json!({"bot_id": b.id, "turn_id": t.id}));
        }
    }
    let open = store::open_assignments(&app.db).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    let safe = working.is_empty() && in_flight.is_empty();
    Ok(json!({
        "safe": safe,
        "working": working,
        "in_flight": in_flight,
        // Reported, not blocking: a pane waiting on a person is a normal state, and a restart
        // leaves it alone.
        "blocked_waiting_for_user": blocked,
        // Queued work is not a reason to refuse either — it is exactly what the pause holds
        // back — but the caller should see how much is waiting on the other side of the window.
        "queued_assignments": open.iter().filter(|a| a.status == "queued").count(),
        "checked_at": crate::db::now(),
    }))
}

/// Take a window: approval checked, safety re-checked and the lease taken, all under the
/// supervisor lock so nothing changes between the check and the hold.
pub async fn acquire(
    app: &Arc<App>,
    resource: &str,
    owner: &str,
    approval_id: &str,
    commit: Option<&str>,
    ttl_secs: i64,
    require_idle: bool,
    exclude: &[String],
) -> Result<Value, LcError> {
    if !RESOURCES.contains(&resource) {
        return Err(LcError::Bad(format!("unknown lease resource: {resource}")));
    }
    let ttl = ttl_secs.clamp(30, MAX_TTL_SECS);
    let _g = super::lock().await;

    let approval = store::approval(&app.db, approval_id)
        .await
        .map_err(|e| LcError::Upstream(e.to_string()))?
        .ok_or_else(|| LcError::NotFound("approval".into()))?;
    if let Some(reason) = approval.refusal(&crate::db::now(), resource, commit) {
        return Err(LcError::conflict(
            "the approval does not cover this operation",
            json!({"reason": reason, "approval_id": approval.id, "status": approval.status,
                   "target_commit": approval.target_commit, "requested_commit": commit}),
        ));
    }

    let safety = safety(app, exclude).await?;
    if require_idle && safety.get("safe") != Some(&Value::Bool(true)) {
        return Err(LcError::conflict(
            "something is still running; wait for a safe window",
            json!({"reason": "not_idle", "safety": safety}),
        ));
    }

    let taken = store::acquire_lease(
        &app.db,
        resource,
        owner,
        Some(&approval.id),
        commit,
        &iso_in(ttl),
        &json!({"require_idle": require_idle, "safety": safety}),
    )
    .await
    .map_err(|e| LcError::Upstream(e.to_string()))?;

    let Some(lease) = taken else {
        let held = store::lease(&app.db, resource).await.map_err(|e| LcError::Upstream(e.to_string()))?;
        return Err(LcError::conflict(
            "someone else holds this window",
            json!({"reason": "lease_held", "lease": held.map(|l| l.to_json())}),
        ));
    };
    tracing::info!(resource, owner, fence = lease.fence, "maintenance lease acquired");
    app.emit("supervisor_changed", json!({"lease": lease.to_json()})).await;
    Ok(json!({"lease": lease.to_json(), "approval": approval.to_json(), "safety": safety}))
}

/// Dispatch is paused; keep the assignment queued until the window closes rather than sending
/// work into a session that is about to be restarted.
pub fn pause_note(until: &str) -> String {
    format!("restart window held until {until}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::store::Approval;

    fn approval(status: &str, purpose: &str, commit: Option<&str>, expires: Option<&str>) -> Approval {
        Approval {
            id: "ap1".into(),
            supervisor_id: "AGM".into(),
            requester: "bot-x".into(),
            purpose: purpose.into(),
            scope: "daemon".into(),
            target_commit: commit.map(str::to_string),
            status: status.into(),
            decided_by: Some("AGM".into()),
            decided_at: Some("2026-09-12T00:00:00Z".into()),
            reason: None,
            expires_at: expires.map(str::to_string),
            created_at: "2026-09-12T00:00:00Z".into(),
            updated_at: "2026-09-12T00:00:00Z".into(),
        }
    }

    const NOW: &str = "2026-09-12T12:00:00Z";

    #[test]
    fn only_an_approved_unexpired_approval_for_this_commit_counts() {
        let ok = approval("approved", "rebuild", Some("abc123"), Some("2026-09-12T13:00:00Z"));
        assert_eq!(ok.refusal(NOW, "rebuild", Some("abc123")), None);
        // The same yes does not carry to a different tree, a different operation, or tomorrow.
        assert_eq!(ok.refusal(NOW, "rebuild", Some("def456")), Some("approval_commit_mismatch"));
        assert_eq!(ok.refusal(NOW, "restart", Some("abc123")), Some("approval_purpose_mismatch"));
        assert_eq!(ok.refusal(NOW, "rebuild", None), Some("approval_commit_required"));
        let expired = approval("approved", "rebuild", Some("abc123"), Some("2026-09-12T11:00:00Z"));
        assert_eq!(expired.refusal(NOW, "rebuild", Some("abc123")), Some("approval_expired"));
    }

    #[test]
    fn a_revoked_or_undecided_approval_is_not_a_yes() {
        for (status, why) in [
            ("pending", "approval_not_decided"),
            ("denied", "approval_denied"),
            ("revoked", "approval_revoked"),
            ("consumed", "approval_already_used"),
        ] {
            let a = approval(status, "rebuild", None, None);
            assert_eq!(a.refusal(NOW, "rebuild", None), Some(why), "{status}");
        }
        // No deadline is allowed — it is the approval's author's choice — and no commit means
        // the approval was written for a resource, not a tree.
        assert_eq!(approval("approved", "rebuild", None, None).refusal(NOW, "rebuild", Some("x")), None);
    }

    #[test]
    fn a_restart_window_pauses_dispatch_but_a_rebuild_does_not() {
        assert!(EXCLUSIVE.contains(&"restart"));
        assert!(!EXCLUSIVE.contains(&"rebuild"));
        assert!(pause_note("2026-09-12T12:15:00Z").contains("12:15"));
    }
}
