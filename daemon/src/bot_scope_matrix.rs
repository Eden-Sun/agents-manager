//! 一般 bot principal 對「全部路由」的範圍表（#790–#812 那一族）。
//!
//! 路由從 [`crate::api::router`] 的 Debug 列出來，不手抄。每條用 Bot A 的 token 打，
//! 路徑上的 bot／project 屬於 Bot B（沒有列的 id 用固定的外國 id）。
//! 允許表沒有這一列、或多一列沒打到，測試就失敗。
//!
//! `open_leak`：政策是 403，但這一輪實測仍放行（不是 401／403／404）。
//! `Some("")` 是還沒有票的洩漏。handler 修掉之後拿掉 `open_leak`，改成實測的 403 或 404。
//! 這裡不改 handler。bot 自己的 hook、relay、build-slots，以及跨 bot 的
//! `POST /api/bots/{id}/prompt`，表上是允許。

use super::*;
use axum::http::StatusCode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Expect {
    /// 允許：不是 401／403／404（含 2xx、400、409、415、422、5xx）。
    Allow,
    /// 403。
    Forbidden,
    /// 404。
    NotFound,
}

struct Rule {
    method: &'static str,
    /// router 上的樣板，例如 `/api/bots/{id}`。
    path: &'static str,
    expect: Expect,
    /// `Some(票號)` 或 `Some("")`（無票）＝政策 403，現況仍允許。
    open_leak: Option<&'static str>,
}

fn rule(method: &'static str, path: &'static str, expect: Expect, open_leak: Option<&'static str>) -> Rule {
    Rule { method, path, expect, open_leak }
}

