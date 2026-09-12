//! The phone entry point (Claude Remote Control), and what can honestly be said about it.
//!
//! AGM is the user's only way in from a phone, so whether that entry point works is a service
//! property the daemon should be able to report. The 2026-09-12 review found it could not: the
//! bot's args carry `--remote-control AGM`, which is a *request*, and nothing ever checked
//! whether a session came up.
//!
//! **The finding after looking for one: there is no reliable observation source.** The daemon
//! learns about a CLI through herdr panes, hooks and transcripts; none of those carry Remote
//! Control session state, and neither the hook payloads nor the session rows mention it (the
//! only `--remote-control` in the tree is the argument `setup` writes). A URL scraped out of
//! pane text would be a string that proves nothing about whether a phone can reach it.
//!
//! So the capability is reported as `unsupported` and the status stays `requested` or
//! `unknown`. Nothing here will ever set `active`/`verified` from argv, from a bot's own words,
//! or from a URL — a person can confirm it by hand ([`Source::Manual`]), and that confirmation
//! is recorded as a person's claim, with who made it and when, and it expires.
//!
//! When a provider observation does become available, [`Source::Provider`] is where it goes and
//! [`capability`] is the one place that has to change.

use crate::state::App;
use serde_json::{json, Value};
use std::sync::Arc;

use super::store;

/// How long an observation counts for. After this it is `unknown` again: "somebody checked an
/// hour ago" is not "it works now".
pub const OBSERVATION_TTL_SECS: i64 = 900;

/// Where a claim about the remote entry point came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The launch arguments asked for it. Evidence that we *requested* a session, nothing more.
    Argv,
    /// A person says they reached it. Recorded with an actor, and it expires.
    Manual,
    /// The provider told us. Not currently available — see the module docs.
    Provider,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Argv => "argv",
            Source::Manual => "manual",
            Source::Provider => "provider",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "argv" => Some(Source::Argv),
            "manual" => Some(Source::Manual),
            "provider" => Some(Source::Provider),
            _ => None,
        }
    }

    /// Only these may claim the entry point actually works. `argv` never can: that is the whole
    /// bug this module exists to fix.
    pub fn can_verify(self) -> bool {
        matches!(self, Source::Manual | Source::Provider)
    }
}

/// Public callers may record an attributed manual check, never manufacture provider evidence.
pub fn validate_external_source(source: Source) -> Result<(), &'static str> {
    match source {
        Source::Manual => Ok(()),
        Source::Argv => Err("argv is daemon bookkeeping, not an external observation"),
        Source::Provider => Err("provider observations are unsupported; use an attributed manual check"),
    }
}

/// Whether the daemon has any way to observe the remote entry point on this deployment.
///
/// `unsupported` is a statement about our observation ability, not about the phone: the session
/// may well be working. What we must not do is claim either way without evidence.
pub fn capability() -> Value {
    json!({
        "status": "unsupported",
        "reason": "no provider-backed observation source for Remote Control sessions on this deployment",
        "checked": ["herdr pane state", "hook payloads", "session rows", "bot args"],
        "manual_confirmation": true,
    })
}

/// The four states. `requested` is what argv buys you; `verified` needs a source that
/// [`Source::can_verify`]; `unavailable` needs evidence of failure, not the absence of evidence
/// of success; everything else — including every expired observation — is `unknown`.
pub const STATES: [&str; 4] = ["requested", "verified", "unavailable", "unknown"];

/// Why a stored status no longer holds. `None` = it still does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Revoked {
    /// The session the observation was made against is gone or has been replaced.
    SessionChanged,
    /// The observation is older than [`OBSERVATION_TTL_SECS`].
    Expired,
    /// Nothing is running, so there is no entry point to have an opinion about.
    NotRunning,
}

impl Revoked {
    pub fn as_str(self) -> &'static str {
        match self {
            Revoked::SessionChanged => "session_changed",
            Revoked::Expired => "observation_expired",
            Revoked::NotRunning => "manager_not_running",
        }
    }
}

