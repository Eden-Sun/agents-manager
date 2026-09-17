//! Spawn hints (issue #94): a parent bot's own Bash tool calls to `herdr pane split` / `herdr
//! agent start` print herdr's own JSON-RPC-shaped response to stdout —
//! `{"id":"cli:pane:split","result":{"pane":{"pane_id":"w1:p2",...}},...}` /
//! `{"id":"cli:agent:start","result":{"agent":{"pane_id":"w1:p2","name":"parent-kid",...}},...}`
//! (verified against a live herdr 0.8.2 session, 2026-09-18). A `PostToolUse` hook on the Bash tool
//! sees that stdout and hands the daemon a direct, unambiguous `pane_id -> bot_id` fact: "I just
//! created this exact pane." `reconcile::adopt_child`'s §6.5a blood-line/prefix inference stays the
//! fallback — a hint is consulted *first*, but only for the one pane it actually names, and it
//! never fabricates an agent that isn't really there (the pane-scan loop still decides whether
//! anything is actually sitting at that pane_id).
//!
//! Only the top-level bot that ran the command ever produces a hint: `managed_by='child'` bots have
//! no hooks at all (§4.3, `inject_hooks = 0`), so a child spawning a grandchild still goes through
//! the same blood-line path this issue does not touch.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

use crate::db;
use crate::state::App;

/// Hints older than this are certainly stale — reconcile runs far more often than this, so a hint
/// still unconsumed after this long either already got adopted through another path or never
/// panned out. Dropping it also means a pane id herdr later recycles for something unrelated can
/// never be matched against a hint that has nothing to do with it.
const MAX_AGE_SECS: i64 = 10 * 60;

