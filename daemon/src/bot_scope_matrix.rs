//! 一般 bot principal 對「全部路由」的範圍表（#790–#812 那一族）。
//!
//! 路由從 [`crate::api::router`] 的 Debug 列出來，不手抄。每條用 Bot A 的 token 打，
//! 路徑上的 bot／project 屬於 Bot B（沒有列的 id 用固定的外國 id）。
//! 允許表沒有這一列、或多一列沒打到，測試就失敗。
//!
//! `open_leak`：政策是 403，但這一輪實測仍放行（不是 401／403／404）。
//! `Some("")` 是還沒有票的洩漏。已決定允許的例外用 `allow_rule` 附理由；
//! 已封鎖的列寫成 `UserOnly`（403 `user_only`）、`RoleRequired`（403 `role_required`）、
//! `Forbidden`、`Unauthorized` 或 404，不再標洩漏。
//! bot 自己的 hook、relay、build-slots，以及跨 bot 的 `POST /api/bots/{id}/prompt`，表上是允許。

use super::*;
use axum::http::StatusCode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Expect {
    /// 允許：不是 401／403／404（含 2xx、400、409、415、422、5xx）。
    Allow,
    /// 403，不限 reason（資源閘或尚未細分）。
    Forbidden,
    /// 403，body reason 是 `user_only`。
    UserOnly,
    /// 403，body reason 是 `role_required`。
    RoleRequired,
    /// 401; the websocket handshake requires the UI token.
    Unauthorized,
    /// 404。
    NotFound,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Decision {
    Allowed,
    Denied,
    Pending,
}

struct Rule {
    method: &'static str,
    /// router 上的樣板，例如 `/api/bots/{id}`。
    path: &'static str,
    expect: Expect,
    /// `Some(票號)` 或 `Some("")`（無票）＝政策 403，現況仍允許。
    open_leak: Option<&'static str>,
    /// Explicit reason for a reviewed Bot allow or a still-pending decision.
    rationale: Option<&'static str>,
    /// Policy decision may differ from this Bot-A-to-B probe's expected response for scoped access.
    decision: Option<Decision>,
}

fn rule(method: &'static str, path: &'static str, expect: Expect, open_leak: Option<&'static str>) -> Rule {
    Rule { method, path, expect, open_leak, rationale: None, decision: None }
}

fn allow_rule(method: &'static str, path: &'static str, rationale: &'static str) -> Rule {
    Rule { method, path, expect: Expect::Allow, open_leak: None, rationale: Some(rationale), decision: Some(Decision::Allowed) }
}

/// The route is available to ordinary Bots only within the named scope; the Bot-A-to-B probe must deny.
fn scoped_allow_rule(method: &'static str, path: &'static str, rationale: &'static str) -> Rule {
    Rule { method, path, expect: Expect::Forbidden, open_leak: None, rationale: Some(rationale), decision: Some(Decision::Allowed) }
}

/// The route is intentionally limited to a different principal class, so an ordinary Bot must deny.
fn principal_allow_rule(method: &'static str, path: &'static str, rationale: &'static str) -> Rule {
    Rule { method, path, expect: Expect::Forbidden, open_leak: None, rationale: Some(rationale), decision: Some(Decision::Allowed) }
}

fn deny_rule(method: &'static str, path: &'static str, expect: Expect, rationale: &'static str) -> Rule {
    Rule { method, path, expect, open_leak: None, rationale: Some(rationale), decision: Some(Decision::Denied) }
}

fn pending_rule(method: &'static str, path: &'static str, ticket: &'static str, rationale: &'static str) -> Rule {
    Rule { method, path, expect: Expect::Forbidden, open_leak: Some(ticket), rationale: Some(rationale), decision: Some(Decision::Pending) }
}

fn rules() -> Vec<Rule> {
    vec![
        deny_rule("GET", "/api/session", Expect::Forbidden, "This loopback bootstrap returns the shared UI token to browsers; Bot and Service principals must not receive it."),
        rule("GET", "/api/bots/{id}/credential/rotate", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/credential/rotate", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/credential/rotate", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/credential/rotate", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/credential/rotate", Expect::Forbidden, None),
        rule("GET", "/api/supervisor", Expect::RoleRequired, None),
        principal_allow_rule("POST", "/api/services/daemon-swap/restart-window", "This operation is allowed only to the daemon-swap Service principal; a Bot credential must be rejected."),
        rule("GET", "/api/missions/{id}/pick", Expect::Forbidden, None),
        rule("GET", "/api/fs/dirs", Expect::Forbidden, None),
        rule("GET", "/api/supervisor/persona", Expect::RoleRequired, None),
        deny_rule("PUT", "/api/supervisor/persona", Expect::RoleRequired, "Persona is supervisory configuration; only User or a registered AGM role may change it."),
        rule("GET", "/api/judge/shadow", Expect::UserOnly, None),
        rule("POST", "/api/services/daemon-swap/probe/{id}", Expect::Forbidden, None),
        rule("GET", "/api/bots/{id}/terminal", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/terminal", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/terminal", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/terminal", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/terminal", Expect::Forbidden, None),
        rule("GET", "/api/supervisor/approvals", Expect::RoleRequired, None),
        allow_rule("POST", "/api/supervisor/approvals", "A Bot may request its own approval; requester_claim binds the request to that Bot and decisions remain AGM-role-only (#799)."),
        rule("POST", "/api/hosts/{name}/identities/{identity}/login", Expect::Forbidden, None),
        rule("GET", "/api/bots/{id}/scratchpad", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/scratchpad", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/scratchpad", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/scratchpad", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/scratchpad", Expect::Forbidden, None),
        rule("GET", "/api/bots/{id}/attachments", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/attachments", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/attachments", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/attachments", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/attachments", Expect::Forbidden, None),
        rule("GET", "/api/claude-update/review", Expect::UserOnly, None),
        rule("POST", "/api/claude-update/review", Expect::UserOnly, None),
        rule("POST", "/relay/spawn/abort", Expect::Allow, None),
        rule("GET", "/api/bots/{id}/local-image", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/local-image", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/local-image", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/local-image", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/local-image", Expect::Forbidden, None),
        rule("GET", "/api/bots/{id}/abort", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/abort", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/abort", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/abort", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/abort", Expect::Forbidden, None),
        rule("GET", "/api/build/remote", Expect::UserOnly, None),
        rule("PUT", "/api/build/remote", Expect::UserOnly, None),
        rule("POST", "/relay/spawn/begin", Expect::Allow, None),
        rule("GET", "/api/bots/{id}/login", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/login", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/login", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/login", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/login", Expect::Forbidden, None),
        rule("GET", "/api/bots/{id}", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}", Expect::Forbidden, None),
        rule("GET", "/api/supervisor/ops-alerts", Expect::Forbidden, None),
        rule("POST", "/api/supervisor/ops-alerts", Expect::Forbidden, None),
        rule("PUT", "/api/supervisor/ops-alerts", Expect::Forbidden, None),
        rule("PATCH", "/api/supervisor/ops-alerts", Expect::Forbidden, None),
        rule("DELETE", "/api/supervisor/ops-alerts", Expect::Forbidden, None),
        rule("DELETE", "/api/hosts/{name}", Expect::Forbidden, None),
        rule("POST", "/build-slots/renew", Expect::Allow, None),
        rule("GET", "/api/projects/{id}/issues", Expect::Forbidden, None),
        rule("POST", "/api/projects/{id}/issues", Expect::Forbidden, None),
        rule("PUT", "/api/projects/{id}/issues", Expect::Forbidden, None),
        rule("PATCH", "/api/projects/{id}/issues", Expect::Forbidden, None),
        rule("DELETE", "/api/projects/{id}/issues", Expect::Forbidden, None),
        rule("POST", "/api/hosts/{name}/shells/{pane_id}/keys", Expect::Forbidden, None),
        rule("GET", "/api/projects/{id}/missions", Expect::Forbidden, None),
        rule("POST", "/api/projects/{id}/missions", Expect::Forbidden, None),
        rule("PUT", "/api/projects/{id}/missions", Expect::Forbidden, None),
        rule("PATCH", "/api/projects/{id}/missions", Expect::Forbidden, None),
        rule("DELETE", "/api/projects/{id}/missions", Expect::Forbidden, None),
        rule("GET", "/api/bots/{id}/pending-question", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/pending-question", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/pending-question", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/pending-question", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/pending-question", Expect::Forbidden, None),
        allow_rule("GET", "/api/capabilities", "Returns only the daemon's static capability names; no user, bot, or host state is exposed."),
        rule("POST", "/api/quota/probe", Expect::UserOnly, None),
        rule("GET", "/api/supervisor/approvals/{id}/decide", Expect::Forbidden, None),
        rule("POST", "/api/supervisor/approvals/{id}/decide", Expect::Forbidden, None),
        rule("PUT", "/api/supervisor/approvals/{id}/decide", Expect::Forbidden, None),
        rule("PATCH", "/api/supervisor/approvals/{id}/decide", Expect::Forbidden, None),
        rule("DELETE", "/api/supervisor/approvals/{id}/decide", Expect::Forbidden, None),
        rule("POST", "/relay/pane", Expect::Allow, None),
        rule("GET", "/api/supervisor/setup", Expect::Forbidden, None),
        rule("POST", "/api/supervisor/setup", Expect::Forbidden, None),
        rule("PUT", "/api/supervisor/setup", Expect::Forbidden, None),
        rule("PATCH", "/api/supervisor/setup", Expect::Forbidden, None),
        rule("DELETE", "/api/supervisor/setup", Expect::Forbidden, None),
        rule("GET", "/api/supervisor/inbox", Expect::RoleRequired, None),
        allow_rule("GET", "/api/models", "Ordinary Bots can read a fresh cached model snapshot only; a miss/stale cache cannot start a CLI probe (#809)."),
        rule("POST", "/api/build/remote/test", Expect::UserOnly, None),
        rule("POST", "/api/hosts/{name}/tools/refresh", Expect::Forbidden, None),
        rule("GET", "/api/bots/{id}/rewind", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/rewind", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/rewind", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/rewind", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/rewind", Expect::Forbidden, None),
        rule("GET", "/api/projects/{id}", Expect::Forbidden, None),
        rule("POST", "/api/projects/{id}", Expect::Forbidden, None),
        rule("PUT", "/api/projects/{id}", Expect::Forbidden, None),
        rule("PATCH", "/api/projects/{id}", Expect::Forbidden, None),
        rule("DELETE", "/api/projects/{id}", Expect::Forbidden, None),
        rule("POST", "/api/supervisor/inbox/{id}/ack", Expect::Forbidden, None),
        rule("GET", "/api/supervisor/leases", Expect::RoleRequired, None),
        rule("GET", "/api/projects/{id}/git/pull", Expect::Forbidden, None),
        rule("POST", "/api/projects/{id}/git/pull", Expect::Forbidden, None),
        rule("PUT", "/api/projects/{id}/git/pull", Expect::Forbidden, None),
        rule("PATCH", "/api/projects/{id}/git/pull", Expect::Forbidden, None),
        rule("DELETE", "/api/projects/{id}/git/pull", Expect::Forbidden, None),
        rule("POST", "/api/missions/{id}/pause", Expect::UserOnly, None),
        deny_rule("GET", "/api/deploy/status", Expect::UserOnly, "The response includes global Bot activity, deployment repository state, and log paths; it is for the User UI."),
        deny_rule("POST", "/api/deploy/wait/escalate", Expect::UserOnly, "Relaxing a waiting deployment is the user's dispatch decision (SPEC 18.10); AGM must ask the user, not press it."),
        deny_rule("POST", "/api/deploy/wait/dismiss", Expect::UserOnly, "Hiding the deploy-wait notice is the user's own UI action (SPEC 18.10)."),
        rule("GET", "/api/supervisor/incidents", Expect::RoleRequired, None),
        rule("POST", "/api/hosts/{name}/tools/install", Expect::UserOnly, None),
        rule("GET", "/api/supervisor/responder/start", Expect::Forbidden, None),
        rule("POST", "/api/supervisor/responder/start", Expect::Forbidden, None),
        rule("PUT", "/api/supervisor/responder/start", Expect::Forbidden, None),
        rule("PATCH", "/api/supervisor/responder/start", Expect::Forbidden, None),
        rule("DELETE", "/api/supervisor/responder/start", Expect::Forbidden, None),
        rule("GET", "/api/supervisor/maintenance/safety", Expect::RoleRequired, None),
        rule("GET", "/api/bots/deleted", Expect::UserOnly, None),
        rule("GET", "/api/supervisor/build-inputs", Expect::RoleRequired, None),
        rule("GET", "/api/projects/{id}/chat", Expect::Forbidden, None),
        rule("POST", "/api/projects/{id}/chat", Expect::Forbidden, None),
        rule("PUT", "/api/projects/{id}/chat", Expect::Forbidden, None),
        rule("PATCH", "/api/projects/{id}/chat", Expect::Forbidden, None),
        rule("DELETE", "/api/projects/{id}/chat", Expect::Forbidden, None),
        rule("GET", "/api/supervisor/assignments/{id}/review", Expect::Forbidden, None),
        rule("POST", "/api/supervisor/assignments/{id}/review", Expect::Forbidden, None),
        rule("PUT", "/api/supervisor/assignments/{id}/review", Expect::Forbidden, None),
        rule("PATCH", "/api/supervisor/assignments/{id}/review", Expect::Forbidden, None),
        rule("DELETE", "/api/supervisor/assignments/{id}/review", Expect::Forbidden, None),
        rule("GET", "/api/intents", Expect::UserOnly, None),
        rule("POST", "/api/release-triage/dispatched", Expect::UserOnly, None),
        rule("GET", "/api/bots/{id}/preview", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/preview", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/preview", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/preview", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/preview", Expect::Forbidden, None),
        allow_rule("GET", "/api/identity-prefs", "Returns only disabled host/kind/identity preferences needed for identity selection; it contains no credentials (#806)."),
        rule("POST", "/api/panes/{id}/adopt", Expect::Forbidden, None),
        rule("GET", "/api/bots/{id}/restart", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/restart", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/restart", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/restart", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/restart", Expect::Forbidden, None),
        allow_rule("GET", "/api/state", "Bot state is projected to the caller and descendants, excluding other projects and global/private configuration (#807)."),
        pending_rule("POST", "/api/release-triage/verdicts", "#801", "Release-triage verdict submission remains open pending the user's decision on binding it to an assigned Bot and generation."),
        rule("PUT", "/api/drafts/{key}", Expect::Forbidden, None),
        scoped_allow_rule("POST", "/api/supervisor/leases/{resource}/renew", "A Bot may renew its own lease, including an upgrade-era tokenless lease; it cannot renew Bot B's lease."),
        rule("POST", "/api/hosts/{name}/gh/cancel", Expect::Forbidden, None),
        rule("POST", "/api/supervisor/persona/adopt-embedded", Expect::Forbidden, None),
        rule("POST", "/build-slots/acquire", Expect::Allow, None),
        rule("POST", "/api/hosts/{name}/identities/{identity}/logout", Expect::Forbidden, None),
        rule("GET", "/api/projects/{id}/messages", Expect::Forbidden, None),
        rule("POST", "/api/projects/{id}/messages", Expect::Forbidden, None),
        rule("PUT", "/api/projects/{id}/messages", Expect::Forbidden, None),
        rule("PATCH", "/api/projects/{id}/messages", Expect::Forbidden, None),
        rule("DELETE", "/api/projects/{id}/messages", Expect::Forbidden, None),
        rule("GET", "/api/supervisor/responder/persona", Expect::RoleRequired, None),
        deny_rule("PUT", "/api/supervisor/responder/persona", Expect::RoleRequired, "Responder persona is supervisory configuration; only User or a registered AGM role may change it."),
        rule("GET", "/api/projects/{id}/issues/{number}", Expect::Forbidden, None),
        rule("POST", "/api/projects/{id}/issues/{number}", Expect::Forbidden, None),
        rule("PUT", "/api/projects/{id}/issues/{number}", Expect::Forbidden, None),
        rule("PATCH", "/api/projects/{id}/issues/{number}", Expect::Forbidden, None),
        rule("DELETE", "/api/projects/{id}/issues/{number}", Expect::Forbidden, None),
        rule("GET", "/api/drafts", Expect::Forbidden, None),
        allow_rule("GET", "/api/upstream-updates", "Returns the existing upstream update metadata snapshot; GET does not fetch or mutate remote state."),
        rule("GET", "/api/bots/{id}/promote", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/promote", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/promote", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/promote", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/promote", Expect::Forbidden, None),
        rule("GET", "/api/supervisor/handoff", Expect::RoleRequired, None),
        rule("POST", "/api/supervisor/handoff", Expect::Forbidden, None),
        rule("PUT", "/api/supervisor/handoff", Expect::Forbidden, None),
        rule("PATCH", "/api/supervisor/handoff", Expect::Forbidden, None),
        rule("DELETE", "/api/supervisor/handoff", Expect::Forbidden, None),
        scoped_allow_rule("POST", "/api/missions/{id}/question", "A Bot may ask about a mission where it is assigned; this probe targets Bot B's mission and must be rejected."),
        rule("POST", "/api/missions/{id}/round", Expect::Forbidden, None),
        rule("GET", "/api/bots/{id}/restore", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/restore", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/restore", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/restore", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/restore", Expect::Forbidden, None),
        rule("POST", "/api/order", Expect::UserOnly, None),
        rule("GET", "/api/bots/{id}/outbox/file", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/outbox/file", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/outbox/file", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/outbox/file", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/outbox/file", Expect::Forbidden, None),
        rule("POST", "/api/panes/{id}/focus", Expect::Forbidden, None),
        rule("POST", "/api/identities", Expect::UserOnly, None),
        rule("GET", "/api/projects/{id}/git", Expect::Forbidden, None),
        rule("POST", "/api/projects/{id}/git", Expect::Forbidden, None),
        rule("PUT", "/api/projects/{id}/git", Expect::Forbidden, None),
        rule("PATCH", "/api/projects/{id}/git", Expect::Forbidden, None),
        rule("DELETE", "/api/projects/{id}/git", Expect::Forbidden, None),
        rule("GET", "/api/projects/{id}/panes", Expect::Forbidden, None),
        rule("POST", "/api/projects/{id}/panes", Expect::Forbidden, None),
        rule("PUT", "/api/projects/{id}/panes", Expect::Forbidden, None),
        rule("PATCH", "/api/projects/{id}/panes", Expect::Forbidden, None),
        rule("DELETE", "/api/projects/{id}/panes", Expect::Forbidden, None),
        rule("POST", "/api/projects", Expect::UserOnly, None),
        rule("POST", "/api/missions/{id}/resume", Expect::UserOnly, None),
        rule("GET", "/api/supervisor/responder", Expect::RoleRequired, None),
        scoped_allow_rule("POST", "/api/missions/{id}/events", "A Bot may report to a mission where it is assigned; verified events still require the verifier/gatekeeper role. This Bot-A-to-B probe must be rejected."),
        rule("GET", "/api/bots/{id}/fork", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/fork", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/fork", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/fork", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/fork", Expect::Forbidden, None),
        rule("GET", "/api/supervisor/assignments/{id}", Expect::RoleRequired, None),
        rule("GET", "/api/mem/processes", Expect::UserOnly, None),
        rule("GET", "/api/bots/{id}/stop", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/stop", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/stop", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/stop", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/stop", Expect::Forbidden, None),
        rule("POST", "/relay/spawn/finish", Expect::Allow, None),
        rule("GET", "/api/supervisor/start", Expect::Forbidden, None),
        rule("POST", "/api/supervisor/start", Expect::Forbidden, None),
        rule("PUT", "/api/supervisor/start", Expect::Forbidden, None),
        rule("PATCH", "/api/supervisor/start", Expect::Forbidden, None),
        rule("DELETE", "/api/supervisor/start", Expect::Forbidden, None),
        rule("POST", "/api/supervisor/herdr-maintenance/open", Expect::Forbidden, None),
        rule("GET", "/api/mem", Expect::UserOnly, None),
        rule("POST", "/api/release-triage/publish", Expect::UserOnly, None),
        rule("GET", "/api/projects/{id}/git/commit", Expect::Forbidden, None),
        rule("POST", "/api/projects/{id}/git/commit", Expect::Forbidden, None),
        rule("PUT", "/api/projects/{id}/git/commit", Expect::Forbidden, None),
        rule("PATCH", "/api/projects/{id}/git/commit", Expect::Forbidden, None),
        rule("DELETE", "/api/projects/{id}/git/commit", Expect::Forbidden, None),
        rule("GET", "/api/bots/{id}/pane/move-to-tab", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/pane/move-to-tab", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/pane/move-to-tab", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/pane/move-to-tab", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/pane/move-to-tab", Expect::Forbidden, None),
        scoped_allow_rule("POST", "/api/supervisor/leases/{resource}/release", "A Bot may release its own lease, including an upgrade-era tokenless lease; the owner Bot must not release Bot B's lease."),
        rule("POST", "/api/deploy/now", Expect::Forbidden, None),
        rule("POST", "/api/mem/processes/kill", Expect::UserOnly, None),
        deny_rule("GET", "/api/changelog", Expect::UserOnly, "This User UI route runs a local or remote CLI version check and must not be a Bot command-execution entry point."),
        rule("POST", "/relay/announce", Expect::Allow, None),
        rule("POST", "/api/build/remote/install-toolchain", Expect::UserOnly, None),
        deny_rule("GET", "/ws", Expect::Unauthorized, "The WebSocket carries global UI events and accepts the UI token; Bot credentials do not authorize a subscription."),
        rule("POST", "/api/hosts/{name}/gh/login", Expect::Forbidden, None),
        rule("PUT", "/api/identities/{name}/disabled", Expect::UserOnly, None),
        rule("POST", "/api/bots/restart-idle", Expect::UserOnly, None),
        rule("GET", "/api/search/messages", Expect::RoleRequired, None),
        rule("GET", "/api/bots/{id}/interrupt", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/interrupt", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/interrupt", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/interrupt", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/interrupt", Expect::Forbidden, None),
        rule("GET", "/api/mem/processes/pane", Expect::UserOnly, None),
        rule("GET", "/api/hosts/{name}/shells/{pane_id}/terminal", Expect::Forbidden, None),
        rule("GET", "/api/attachments/{id}", Expect::Forbidden, None),
        rule("GET", "/api/supervisor/stop", Expect::Forbidden, None),
        rule("POST", "/api/supervisor/stop", Expect::Forbidden, None),
        rule("PUT", "/api/supervisor/stop", Expect::Forbidden, None),
        rule("PATCH", "/api/supervisor/stop", Expect::Forbidden, None),
        rule("DELETE", "/api/supervisor/stop", Expect::Forbidden, None),
        rule("GET", "/api/bots/{id}/scratchpad/file", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/scratchpad/file", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/scratchpad/file", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/scratchpad/file", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/scratchpad/file", Expect::Forbidden, None),
        rule("GET", "/api/projects/{id}/git/push", Expect::Forbidden, None),
        rule("POST", "/api/projects/{id}/git/push", Expect::Forbidden, None),
        rule("PUT", "/api/projects/{id}/git/push", Expect::Forbidden, None),
        rule("PATCH", "/api/projects/{id}/git/push", Expect::Forbidden, None),
        rule("DELETE", "/api/projects/{id}/git/push", Expect::Forbidden, None),
        rule("GET", "/api/hosts/{name}/gh", Expect::Forbidden, None),
        rule("GET", "/api/bots/{id}/outbox", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/outbox", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/outbox", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/outbox", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/outbox", Expect::Forbidden, None),
        rule("GET", "/api/supervisor/evidence", Expect::RoleRequired, None),
        rule("DELETE", "/api/hosts/{name}/shells/{pane_id}", Expect::Forbidden, None),
        rule("POST", "/hook/{provider}", Expect::Allow, None),
        rule("GET", "/api/panes", Expect::Forbidden, None),
        rule("POST", "/build-slots/release", Expect::Allow, None),
        rule("POST", "/api/hosts/{name}/herdr-update", Expect::Forbidden, None),
        rule("GET", "/api/supervisor/remote", Expect::RoleRequired, None),
        rule("POST", "/api/supervisor/remote", Expect::Forbidden, None),
        rule("PUT", "/api/supervisor/remote", Expect::Forbidden, None),
        rule("PATCH", "/api/supervisor/remote", Expect::Forbidden, None),
        rule("DELETE", "/api/supervisor/remote", Expect::Forbidden, None),
        rule("GET", "/api/supervisor/cli", Expect::RoleRequired, None),
        rule("POST", "/api/supervisor/cli", Expect::Forbidden, None),
        rule("PUT", "/api/supervisor/cli", Expect::Forbidden, None),
        rule("PATCH", "/api/supervisor/cli", Expect::Forbidden, None),
        rule("DELETE", "/api/supervisor/cli", Expect::Forbidden, None),
        rule("GET", "/api/build-slots", Expect::UserOnly, None),
        rule("GET", "/api/judge/settings", Expect::UserOnly, None),
        rule("PUT", "/api/judge/settings", Expect::UserOnly, None),
        rule("GET", "/api/projects/{id}/github/refresh", Expect::Forbidden, None),
        rule("POST", "/api/projects/{id}/github/refresh", Expect::Forbidden, None),
        rule("PUT", "/api/projects/{id}/github/refresh", Expect::Forbidden, None),
        rule("PATCH", "/api/projects/{id}/github/refresh", Expect::Forbidden, None),
        rule("DELETE", "/api/projects/{id}/github/refresh", Expect::Forbidden, None),
        rule("DELETE", "/api/identities/{name}", Expect::UserOnly, None),
        scoped_allow_rule("POST", "/api/supervisor/leases/{resource}/acquire", "A Bot may acquire a lease only for an approved request owned by itself; it cannot acquire a lease for Bot B."),
        rule("POST", "/api/panes/{id}/close", Expect::Forbidden, None),
        rule("GET", "/api/supervisor/responder/stop", Expect::Forbidden, None),
        rule("POST", "/api/supervisor/responder/stop", Expect::Forbidden, None),
        rule("PUT", "/api/supervisor/responder/stop", Expect::Forbidden, None),
        rule("PATCH", "/api/supervisor/responder/stop", Expect::Forbidden, None),
        rule("DELETE", "/api/supervisor/responder/stop", Expect::Forbidden, None),
        rule("POST", "/api/turns/{id}/withdraw", Expect::NotFound, None),
        scoped_allow_rule("POST", "/api/missions/{id}/complete", "An assigned Bot may complete its own mission when workflow and delivery gates pass; this Bot-A-to-B probe must be rejected."),
        rule("POST", "/api/supervisor/herdr-maintenance/end", Expect::Forbidden, None),
        pending_rule("GET", "/api/release-triage", "#801", "Release-triage ledger visibility remains open pending the user's decision on whether it is global or assignment-scoped."),
        principal_allow_rule("POST", "/api/services/herdr-upgrade/notify", "This notification path is allowed only to the herdr-upgrade Service principal; a Bot credential must be rejected."),
        allow_rule("GET", "/api/quota", "Reads the cached quota snapshot; refresh requests are centrally limited to User or registered AGM roles (#808)."),
        rule("GET", "/api/projects/{id}/group/read", Expect::Forbidden, None),
        rule("POST", "/api/projects/{id}/group/read", Expect::Forbidden, None),
        rule("PUT", "/api/projects/{id}/group/read", Expect::Forbidden, None),
        rule("PATCH", "/api/projects/{id}/group/read", Expect::Forbidden, None),
        rule("DELETE", "/api/projects/{id}/group/read", Expect::Forbidden, None),
        rule("GET", "/api/bots/{id}/prompt", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/prompt", Expect::Allow, None),
        rule("PUT", "/api/bots/{id}/prompt", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/prompt", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/prompt", Expect::Forbidden, None),
        scoped_allow_rule("POST", "/api/missions/{id}/answer", "An assigned Bot may answer its own mission's question; this Bot-A-to-B probe must be rejected."),
        rule("GET", "/api/bots/{id}/text", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/text", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/text", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/text", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/text", Expect::Forbidden, None),
        rule("GET", "/api/bots/{id}/suggestion/accept", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/suggestion/accept", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/suggestion/accept", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/suggestion/accept", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/suggestion/accept", Expect::Forbidden, None),
        rule("GET", "/api/hosts/{name}/shells", Expect::Forbidden, None),
        rule("POST", "/api/hosts/{name}/shells", Expect::Forbidden, None),
        rule("GET", "/api/bots/{id}/start", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/start", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/start", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/start", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/start", Expect::Forbidden, None),
        rule("POST", "/api/hosts", Expect::UserOnly, None),
        rule("POST", "/api/hosts/{name}/cli-update", Expect::Forbidden, None),
        rule("POST", "/api/turns/{id}/abandon", Expect::NotFound, None),
        rule("GET", "/api/supervisor/assignments", Expect::RoleRequired, None),
        scoped_allow_rule("POST", "/api/supervisor/assignments", "An ordinary Bot may send a notice to a registered AGM role; it may not dispatch work to Bot B, ask for review, or attach a mission."),
        scoped_allow_rule("POST", "/api/missions/{id}/revise", "An assigned Bot may open a revision for its own completed mission; this Bot-A-to-B probe must be rejected."),
        deny_rule("POST", "/api/missions/{id}/deliver", Expect::Forbidden, "Delivery is a gatekeeper operation for User/AGM, not a general participant report; an ordinary Bot must not publish a mission."),
        rule("GET", "/api/projects/{id}/bots", Expect::Forbidden, None),
        rule("POST", "/api/projects/{id}/bots", Expect::Forbidden, None),
        rule("PUT", "/api/projects/{id}/bots", Expect::Forbidden, None),
        rule("PATCH", "/api/projects/{id}/bots", Expect::Forbidden, None),
        rule("DELETE", "/api/projects/{id}/bots", Expect::Forbidden, None),
        rule("GET", "/api/supervisor/fallback", Expect::Forbidden, None),
        rule("POST", "/api/supervisor/fallback", Expect::Forbidden, None),
        rule("PUT", "/api/supervisor/fallback", Expect::Forbidden, None),
        rule("PATCH", "/api/supervisor/fallback", Expect::Forbidden, None),
        rule("DELETE", "/api/supervisor/fallback", Expect::Forbidden, None),
        rule("POST", "/api/hosts/{name}/shells/{pane_id}/text", Expect::Forbidden, None),
        rule("GET", "/api/hosts/{name}/shells/{pane_id}/login", Expect::Forbidden, None),
        rule("POST", "/api/hosts/{name}/shells/{pane_id}/login/code", Expect::Forbidden, None),
        rule("GET", "/api/projects/{id}/submodules", Expect::Forbidden, None),
        rule("POST", "/api/projects/{id}/submodules", Expect::Forbidden, None),
        rule("PUT", "/api/projects/{id}/submodules", Expect::Forbidden, None),
        rule("PATCH", "/api/projects/{id}/submodules", Expect::Forbidden, None),
        rule("DELETE", "/api/projects/{id}/submodules", Expect::Forbidden, None),
        rule("GET", "/api/supervisor/state", Expect::RoleRequired, None),
        rule("GET", "/api/supervisor/responder/setup", Expect::Forbidden, None),
        rule("POST", "/api/supervisor/responder/setup", Expect::Forbidden, None),
        rule("PUT", "/api/supervisor/responder/setup", Expect::Forbidden, None),
        rule("PATCH", "/api/supervisor/responder/setup", Expect::Forbidden, None),
        rule("DELETE", "/api/supervisor/responder/setup", Expect::Forbidden, None),
        rule("GET", "/api/supervisor/herdr-maintenance", Expect::RoleRequired, None),
        rule("POST", "/api/hosts/{name}/reconnect", Expect::Forbidden, None),
        rule("POST", "/api/services/herdr-upgrade/resume/{id}", Expect::Forbidden, None),
        rule("GET", "/api/missions/{id}", Expect::Forbidden, None),
        rule("GET", "/api/bots/{id}/read", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/read", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/read", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/read", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/read", Expect::Forbidden, None),
        rule("POST", "/api/missions/{id}/cancel", Expect::UserOnly, None),
        rule("GET", "/api/supervisor/health", Expect::RoleRequired, None),
        rule("GET", "/api/bots/{id}/keys", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/keys", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/keys", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/keys", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/keys", Expect::Forbidden, None),
        rule("GET", "/api/bots/{id}/messages", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/messages", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/messages", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/messages", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/messages", Expect::Forbidden, None),
        rule("GET", "/api", Expect::NotFound, None),
        rule("POST", "/api", Expect::NotFound, None),
        rule("PUT", "/api", Expect::NotFound, None),
        rule("PATCH", "/api", Expect::NotFound, None),
        rule("DELETE", "/api", Expect::NotFound, None),
        // 有嵌前端是 200 的 index.html，沒嵌是 404。兩種都不是 Bot B 的資料。
        rule("GET", "/api/bots/{id}/share", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/share", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/share", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/share", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/share", Expect::Forbidden, None),
        rule("GET", "/api/bots/{id}/share/rotate", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/share/rotate", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/share/rotate", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/share/rotate", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/share/rotate", Expect::Forbidden, None),
        // 有嵌前端是 200 的 index.html，沒嵌是 404。兩種都不是 Bot B 的資料。
        rule("GET", "/", Expect::NotFound, None),

        rule("POST", "/api/supervisor/responder/persona", Expect::RoleRequired, None),
        rule("PATCH", "/api/supervisor/responder/persona", Expect::RoleRequired, None),
        rule("DELETE", "/api/supervisor/responder/persona", Expect::RoleRequired, None),
        rule("POST", "/api/supervisor/build-inputs", Expect::RoleRequired, None),
        rule("PUT", "/api/supervisor/build-inputs", Expect::RoleRequired, None),
        rule("PATCH", "/api/supervisor/build-inputs", Expect::RoleRequired, None),
        rule("DELETE", "/api/supervisor/build-inputs", Expect::RoleRequired, None),
        rule("POST", "/api/supervisor/maintenance/safety", Expect::RoleRequired, None),
        rule("PUT", "/api/supervisor/maintenance/safety", Expect::RoleRequired, None),
        rule("PATCH", "/api/supervisor/maintenance/safety", Expect::RoleRequired, None),
        rule("DELETE", "/api/supervisor/maintenance/safety", Expect::RoleRequired, None),
        rule("PATCH", "/api/bots/restart-idle", Expect::UserOnly, None),
        rule("DELETE", "/api/bots/restart-idle", Expect::UserOnly, None),
        rule("POST", "/api/supervisor/leases", Expect::RoleRequired, None),
        rule("PUT", "/api/supervisor/leases", Expect::RoleRequired, None),
        rule("PATCH", "/api/supervisor/leases", Expect::RoleRequired, None),
        rule("DELETE", "/api/supervisor/leases", Expect::RoleRequired, None),
        rule("POST", "/api/supervisor/assignments/{id}", Expect::RoleRequired, None),
        rule("PUT", "/api/supervisor/assignments/{id}", Expect::RoleRequired, None),
        rule("PATCH", "/api/supervisor/assignments/{id}", Expect::RoleRequired, None),
        rule("DELETE", "/api/supervisor/assignments/{id}", Expect::RoleRequired, None),
        rule("PUT", "/api/supervisor/assignments", Expect::RoleRequired, None),
        rule("PATCH", "/api/supervisor/assignments", Expect::RoleRequired, None),
        rule("DELETE", "/api/supervisor/assignments", Expect::RoleRequired, None),
        rule("PATCH", "/api/bots/deleted", Expect::UserOnly, None),
        rule("DELETE", "/api/bots/deleted", Expect::UserOnly, None),
        rule("PUT", "/api/supervisor/approvals", Expect::RoleRequired, None),
        rule("PATCH", "/api/supervisor/approvals", Expect::RoleRequired, None),
        rule("DELETE", "/api/supervisor/approvals", Expect::RoleRequired, None),
        rule("POST", "/api/search/messages", Expect::RoleRequired, None),
        rule("PUT", "/api/search/messages", Expect::RoleRequired, None),
        rule("PATCH", "/api/search/messages", Expect::RoleRequired, None),
        rule("DELETE", "/api/search/messages", Expect::RoleRequired, None),
        rule("POST", "/api/supervisor/persona", Expect::RoleRequired, None),
        rule("PATCH", "/api/supervisor/persona", Expect::RoleRequired, None),
        rule("DELETE", "/api/supervisor/persona", Expect::RoleRequired, None),
        rule("POST", "/api/supervisor/responder", Expect::RoleRequired, None),
        rule("PUT", "/api/supervisor/responder", Expect::RoleRequired, None),
        rule("PATCH", "/api/supervisor/responder", Expect::RoleRequired, None),
        rule("DELETE", "/api/supervisor/responder", Expect::RoleRequired, None),
        rule("POST", "/api/supervisor/incidents", Expect::RoleRequired, None),
        rule("PUT", "/api/supervisor/incidents", Expect::RoleRequired, None),
        rule("PATCH", "/api/supervisor/incidents", Expect::RoleRequired, None),
        rule("DELETE", "/api/supervisor/incidents", Expect::RoleRequired, None),
        rule("POST", "/api/supervisor", Expect::RoleRequired, None),
        rule("PUT", "/api/supervisor", Expect::RoleRequired, None),
        rule("PATCH", "/api/supervisor", Expect::RoleRequired, None),
        rule("DELETE", "/api/supervisor", Expect::RoleRequired, None),
        rule("POST", "/api/supervisor/state", Expect::RoleRequired, None),
        rule("PUT", "/api/supervisor/state", Expect::RoleRequired, None),
        rule("PATCH", "/api/supervisor/state", Expect::RoleRequired, None),
        rule("DELETE", "/api/supervisor/state", Expect::RoleRequired, None),
        rule("POST", "/api/supervisor/health", Expect::RoleRequired, None),
        rule("PUT", "/api/supervisor/health", Expect::RoleRequired, None),
        rule("PATCH", "/api/supervisor/health", Expect::RoleRequired, None),
        rule("DELETE", "/api/supervisor/health", Expect::RoleRequired, None),
        rule("POST", "/api/supervisor/evidence", Expect::RoleRequired, None),
        rule("PUT", "/api/supervisor/evidence", Expect::RoleRequired, None),
        rule("PATCH", "/api/supervisor/evidence", Expect::RoleRequired, None),
        rule("DELETE", "/api/supervisor/evidence", Expect::RoleRequired, None),
        rule("POST", "/api/supervisor/herdr-maintenance", Expect::RoleRequired, None),
        rule("PUT", "/api/supervisor/herdr-maintenance", Expect::RoleRequired, None),
        rule("PATCH", "/api/supervisor/herdr-maintenance", Expect::RoleRequired, None),
        rule("DELETE", "/api/supervisor/herdr-maintenance", Expect::RoleRequired, None),
        rule("POST", "/api/supervisor/inbox", Expect::RoleRequired, None),
        rule("PUT", "/api/supervisor/inbox", Expect::RoleRequired, None),
        rule("PATCH", "/api/supervisor/inbox", Expect::RoleRequired, None),
        rule("DELETE", "/api/supervisor/inbox", Expect::RoleRequired, None),

    ]
}

fn router_paths_and_methods(debug: &str) -> Vec<(String, String)> {
    // Node 的 paths 是 `"RouteId(N)": "/api/..."`. 方法不跟路徑綁在同一段字串，
    // 所以樣板路徑來自 router Debug，方法用探測：405 代表這個方法沒註冊。
    let mut paths = Vec::new();
    let mut rest = debug;
    while let Some(i) = rest.find("\"/") {
        rest = &rest[i + 1..];
        let Some(end) = rest.find('"') else { break };
        let path = &rest[..end];
        rest = &rest[end + 1..];
        if path.contains("__private__") || path.contains('*') {
            continue;
        }
        if !paths.iter().any(|p: &String| p == path) {
            paths.push(path.to_string());
        }
    }
    let mut out = Vec::new();
    for path in paths {
        for method in ["GET", "POST", "PUT", "PATCH", "DELETE"] {
            out.push((method.to_string(), path.clone()));
        }
    }
    out
}

fn fill(template: &str, bot_b: &str, project_b: &str, mission_b: &str) -> String {
    let mut s = template.to_string();
    while let Some(start) = s.find('{') {
        let end = s[start..].find('}').map(|i| start + i).unwrap_or(s.len() - 1);
        let key = &s[start + 1..end];
        let before = s[..start].trim_end_matches('/');
        let value = match (before.rsplit('/').next().unwrap_or(""), key) {
            ("bots", "id") => bot_b,
            ("projects", "id") => project_b,
            ("missions", "id") => mission_b,
            ("turns", "id") => "turn-b",
            ("attachments", "id") => "att-b",
            ("assignments", "id") => "asg-b",
            ("inbox", "id") => "inbox-b",
            ("panes", "id") => "pane-b",
            (_, "name") if before.ends_with("hosts") => "local",
            (_, "name") => "cc0",
            (_, "identity") => "cc0",
            (_, "number") => "1",
            (_, "key") => "draft-key",
            (_, "provider") => "claude",
            (_, "resource") => "restart",
            (_, "pane_id") => "pane-b",
            _ => "foreign",
        };
        s.replace_range(start..=end, value);
    }
    s
}

fn observed(status: StatusCode, body: &str) -> Result<Expect, StatusCode> {
    if status == StatusCode::FORBIDDEN {
        if body.contains("\"reason\":\"user_only\"") {
            Ok(Expect::UserOnly)
        } else if body.contains("\"reason\":\"role_required\"") {
            Ok(Expect::RoleRequired)
        } else {
            Ok(Expect::Forbidden)
        }
    } else if status == StatusCode::NOT_FOUND {
        Ok(Expect::NotFound)
    } else if status == StatusCode::UNAUTHORIZED {
        Ok(Expect::Unauthorized)
    } else {
        Ok(Expect::Allow)
    }
}

fn matches_expect(expect: Expect, got: Expect) -> bool {
    match expect {
        // 舊列只要求 403，reason 未分。
        Expect::Forbidden => matches!(got, Expect::Forbidden | Expect::UserOnly | Expect::RoleRequired),
        other => got == other,
    }
}

fn request_body(method: &str, template: &str, bot_a: &str, bot_b: &str) -> String {
    let body = match (method, template) {
        ("PUT", "/api/supervisor/persona" | "/api/supervisor/responder/persona") => json!({"text":"matrix persona", "expected_version":0}),
        ("POST", "/api/supervisor/approvals") => json!({"requester":bot_a, "purpose":"rebuild", "scope":"matrix review"}),
        ("POST", "/api/supervisor/assignments") => json!({"target_bot_id":bot_b, "text":"cross-bot task", "client_request_id":"matrix-cross-bot-task", "kind":"task"}),
        ("POST", "/api/supervisor/leases/{resource}/acquire") => json!({"owner":bot_b, "approval_id":"approval-b", "require_idle":false}),
        ("POST", "/api/supervisor/leases/{resource}/renew" | "/api/supervisor/leases/{resource}/release") => json!({"owner":bot_b, "fence":1}),
        ("POST", "/api/missions/{id}/question") => json!({"text":"cross-mission question", "client_request_id":"matrix-question"}),
        ("POST", "/api/missions/{id}/events") => json!({"kind":"note", "text":"cross-mission event"}),
        ("POST", "/api/missions/{id}/complete") => json!({"result_summary":"cross-mission completion"}),
        ("POST", "/api/missions/{id}/answer") => json!({"text":"cross-mission answer", "client_request_id":"matrix-answer", "reply_to":"question-b"}),
        ("POST", "/api/missions/{id}/revise") => json!({"text":"cross-mission revision", "client_request_id":"matrix-revision"}),
        ("POST", "/api/missions/{id}/deliver") => json!({"worktree":"/tmp"}),
        ("POST", "/api/services/daemon-swap/restart-window") => json!({"owner":"daemon-update-kick", "commit":"abcdef0", "ttl_secs":600}),
        ("POST", "/api/services/herdr-upgrade/notify") => json!({"text":"matrix notification"}),
        _ => json!({}),
    };
    serde_json::to_string(&body).unwrap()
}

#[tokio::test]
async fn bot_a_against_bot_b_matches_the_allow_table() {
    let env = crate::testing::env().await;
    env.app.set_startup_ready(true);
    let now = db::now();
    for (id, path, label) in [("pa", "/tmp/pa", "A"), ("pb", "/tmp/pb", "B")] {
        sqlx::query("INSERT INTO projects (id, path, label, created_at) VALUES (?,?,?,?)")
            .bind(id)
            .bind(path)
            .bind(label)
            .bind(&now)
            .execute(&env.app.db)
            .await
            .unwrap();
    }
    let bot_a = crate::testing::claude_bot(&env.app, "pa", "bot-a").await;
    let bot_b = crate::testing::claude_bot(&env.app, "pb", "bot-b").await;
    sqlx::query("UPDATE bots SET hook_token = ? WHERE id = ?")
        .bind("token-a")
        .bind(&bot_a.id)
        .execute(&env.app.db)
        .await
        .unwrap();
    env.app.models_cache.lock().await.insert(
        "local/codex/".into(),
        (std::time::Instant::now(), json!({"kind":"codex", "host":"local", "models":[{"id":"cached-model"}]})),
    );

    // A real foreign mission with an assignment to Bot B makes the mission probes distinguish
    // participant authorization from a nonexistent-id 404.
    let mission_b = crate::db::ulid();
    sqlx::query("INSERT INTO missions (id, project_id, client_request_id, text, delivery_mode, executor_kind, on_5h_limit, created_at, updated_at) VALUES (?,?,?,'scope fixture','pr','claude','wait',?,?)")
        .bind(&mission_b)
        .bind("pb")
        .bind(format!("matrix-{}", mission_b))
        .bind(&now)
        .bind(&now)
        .execute(&env.app.db)
        .await
        .unwrap();
    let assignment_b = crate::supervisor::store::insert_assignment(
        &env.app.db,
        None,
        &bot_b.id,
        &format!("matrix-assignment-{}", mission_b),
        "Bot B owns this mission",
        &[],
        None,
        false,
    )
    .await
    .unwrap();
    crate::supervisor::store::set_mission_link(&env.app.db, &assignment_b.id, &mission_b, "executor")
        .await
        .unwrap();

    // #801 stays deliberately open pending the user's decision, but exercise a real verdict
    // against a seeded ledger row so the matrix proves the write endpoint is reachable.
    let sections = crate::release_triage::source_sections("claude", include_str!("release_triage/fixtures/claude_2.1.276-278.md"));
    let section = sections.iter().find(|section| section.version == "2.1.277").unwrap();
    let entries = crate::release_triage::build_entries("claude", section).unwrap();
    crate::release_triage::ledger::insert_version(&env.app.db, "claude", "2.1.277", &entries)
        .await
        .unwrap();
    let release_submission = json!({
        "kind":"claude",
        "version":"2.1.277",
        "verdicts":entries.iter().filter(|entry| entry.bucket != crate::release_triage::Bucket::Dropped).map(|entry| json!({
            "entry_id":entry.id,
            "verdict":"none",
            "reason":"scope matrix",
            "module":"none"
        })).collect::<Vec<_>>(),
        "issues":[]
    }).to_string();

    let router = router(env.app.clone());
    let discovered = router_paths_and_methods(&format!("{router:?}"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
    });
    let client = reqwest::Client::new();
    let mut misses = Vec::new();
    let table = rules();
    let mut seen = vec![false; table.len()];
    for (method, template) in &discovered {
        // The principal middleware checks a real foreign mission before Axum can return 405,
        // so unsupported verbs would look like unlisted protected endpoints. Probe those verbs
        // with a nonexistent mission first; registered handlers reach their own 404/validation.
        if template.starts_with("/api/missions/") && !table.iter().any(|r| r.method == method && r.path == template) {
            let probe_uri = fill(template, &bot_b.id, "pb", "no-such-mission");
            let probe_body = request_body(method, template, &bot_a.id, &bot_b.id);
            let probe = client
                .request(method.parse().unwrap(), format!("http://127.0.0.1:{port}{probe_uri}"))
                .header("content-type", "application/json")
                .header("X-AM-Bot-Id", &bot_a.id)
                .header("X-AM-Bot-Token", "token-a")
                .body(probe_body)
                .send()
                .await
                .unwrap();
            if probe.status() == StatusCode::METHOD_NOT_ALLOWED {
                continue;
            }
        }
        let mut uri = fill(template, &bot_b.id, "pb", &mission_b);
        if template == "/api/models" {
            uri.push_str("?kind=codex");
        }
        let body = if method == "POST" && template == "/api/release-triage/verdicts" {
            release_submission.clone()
        } else {
            request_body(method, template, &bot_a.id, &bot_b.id)
        };
        let mut request = client
            .request(method.parse().unwrap(), format!("http://127.0.0.1:{port}{uri}"))
            .header("content-type", "application/json")
            .header("X-AM-Bot-Id", &bot_a.id)
            .header("X-AM-Bot-Token", "token-a")
            .body(body);
        if template == "/ws" {
            request = request
                .header("connection", "Upgrade")
                .header("upgrade", "websocket")
                .header("sec-websocket-version", "13")
                .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==");
        }
        let res = request.send().await.unwrap();
        let status = StatusCode::from_u16(res.status().as_u16()).unwrap();
        let body = res.text().await.unwrap_or_default();
        if status == StatusCode::METHOD_NOT_ALLOWED {
            continue;
        }
        let Some(idx) = table.iter().position(|r| r.method == method && r.path == template) else {
            misses.push(format!("UNLISTED {method} {template} -> {status}"));
            continue;
        };
        seen[idx] = true;
        let rule = &table[idx];
        let got = observed(status, &body).unwrap_or_else(|s| panic!("{method} {template} 出現未分類的 {s}"));
        if matches!(rule.decision, Some(Decision::Allowed | Decision::Pending)) {
            assert!(rule.rationale.is_some_and(|reason| !reason.trim().is_empty()), "{method} {template} 的允許／待決判定必須附理由");
        }
        if let Some(reason) = rule.rationale {
            assert!(!reason.trim().is_empty(), "{method} {template} 的權限判定必須附理由");
        }
        if template == "/api/state" {
            let state: Value = serde_json::from_str(&body).unwrap_or_else(|e| panic!("Bot /api/state 必須回 JSON：{e}: {body}"));
            let visible: Vec<&Value> = state["projects"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|project| project["bots"].as_array().into_iter().flatten())
                .collect();
            assert!(visible.iter().any(|bot| bot["id"] == bot_a.id), "filtered state keeps the caller: {body}");
            assert!(visible.iter().all(|bot| bot["id"] != bot_b.id), "filtered state must not include Bot B: {body}");
            assert!(state.get("hosts").is_none() && state.get("identities").is_none(), "Bot state excludes global host and identity data: {body}");
            for bot in visible {
                for field in ["persona", "args", "identity", "env", "herdr_session", "unread", "read_mark"] {
                    assert!(bot.get(field).is_none(), "Bot state must omit {field}: {bot}");
                }
            }
            assert!(!body.contains("token-a") && !body.contains("hook_token"), "Bot state excludes credential fields: {body}");
        }
        if rule.path == "/" {
            assert!(
                matches!(got, Expect::Allow | Expect::NotFound),
                "GET / 只接受嵌前端的 200 或沒嵌的 404，實際 {status}"
            );
            continue;
        }
        if let Some(ticket) = rule.open_leak {
            assert_eq!(rule.decision, Some(Decision::Pending), "open_leak 必須保留待決判定");
            assert_eq!(rule.expect, Expect::Forbidden, "{method} {template} 的 open_leak 只用於政策 403");
            let label = if ticket.is_empty() { "無票" } else { ticket };
            assert!(
                matches!(got, Expect::Allow),
                "{method} {template} 標成仍放行（{label}）但現在是 {status} {}，拿掉 open_leak",
                body.chars().take(180).collect::<String>()
            );
        } else if !matches_expect(rule.expect, got) {
            panic!("{method} {template} 期望 {:?} 實際 {status} {}", rule.expect, body.chars().take(180).collect::<String>());
        }
    }
    for (rule, hit) in table.iter().zip(seen) {
        if !hit {
            misses.push(format!("UNUSED {} {}", rule.method, rule.path));
        }
    }
    assert!(misses.is_empty(), "允許表與 router 不一致：\n{}", misses.join("\n"));
}