fn rules() -> Vec<Rule> {
    vec![
        rule("GET", "/api/session", Expect::Forbidden, Some("")),
        rule("GET", "/api/bots/{id}/credential/rotate", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/credential/rotate", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/credential/rotate", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/credential/rotate", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/credential/rotate", Expect::Forbidden, None),
        rule("GET", "/api/supervisor", Expect::Forbidden, Some("#799")),
        rule("POST", "/api/services/daemon-swap/restart-window", Expect::Forbidden, Some("")),
        rule("GET", "/api/missions/{id}/pick", Expect::NotFound, None),
        rule("GET", "/api/fs/dirs", Expect::Forbidden, None),
        rule("GET", "/api/supervisor/persona", Expect::Forbidden, Some("#799")),
        rule("PUT", "/api/supervisor/persona", Expect::Forbidden, Some("#799")),
        rule("GET", "/api/judge/shadow", Expect::Forbidden, Some("#795")),
        rule("POST", "/api/services/daemon-swap/probe/{id}", Expect::Forbidden, None),
        rule("GET", "/api/bots/{id}/terminal", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/terminal", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/terminal", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/terminal", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/terminal", Expect::Forbidden, None),
        rule("GET", "/api/supervisor/approvals", Expect::Forbidden, Some("#799")),
        rule("POST", "/api/supervisor/approvals", Expect::Forbidden, Some("#799")),
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
        rule("GET", "/api/claude-update/review", Expect::Forbidden, Some("#802")),
        rule("POST", "/api/claude-update/review", Expect::Forbidden, Some("#802")),
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
        rule("GET", "/api/build/remote", Expect::Forbidden, Some("#794")),
        rule("PUT", "/api/build/remote", Expect::Forbidden, Some("#794")),
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
        rule("GET", "/api/hosts/{name}/shells/{pane_id}/keys", Expect::Forbidden, None),
        rule("POST", "/api/hosts/{name}/shells/{pane_id}/keys", Expect::Forbidden, None),
        rule("PUT", "/api/hosts/{name}/shells/{pane_id}/keys", Expect::Forbidden, None),
        rule("PATCH", "/api/hosts/{name}/shells/{pane_id}/keys", Expect::Forbidden, None),
        rule("DELETE", "/api/hosts/{name}/shells/{pane_id}/keys", Expect::Forbidden, None),
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
        rule("GET", "/api/capabilities", Expect::Forbidden, Some("")),
        rule("POST", "/api/quota/probe", Expect::Forbidden, Some("#800")),
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
        rule("GET", "/api/supervisor/inbox", Expect::Forbidden, Some("#799")),
        rule("GET", "/api/models", Expect::Forbidden, Some("#809")),
        rule("POST", "/api/build/remote/test", Expect::Forbidden, Some("#794")),
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
        rule("GET", "/api/supervisor/leases", Expect::Forbidden, Some("#799")),
        rule("GET", "/api/projects/{id}/git/pull", Expect::Forbidden, None),
        rule("POST", "/api/projects/{id}/git/pull", Expect::Forbidden, None),
        rule("PUT", "/api/projects/{id}/git/pull", Expect::Forbidden, None),
        rule("PATCH", "/api/projects/{id}/git/pull", Expect::Forbidden, None),
        rule("DELETE", "/api/projects/{id}/git/pull", Expect::Forbidden, None),
        rule("POST", "/api/missions/{id}/pause", Expect::Forbidden, Some("#803")),
        rule("GET", "/api/deploy/status", Expect::Forbidden, Some("")),
        rule("GET", "/api/supervisor/incidents", Expect::Forbidden, Some("#799")),
        rule("POST", "/api/hosts/{name}/tools/install", Expect::Forbidden, Some("")),
        rule("GET", "/api/supervisor/responder/start", Expect::Forbidden, None),
        rule("POST", "/api/supervisor/responder/start", Expect::Forbidden, None),
        rule("PUT", "/api/supervisor/responder/start", Expect::Forbidden, None),
        rule("PATCH", "/api/supervisor/responder/start", Expect::Forbidden, None),
        rule("DELETE", "/api/supervisor/responder/start", Expect::Forbidden, None),
        rule("GET", "/api/supervisor/maintenance/safety", Expect::Forbidden, Some("#799")),
        rule("GET", "/api/bots/deleted", Expect::Forbidden, Some("")),
        rule("GET", "/api/supervisor/build-inputs", Expect::Forbidden, Some("#799")),
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
        rule("GET", "/api/intents", Expect::Forbidden, Some("#811")),
        rule("POST", "/api/release-triage/dispatched", Expect::Forbidden, Some("#801")),
        rule("GET", "/api/bots/{id}/preview", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/preview", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/preview", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/preview", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/preview", Expect::Forbidden, None),
        rule("GET", "/api/identity-prefs", Expect::Forbidden, Some("#806")),
        rule("GET", "/api/panes/{id}/adopt", Expect::Forbidden, None),
        rule("POST", "/api/panes/{id}/adopt", Expect::Forbidden, None),
        rule("PUT", "/api/panes/{id}/adopt", Expect::Forbidden, None),
        rule("PATCH", "/api/panes/{id}/adopt", Expect::Forbidden, None),
        rule("DELETE", "/api/panes/{id}/adopt", Expect::Forbidden, None),
        rule("GET", "/api/bots/{id}/restart", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/restart", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/restart", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/restart", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/restart", Expect::Forbidden, None),
        rule("GET", "/api/state", Expect::Forbidden, Some("#807")),
        rule("POST", "/api/release-triage/verdicts", Expect::Forbidden, Some("#801")),
        rule("GET", "/api/drafts/{key}", Expect::Forbidden, None),
        rule("POST", "/api/drafts/{key}", Expect::Forbidden, None),
        rule("PUT", "/api/drafts/{key}", Expect::Forbidden, None),
        rule("PATCH", "/api/drafts/{key}", Expect::Forbidden, None),
        rule("DELETE", "/api/drafts/{key}", Expect::Forbidden, None),
        rule("POST", "/api/supervisor/leases/{resource}/renew", Expect::Forbidden, Some("#799")),
        rule("POST", "/api/hosts/{name}/gh/cancel", Expect::Forbidden, None),
        rule("POST", "/api/supervisor/persona/adopt-embedded", Expect::Forbidden, None),
        rule("POST", "/build-slots/acquire", Expect::Allow, None),
        rule("POST", "/api/hosts/{name}/identities/{identity}/logout", Expect::Forbidden, None),
        rule("GET", "/api/projects/{id}/messages", Expect::Forbidden, None),
        rule("POST", "/api/projects/{id}/messages", Expect::Forbidden, None),
        rule("PUT", "/api/projects/{id}/messages", Expect::Forbidden, None),
        rule("PATCH", "/api/projects/{id}/messages", Expect::Forbidden, None),
        rule("DELETE", "/api/projects/{id}/messages", Expect::Forbidden, None),
        rule("GET", "/api/supervisor/responder/persona", Expect::Forbidden, Some("#799")),
        rule("PUT", "/api/supervisor/responder/persona", Expect::Forbidden, Some("#799")),
        rule("GET", "/api/projects/{id}/issues/{number}", Expect::Forbidden, None),
        rule("POST", "/api/projects/{id}/issues/{number}", Expect::Forbidden, None),
        rule("PUT", "/api/projects/{id}/issues/{number}", Expect::Forbidden, None),
        rule("PATCH", "/api/projects/{id}/issues/{number}", Expect::Forbidden, None),
        rule("DELETE", "/api/projects/{id}/issues/{number}", Expect::Forbidden, None),
        rule("GET", "/api/drafts", Expect::Forbidden, None),
        rule("POST", "/api/drafts", Expect::Forbidden, None),
        rule("PUT", "/api/drafts", Expect::Forbidden, None),
        rule("PATCH", "/api/drafts", Expect::Forbidden, None),
        rule("DELETE", "/api/drafts", Expect::Forbidden, None),
        rule("GET", "/api/upstream-updates", Expect::Forbidden, Some("")),
        rule("GET", "/api/bots/{id}/promote", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/promote", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/promote", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/promote", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/promote", Expect::Forbidden, None),
        rule("GET", "/api/supervisor/handoff", Expect::Forbidden, Some("#799")),
        rule("POST", "/api/supervisor/handoff", Expect::Forbidden, None),
        rule("PUT", "/api/supervisor/handoff", Expect::Forbidden, None),
        rule("PATCH", "/api/supervisor/handoff", Expect::Forbidden, None),
        rule("DELETE", "/api/supervisor/handoff", Expect::Forbidden, None),
        rule("POST", "/api/missions/{id}/question", Expect::Forbidden, Some("#803")),
        rule("POST", "/api/missions/{id}/round", Expect::NotFound, None),
        rule("GET", "/api/bots/{id}/restore", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/restore", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/restore", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/restore", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/restore", Expect::Forbidden, None),
        rule("POST", "/api/order", Expect::Forbidden, Some("#804")),
        rule("GET", "/api/bots/{id}/outbox/file", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/outbox/file", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/outbox/file", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/outbox/file", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/outbox/file", Expect::Forbidden, None),
        rule("GET", "/api/panes/{id}/focus", Expect::Forbidden, None),
        rule("POST", "/api/panes/{id}/focus", Expect::Forbidden, None),
        rule("PUT", "/api/panes/{id}/focus", Expect::Forbidden, None),
        rule("PATCH", "/api/panes/{id}/focus", Expect::Forbidden, None),
        rule("DELETE", "/api/panes/{id}/focus", Expect::Forbidden, None),
        rule("POST", "/api/identities", Expect::Forbidden, Some("#804")),
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
        rule("POST", "/api/projects", Expect::Forbidden, Some("#804")),
        rule("POST", "/api/missions/{id}/resume", Expect::NotFound, None),
        rule("GET", "/api/supervisor/responder", Expect::Forbidden, Some("#799")),
        rule("POST", "/api/missions/{id}/events", Expect::Forbidden, Some("#803")),
        rule("GET", "/api/bots/{id}/fork", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/fork", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/fork", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/fork", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/fork", Expect::Forbidden, None),
        rule("GET", "/api/supervisor/assignments/{id}", Expect::NotFound, None),
        rule("GET", "/api/mem/processes", Expect::Forbidden, Some("#810")),
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
        rule("GET", "/api/mem", Expect::Forbidden, Some("#810")),
        rule("POST", "/api/release-triage/publish", Expect::Forbidden, Some("#801")),
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
        rule("POST", "/api/supervisor/leases/{resource}/release", Expect::Forbidden, Some("#799")),
        rule("POST", "/api/deploy/now", Expect::Forbidden, None),
        rule("POST", "/api/mem/processes/kill", Expect::Forbidden, Some("#796")),
        rule("GET", "/api/changelog", Expect::Forbidden, Some("")),
        rule("POST", "/relay/announce", Expect::Allow, None),
        rule("POST", "/api/build/remote/install-toolchain", Expect::Forbidden, Some("#794")),
        rule("GET", "/ws", Expect::Forbidden, Some("")),
        rule("POST", "/api/hosts/{name}/gh/login", Expect::Forbidden, None),
        rule("PUT", "/api/identities/{name}/disabled", Expect::Forbidden, Some("#806")),
        rule("POST", "/api/bots/restart-idle", Expect::Forbidden, Some("#804")),
        rule("GET", "/api/search/messages", Expect::Forbidden, Some("")),
        rule("GET", "/api/bots/{id}/interrupt", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/interrupt", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/interrupt", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/interrupt", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/interrupt", Expect::Forbidden, None),
        rule("GET", "/api/mem/processes/pane", Expect::Forbidden, Some("#798")),
        rule("GET", "/api/hosts/{name}/shells/{pane_id}/terminal", Expect::Forbidden, None),
        rule("POST", "/api/hosts/{name}/shells/{pane_id}/terminal", Expect::Forbidden, None),
        rule("PUT", "/api/hosts/{name}/shells/{pane_id}/terminal", Expect::Forbidden, None),
        rule("PATCH", "/api/hosts/{name}/shells/{pane_id}/terminal", Expect::Forbidden, None),
        rule("DELETE", "/api/hosts/{name}/shells/{pane_id}/terminal", Expect::Forbidden, None),
        rule("GET", "/api/attachments/{id}", Expect::Forbidden, None),
        rule("POST", "/api/attachments/{id}", Expect::Forbidden, None),
        rule("PUT", "/api/attachments/{id}", Expect::Forbidden, None),
        rule("PATCH", "/api/attachments/{id}", Expect::Forbidden, None),
        rule("DELETE", "/api/attachments/{id}", Expect::Forbidden, None),
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
        rule("GET", "/api/supervisor/evidence", Expect::Forbidden, Some("#790")),
        rule("GET", "/api/hosts/{name}/shells/{pane_id}", Expect::Forbidden, None),
        rule("POST", "/api/hosts/{name}/shells/{pane_id}", Expect::Forbidden, None),
        rule("PUT", "/api/hosts/{name}/shells/{pane_id}", Expect::Forbidden, None),
        rule("PATCH", "/api/hosts/{name}/shells/{pane_id}", Expect::Forbidden, None),
        rule("DELETE", "/api/hosts/{name}/shells/{pane_id}", Expect::Forbidden, None),
        rule("POST", "/hook/{provider}", Expect::Allow, None),
        rule("GET", "/api/panes", Expect::Forbidden, None),
        rule("POST", "/api/panes", Expect::Forbidden, None),
        rule("PUT", "/api/panes", Expect::Forbidden, None),
        rule("PATCH", "/api/panes", Expect::Forbidden, None),
        rule("DELETE", "/api/panes", Expect::Forbidden, None),
        rule("POST", "/build-slots/release", Expect::Allow, None),
        rule("POST", "/api/hosts/{name}/herdr-update", Expect::Forbidden, None),
        rule("GET", "/api/supervisor/remote", Expect::Forbidden, Some("#799")),
        rule("POST", "/api/supervisor/remote", Expect::Forbidden, None),
        rule("PUT", "/api/supervisor/remote", Expect::Forbidden, None),
        rule("PATCH", "/api/supervisor/remote", Expect::Forbidden, None),
        rule("DELETE", "/api/supervisor/remote", Expect::Forbidden, None),
        rule("GET", "/api/supervisor/cli", Expect::Forbidden, Some("#799")),
        rule("POST", "/api/supervisor/cli", Expect::Forbidden, None),
        rule("PUT", "/api/supervisor/cli", Expect::Forbidden, None),
        rule("PATCH", "/api/supervisor/cli", Expect::Forbidden, None),
        rule("DELETE", "/api/supervisor/cli", Expect::Forbidden, None),
        rule("GET", "/api/build-slots", Expect::Forbidden, Some("#812")),
        rule("GET", "/api/judge/settings", Expect::Forbidden, Some("#795")),
        rule("PUT", "/api/judge/settings", Expect::Forbidden, Some("#795")),
        rule("GET", "/api/projects/{id}/github/refresh", Expect::Forbidden, None),
        rule("POST", "/api/projects/{id}/github/refresh", Expect::Forbidden, None),
        rule("PUT", "/api/projects/{id}/github/refresh", Expect::Forbidden, None),
        rule("PATCH", "/api/projects/{id}/github/refresh", Expect::Forbidden, None),
        rule("DELETE", "/api/projects/{id}/github/refresh", Expect::Forbidden, None),
        rule("DELETE", "/api/identities/{name}", Expect::Forbidden, Some("#806")),
        rule("POST", "/api/supervisor/leases/{resource}/acquire", Expect::Forbidden, Some("#799")),
        rule("GET", "/api/panes/{id}/close", Expect::Forbidden, None),
        rule("POST", "/api/panes/{id}/close", Expect::Forbidden, None),
        rule("PUT", "/api/panes/{id}/close", Expect::Forbidden, None),
        rule("PATCH", "/api/panes/{id}/close", Expect::Forbidden, None),
        rule("DELETE", "/api/panes/{id}/close", Expect::Forbidden, None),
        rule("GET", "/api/supervisor/responder/stop", Expect::Forbidden, None),
        rule("POST", "/api/supervisor/responder/stop", Expect::Forbidden, None),
        rule("PUT", "/api/supervisor/responder/stop", Expect::Forbidden, None),
        rule("PATCH", "/api/supervisor/responder/stop", Expect::Forbidden, None),
        rule("DELETE", "/api/supervisor/responder/stop", Expect::Forbidden, None),
        rule("POST", "/api/turns/{id}/withdraw", Expect::NotFound, None),
        rule("POST", "/api/missions/{id}/complete", Expect::Forbidden, Some("#803")),
        rule("POST", "/api/supervisor/herdr-maintenance/end", Expect::Forbidden, None),
        rule("GET", "/api/release-triage", Expect::Forbidden, Some("#801")),
        rule("POST", "/api/services/herdr-upgrade/notify", Expect::Forbidden, Some("")),
        rule("GET", "/api/quota", Expect::Forbidden, Some("#808")),
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
        rule("POST", "/api/missions/{id}/answer", Expect::Forbidden, Some("#803")),
        rule("GET", "/api/bots/{id}/text", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/text", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/text", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/text", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/text", Expect::Forbidden, None),
        rule("GET", "/api/hosts/{name}/shells", Expect::Forbidden, None),
        rule("POST", "/api/hosts/{name}/shells", Expect::Forbidden, None),
        rule("PUT", "/api/hosts/{name}/shells", Expect::Forbidden, None),
        rule("PATCH", "/api/hosts/{name}/shells", Expect::Forbidden, None),
        rule("DELETE", "/api/hosts/{name}/shells", Expect::Forbidden, None),
        rule("GET", "/api/bots/{id}/start", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/start", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/start", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/start", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/start", Expect::Forbidden, None),
        rule("POST", "/api/hosts", Expect::Forbidden, Some("#804")),
        rule("POST", "/api/hosts/{name}/cli-update", Expect::Forbidden, None),
        rule("POST", "/api/turns/{id}/abandon", Expect::NotFound, None),
        rule("GET", "/api/supervisor/assignments", Expect::Forbidden, Some("#799")),
        rule("POST", "/api/supervisor/assignments", Expect::Forbidden, Some("#799")),
        rule("POST", "/api/missions/{id}/revise", Expect::Forbidden, Some("#803")),
        rule("POST", "/api/missions/{id}/deliver", Expect::Forbidden, Some("#803")),
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
        rule("GET", "/api/hosts/{name}/shells/{pane_id}/text", Expect::Forbidden, None),
        rule("POST", "/api/hosts/{name}/shells/{pane_id}/text", Expect::Forbidden, None),
        rule("PUT", "/api/hosts/{name}/shells/{pane_id}/text", Expect::Forbidden, None),
        rule("PATCH", "/api/hosts/{name}/shells/{pane_id}/text", Expect::Forbidden, None),
        rule("DELETE", "/api/hosts/{name}/shells/{pane_id}/text", Expect::Forbidden, None),
        rule("GET", "/api/projects/{id}/submodules", Expect::Forbidden, None),
        rule("POST", "/api/projects/{id}/submodules", Expect::Forbidden, None),
        rule("PUT", "/api/projects/{id}/submodules", Expect::Forbidden, None),
        rule("PATCH", "/api/projects/{id}/submodules", Expect::Forbidden, None),
        rule("DELETE", "/api/projects/{id}/submodules", Expect::Forbidden, None),
        rule("GET", "/api/supervisor/state", Expect::Forbidden, Some("#799")),
        rule("GET", "/api/supervisor/responder/setup", Expect::Forbidden, None),
        rule("POST", "/api/supervisor/responder/setup", Expect::Forbidden, None),
        rule("PUT", "/api/supervisor/responder/setup", Expect::Forbidden, None),
        rule("PATCH", "/api/supervisor/responder/setup", Expect::Forbidden, None),
        rule("DELETE", "/api/supervisor/responder/setup", Expect::Forbidden, None),
        rule("GET", "/api/supervisor/herdr-maintenance", Expect::Forbidden, Some("#799")),
        rule("POST", "/api/hosts/{name}/reconnect", Expect::Forbidden, None),
        rule("POST", "/api/services/herdr-upgrade/resume/{id}", Expect::Forbidden, None),
        rule("GET", "/api/missions/{id}", Expect::NotFound, None),
        rule("GET", "/api/bots/{id}/read", Expect::Forbidden, None),
        rule("POST", "/api/bots/{id}/read", Expect::Forbidden, None),
        rule("PUT", "/api/bots/{id}/read", Expect::Forbidden, None),
        rule("PATCH", "/api/bots/{id}/read", Expect::Forbidden, None),
        rule("DELETE", "/api/bots/{id}/read", Expect::Forbidden, None),
        rule("POST", "/api/missions/{id}/cancel", Expect::NotFound, None),
        rule("GET", "/api/supervisor/health", Expect::Forbidden, Some("#799")),
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

fn fill(template: &str, bot_b: &str, project_b: &str) -> String {
    let mut s = template.to_string();
    while let Some(start) = s.find('{') {
        let end = s[start..].find('}').map(|i| start + i).unwrap_or(s.len() - 1);
        let key = &s[start + 1..end];
        let before = s[..start].trim_end_matches('/');
        let value = match (before.rsplit('/').next().unwrap_or(""), key) {
            ("bots", "id") => bot_b,
            ("projects", "id") => project_b,
            ("missions", "id") => "mission-b",
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

fn class(status: StatusCode) -> Result<Expect, StatusCode> {
    if status == StatusCode::FORBIDDEN {
        Ok(Expect::Forbidden)
    } else if status == StatusCode::NOT_FOUND {
        Ok(Expect::NotFound)
    } else if status == StatusCode::UNAUTHORIZED {
        Err(status)
    } else {
        Ok(Expect::Allow)
    }
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
        let uri = fill(template, &bot_b.id, "pb");
        let res = client
            .request(method.parse().unwrap(), format!("http://127.0.0.1:{port}{uri}"))
            .header("content-type", "application/json")
            .header("X-AM-Bot-Id", &bot_a.id)
            .header("X-AM-Bot-Token", "token-a")
            .body("{}")
            .send()
            .await
            .unwrap();
        let status = StatusCode::from_u16(res.status().as_u16()).unwrap();
        if status == StatusCode::METHOD_NOT_ALLOWED {
            continue;
        }
        let Some(idx) = table.iter().position(|r| r.method == method && r.path == template) else {
            misses.push(format!("UNLISTED {method} {template} -> {status}"));
            continue;
        };
        seen[idx] = true;
        let rule = &table[idx];
        let got = class(status).unwrap_or_else(|s| panic!("{method} {template} 出現未分類的 {s}"));
        if rule.path == "/" {
            assert!(
                matches!(got, Expect::Allow | Expect::NotFound),
                "GET / 只接受嵌前端的 200 或沒嵌的 404，實際 {status}"
            );
            continue;
        }
        if let Some(ticket) = rule.open_leak {
            assert_eq!(rule.expect, Expect::Forbidden, "{method} {template} 的 open_leak 只用於政策 403");
            let label = if ticket.is_empty() { "無票" } else { ticket };
            assert_eq!(
                got,
                Expect::Allow,
                "{method} {template} 標成仍放行（{label}）但現在是 {status}，拿掉 open_leak 並寫上 403 或 404"
            );
        } else {
            assert_eq!(got, rule.expect, "{method} {template} 期望 {:?} 實際 {status}", rule.expect);
        }
    }
    for (rule, hit) in table.iter().zip(seen) {
        if !hit {
            misses.push(format!("UNUSED {} {}", rule.method, rule.path));
        }
    }
    assert!(misses.is_empty(), "允許表與 router 不一致：\n{}", misses.join("\n"));
}