/// Decide what a stored observation is still worth. Pure, so the expiry and session rules are
/// testable without a database.
pub fn revocation(
    stored_status: &str,
    observed_at: Option<&str>,
    observed_session: Option<&str>,
    current_session: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<Revoked> {
    if stored_status == "unknown" {
        return None;
    }
    let Some(current) = current_session else { return Some(Revoked::NotRunning) };
    // A restart or a model switch opens a *new* session; whatever was true of the old one says
    // nothing about this one.
    if observed_session.is_some_and(|s| s != current)
        || (stored_status != "requested" && observed_session.is_none()) {
        return Some(Revoked::SessionChanged);
    }
    // `requested` is tied to the session, not to a clock: the argument was passed when this
    // session started and stays true for as long as it runs.
    if stored_status == "requested" {
        return None;
    }
    let Some(at) = observed_at else { return Some(Revoked::Expired) };
    let Ok(at) = chrono::DateTime::parse_from_rfc3339(at) else { return Some(Revoked::Expired) };
    let age = now.signed_duration_since(at.with_timezone(&chrono::Utc)).num_seconds();
    (age < 0 || age >= OBSERVATION_TTL_SECS).then_some(Revoked::Expired)
}

/// The remote entry point as it should be reported right now, applying expiry and session
/// binding to whatever is stored.
pub async fn status(app: &Arc<App>) -> Value {
    let Ok(sup) = store::get_or_init(&app.db).await else {
        return json!({"status": "unknown", "capability": capability()});
    };
    let run = match sup.bot_id.as_deref() {
        Some(id) => crate::db::active_run(&app.db, id).await.ok().flatten(),
        None => None,
    };
    // The run id is the session identity we can actually see. `native_session_id` is carried
    // too, because a resume keeps the conversation but is still a new process.
    let current = run.as_ref().map(|r| r.id.clone());
    let revoked = revocation(
        &sup.remote_status,
        sup.remote_observed_at.as_deref(),
        sup.remote_session_id.as_deref(),
        current.as_deref(),
        chrono::Utc::now(),
    );
    let effective = if revoked.is_some() { "unknown" } else { sup.remote_status.as_str() };
    json!({
        "status": effective,
        "stored_status": sup.remote_status,
        "revoked": revoked.map(Revoked::as_str),
        "source": sup.remote_source,
        "observed_at": sup.remote_observed_at,
        "observed_by": sup.remote_actor,
        "session_id": sup.remote_session_id,
        "current_session_id": current,
        // Present only when an observation carried one. A URL is not a connection: it is never
        // treated as evidence that a phone reached anything.
        "url": if revoked.is_some() { None } else { sup.remote_url },
        "url_is_evidence": false,
        "capability": capability(),
        "ttl_secs": OBSERVATION_TTL_SECS,
    })
}

/// Incident severity for the remote entry point.
///
/// Only an *observed* failure counts. `unknown` with an unsupported capability is a documented
/// limit, not a fault, and opening an incident for it would mean a permanent red light nobody
/// can clear.
pub fn severity(status: &str) -> &'static str {
    match status {
        "unavailable" => "degraded",
        "unknown" => "unknown",
        _ => "healthy",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-09-12T12:00:00Z").unwrap().with_timezone(&chrono::Utc)
    }

    /// Launch arguments buy `requested` and nothing else. This is the actual bug: argv was the
    /// only input and the status it produced was being read as "the phone is connected".
    #[test]
    fn argv_can_never_verify_anything() {
        assert!(!Source::Argv.can_verify());
        assert!(Source::Manual.can_verify());
        assert!(Source::Provider.can_verify());
        assert_eq!(capability()["status"], "unsupported");
        assert!(!STATES.contains(&"active"), "there is no `active`: the honest states are requested/verified/unavailable/unknown");
    }

    #[test]
    fn external_callers_cannot_claim_provider_evidence() {
        assert!(validate_external_source(Source::Manual).is_ok());
        assert!(validate_external_source(Source::Provider).is_err());
        assert!(validate_external_source(Source::Argv).is_err());
        assert_eq!(revocation("verified", Some("2026-09-12T11:55:00Z"), None, Some("new-run"), now()),
                   Some(Revoked::SessionChanged));
    }

    /// A `requested` status lives and dies with its session, and needs no clock: the argument
    /// was passed at start and stays passed.
    #[test]
    fn requested_is_tied_to_the_session_not_to_a_timer() {
        assert_eq!(revocation("requested", None, Some("run-1"), Some("run-1"), now()), None);
        assert_eq!(
            revocation("requested", None, Some("run-1"), Some("run-2"), now()),
            Some(Revoked::SessionChanged),
            "a restart is a new session; the old request says nothing about it"
        );
        assert_eq!(revocation("requested", None, Some("run-1"), None, now()), Some(Revoked::NotRunning));
    }

    /// A verification is a snapshot. Fifteen minutes later it is `unknown` again, because
    /// "somebody reached it earlier" is not "it works now".
    #[test]
    fn a_verification_expires_and_does_not_survive_a_new_session() {
        let fresh = "2026-09-12T11:55:00Z";
        let stale = "2026-09-12T11:40:00Z";
        assert_eq!(revocation("verified", Some(fresh), Some("run-1"), Some("run-1"), now()), None);
        assert_eq!(revocation("verified", Some(stale), Some("run-1"), Some("run-1"), now()), Some(Revoked::Expired));
        assert_eq!(
            revocation("verified", Some(fresh), Some("run-1"), Some("run-9"), now()),
            Some(Revoked::SessionChanged)
        );
        // A clock that jumped, or no timestamp at all, is not evidence either.
        assert_eq!(revocation("verified", None, Some("run-1"), Some("run-1"), now()), Some(Revoked::Expired));
        assert_eq!(
            revocation("verified", Some("2026-09-12T13:00:00Z"), Some("run-1"), Some("run-1"), now()),
            Some(Revoked::Expired)
        );
    }

    #[test]
    fn not_knowing_is_not_a_fault_but_an_observed_failure_is() {
        assert_eq!(severity("unknown"), "unknown");
        assert_eq!(severity("requested"), "healthy");
        assert_eq!(severity("verified"), "healthy");
        assert_eq!(severity("unavailable"), "degraded");
    }
}