fn cutoff() -> String {
    (chrono::Utc::now() - chrono::Duration::seconds(MAX_AGE_SECS)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// `id` is herdr's own request echo (`cli:<resource>:<verb>`) — the one unambiguous way to tell
/// "this pane was just created" (`pane:split`, `agent:start`) apart from a pane the agent merely
/// *looked at* (`pane:get`, `pane:current`, `pane:list`, …), which returns the exact same
/// `{"pane": {...}}` shape. Trusting the `type` field alone (`"pane_info"`) would treat every
/// inspection as a spawn.
fn pane_id_from_herdr_json(v: &Value) -> Option<String> {
    let pane = match v.get("id").and_then(|s| s.as_str())? {
        "cli:pane:split" => v.pointer("/result/pane/pane_id"),
        "cli:agent:start" => v.pointer("/result/agent/pane_id"),
        _ => None,
    }?;
    pane.as_str().map(String::from)
}

/// The Bash tool's `PostToolUse` payload: `tool_response` is normally `{stdout, stderr, ...}`, but
/// a hook implementation may flatten it to a bare string — try both shapes, and both streams
/// (herdr prints its JSON envelope to stdout in every case observed, but nothing here assumes it
/// never lands on stderr).
pub(crate) fn extract_pane_id(payload: &Value) -> Option<String> {
    if payload.get("tool_name").and_then(|v| v.as_str()) != Some("Bash") {
        return None;
    }
    let mut candidates: Vec<&str> = Vec::new();
    if let Some(s) = payload.get("tool_response").and_then(|v| v.as_str()) {
        candidates.push(s);
    }
    if let Some(s) = payload.pointer("/tool_response/stdout").and_then(|v| v.as_str()) {
        candidates.push(s);
    }
    if let Some(s) = payload.pointer("/tool_response/stderr").and_then(|v| v.as_str()) {
        candidates.push(s);
    }
    candidates.into_iter().find_map(|s| serde_json::from_str::<Value>(s.trim()).ok().as_ref().and_then(pane_id_from_herdr_json))
}

/// Record (or replace) which bot's own tool call just produced `pane_id`. One row per pane —
/// `pane_id` is unique within a herdr session, and a pane can only ever be legitimately created
/// once, so a later insert for the same id is either a retry (harmless overwrite) or that id being
/// recycled for a new pane later (the newer claim is the one that is actually true now).
pub(crate) async fn record(app: &Arc<App>, bot_id: &str, pane_id: &str) -> anyhow::Result<()> {
    let host = db::bot_host(&app.db, bot_id).await?;
    sqlx::query(
        "INSERT INTO spawn_hints (pane_id, host, bot_id, created_at) VALUES (?,?,?,?)
         ON CONFLICT(pane_id) DO UPDATE SET host=excluded.host, bot_id=excluded.bot_id, created_at=excluded.created_at",
    )
    .bind(pane_id)
    .bind(&host)
    .bind(bot_id)
    .bind(db::now())
    .execute(&app.db)
    .await?;
    Ok(())
}

/// Every still-fresh hint for this host, as `pane_id -> bot_id`. Called once per `reconcile_host`
/// pass; the caller looks up at most one entry per unclaimed agent.
pub(crate) async fn for_host(app: &Arc<App>, host: &str) -> anyhow::Result<HashMap<String, String>> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT pane_id, bot_id FROM spawn_hints WHERE host = ? AND created_at >= ?")
            .bind(host)
            .bind(cutoff())
            .fetch_all(&app.db)
            .await?;
    Ok(rows.into_iter().collect())
}

/// A hint that actually decided an adoption is spent — not required for correctness (the pane's
/// agent is `claimed` either way, so nothing looks at this pane's hint again), just hygiene.
pub(crate) async fn consume(app: &Arc<App>, pane_id: &str) {
    let _ = sqlx::query("DELETE FROM spawn_hints WHERE pane_id = ?").bind(pane_id).execute(&app.db).await;
}

/// Hints nobody ever consumed (the spawn failed, or reconcile never got to it in time). Run once
/// per `reconcile_host` pass — cheap, keeps the table from growing unbounded.
pub(crate) async fn prune_stale(app: &Arc<App>) {
    let _ = sqlx::query("DELETE FROM spawn_hints WHERE created_at < ?").bind(cutoff()).execute(&app.db).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bash_result(id: &str, stdout: Value) -> Value {
        json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "herdr agent start kid --kind claude --pane w1:p2"},
            "tool_response": {"stdout": serde_json::to_string(&json!({"id": id, "result": stdout})).unwrap(), "stderr": ""},
        })
    }

    #[test]
    fn agent_start_and_pane_split_both_yield_the_pane_id() {
        let start = bash_result("cli:agent:start", json!({"agent": {"pane_id": "w1:p2", "name": "parent-kid"}}));
        assert_eq!(extract_pane_id(&start).as_deref(), Some("w1:p2"));

        let split = bash_result("cli:pane:split", json!({"pane": {"pane_id": "w1:p3", "tab_id": "w1:t1"}}));
        assert_eq!(extract_pane_id(&split).as_deref(), Some("w1:p3"));
    }

    /// `pane:get`／`pane:current`／`pane:list` return the exact same `{"pane": {...}}` shape as
    /// `pane:split` — an agent merely *looking at* a pane it does not own must never be read as
    /// "I just created this". Only the request `id` tells the two apart.
    #[test]
    fn merely_inspecting_a_pane_is_not_a_spawn() {
        for id in ["cli:pane:get", "cli:pane:current", "cli:pane:list", "cli:agent:get", "cli:agent:list"] {
            let v = bash_result(id, json!({"pane": {"pane_id": "w1:p2"}}));
            assert_eq!(extract_pane_id(&v), None, "{id} must not be treated as a spawn");
        }
    }

    /// herdr's own error envelope (`{"error":{...},"id":"cli:agent:start"}`, e.g. a busy pane or a
    /// timeout) has no `result` at all — no pane was actually created, so there is nothing to hint.
    #[test]
    fn a_herdr_error_response_yields_no_hint() {
        let v = json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "herdr agent start kid --kind claude --pane w1:p2"},
            "tool_response": {"stdout": r#"{"error":{"code":"agent_pane_busy","message":"..."},"id":"cli:agent:start"}"#, "stderr": ""},
        });
        assert_eq!(extract_pane_id(&v), None);
    }

    #[test]
    fn unrelated_bash_output_and_non_bash_tools_yield_no_hint() {
        let ls = json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "ls -la"},
            "tool_response": {"stdout": "total 0\ndrwxr-xr-x  2 x  x  64 Jan  1 00:00 .\n", "stderr": ""},
        });
        assert_eq!(extract_pane_id(&ls), None, "plain command output is not JSON at all");

        let not_bash = json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Read",
            "tool_input": {"file_path": "/tmp/x"},
            "tool_response": {"stdout": r#"{"id":"cli:agent:start","result":{"agent":{"pane_id":"w1:p2"}}}"#},
        });
        assert_eq!(extract_pane_id(&not_bash), None, "only the Bash tool is trusted");
    }

    /// A hook implementation that flattens `tool_response` to a bare string instead of `{stdout,
    /// stderr}` must still work — the field's shape is not part of this daemon's own contract.
    #[test]
    fn a_flattened_string_tool_response_still_works() {
        let v = json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "herdr agent start kid --kind claude --pane w1:p2"},
            "tool_response": r#"{"id":"cli:agent:start","result":{"agent":{"pane_id":"w1:p2"}}}"#,
        });
        assert_eq!(extract_pane_id(&v).as_deref(), Some("w1:p2"));
    }

    #[tokio::test]
    async fn a_recorded_hint_is_visible_by_host_and_disappears_once_consumed() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "alfa").await;

        record(&env.app, &bot.id, "w1:p2").await.unwrap();
        let hints = for_host(&env.app, crate::config::LOCAL_HOST).await.unwrap();
        assert_eq!(hints.get("w1:p2"), Some(&bot.id));
        assert!(for_host(&env.app, "some-other-host").await.unwrap().is_empty(), "scoped by host");

        consume(&env.app, "w1:p2").await;
        assert!(for_host(&env.app, crate::config::LOCAL_HOST).await.unwrap().is_empty());
    }

    /// Same pane id recorded twice (a retried Bash call, or the id recycled later): the row is
    /// replaced, not duplicated, and the newest claim wins.
    #[tokio::test]
    async fn recording_the_same_pane_twice_replaces_rather_than_duplicates() {
        let env = crate::testing::env().await;
        let first = crate::testing::claude_bot(&env.app, &env.project_id, "alfa").await;
        let second = crate::testing::claude_bot(&env.app, &env.project_id, "bravo").await;

        record(&env.app, &first.id, "w1:p2").await.unwrap();
        record(&env.app, &second.id, "w1:p2").await.unwrap();

        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM spawn_hints WHERE pane_id = 'w1:p2'").fetch_one(&env.app.db).await.unwrap();
        assert_eq!(n, 1, "one row per pane_id");
        let hints = for_host(&env.app, crate::config::LOCAL_HOST).await.unwrap();
        assert_eq!(hints.get("w1:p2"), Some(&second.id), "the newer claim wins");
    }

    /// A hint older than the staleness window must not surface, and `prune_stale` removes it.
    #[tokio::test]
    async fn a_stale_hint_is_invisible_and_gets_pruned() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "alfa").await;
        let old = (chrono::Utc::now() - chrono::Duration::seconds(MAX_AGE_SECS + 60)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        sqlx::query("INSERT INTO spawn_hints (pane_id, host, bot_id, created_at) VALUES ('w1:p9', 'local', ?, ?)")
            .bind(&bot.id)
            .bind(&old)
            .execute(&env.app.db)
            .await
            .unwrap();

        assert!(!for_host(&env.app, crate::config::LOCAL_HOST).await.unwrap().contains_key("w1:p9"), "too old to trust");

        prune_stale(&env.app).await;
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM spawn_hints WHERE pane_id = 'w1:p9'").fetch_one(&env.app.db).await.unwrap();
        assert_eq!(n, 0, "prune actually removes it");
    }
}
