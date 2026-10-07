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

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::db;

/// Hints older than this are certainly stale — reconcile runs far more often than this, so a hint
/// still unconsumed after this long either already got adopted through another path or never
/// panned out. Dropping it also means a pane id herdr later recycles for something unrelated can
/// never be matched against a hint that has nothing to do with it.
pub const MAX_AGE_SECS: i64 = 10 * 60;

pub(crate) fn cutoff() -> String {
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

fn append_pane_ids(output: &str, pane_ids: &mut Vec<String>, seen: &mut HashSet<String>) {
    let mut stream = serde_json::Deserializer::from_str(output).into_iter::<Value>();
    while let Some(Ok(value)) = stream.next() {
        if let Some(pane_id) = pane_id_from_herdr_json(&value) {
            if seen.insert(pane_id.clone()) {
                pane_ids.push(pane_id);
            }
        }
    }

    // If shell output surrounds the JSON stream, still accept valid JSON Lines after/between it.
    // This also recovers responses after one unrelated line without treating that line as a hint.
    for line in output.lines() {
        if let Ok(value) = serde_json::from_str::<Value>(line.trim()) {
            if let Some(pane_id) = pane_id_from_herdr_json(&value) {
                if seen.insert(pane_id.clone()) {
                    pane_ids.push(pane_id);
                }
            }
        }
    }
}

/// The Bash tool's `PostToolUse` payload: `tool_response` is normally `{stdout, stderr, ...}`, but
/// a hook implementation may flatten it to a bare string — try both shapes, and both streams
/// (herdr prints its JSON envelope to stdout in every case observed, but nothing here assumes it
/// never lands on stderr). Bash loops can emit several JSON values, each of which may contain a
/// spawn response.
pub fn extract_pane_ids(payload: &Value) -> Vec<String> {
    if payload.get("tool_name").and_then(|v| v.as_str()) != Some("Bash") {
        return Vec::new();
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
    let mut pane_ids = Vec::new();
    let mut seen = HashSet::new();
    for output in candidates {
        append_pane_ids(output, &mut pane_ids, &mut seen);
    }
    pane_ids
}

/// Record (or replace) which bot's own tool call just produced `pane_id`. Pane IDs are only unique
/// within a host, so a later insert for the same `(host, pane_id)` is either a retry (harmless
/// overwrite) or that id being recycled for a new pane later (the newer claim is the one that is
/// actually true now). An identical ID on another host is a separate hint.
pub async fn record(app: &impl crate::capabilities::Db, bot_id: &str, pane_id: &str) -> anyhow::Result<()> {
    let host = db::bot_host(app.db(), bot_id).await?;
    sqlx::query(
        "INSERT INTO spawn_hints (pane_id, host, bot_id, created_at) VALUES (?,?,?,?)
         ON CONFLICT(host, pane_id) DO UPDATE SET bot_id=excluded.bot_id, created_at=excluded.created_at",
    )
    .bind(pane_id)
    .bind(&host)
    .bind(bot_id)
    .bind(db::now())
    .execute(app.db())
    .await?;
    Ok(())
}

/// Every still-fresh hint for this host, as `pane_id -> bot_id`. Called once per `reconcile_host`
/// pass; the caller looks up at most one entry per unclaimed agent.
pub async fn for_host(app: &impl crate::capabilities::Db, host: &str) -> anyhow::Result<HashMap<String, String>> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT pane_id, bot_id FROM spawn_hints WHERE host = ? AND created_at >= ?")
            .bind(host)
            .bind(cutoff())
            .fetch_all(app.db())
            .await?;
    Ok(rows.into_iter().collect())
}

/// A hint that actually decided an adoption is spent — not required for correctness (the pane's
/// agent is `claimed` either way, so nothing looks at this host's hint again), just hygiene.
pub async fn consume(app: &impl crate::capabilities::Db, host: &str, pane_id: &str) {
    let _ = sqlx::query("DELETE FROM spawn_hints WHERE host = ? AND pane_id = ?")
        .bind(host)
        .bind(pane_id)
        .execute(app.db())
        .await;
}

/// Hints nobody ever consumed (the spawn failed, or reconcile never got to it in time). Run once
/// per `reconcile_host` pass — cheap, keeps the table from growing unbounded.
pub async fn prune_stale(app: &impl crate::capabilities::Db) {
    let _ = sqlx::query("DELETE FROM spawn_hints WHERE created_at < ?").bind(cutoff()).execute(app.db()).await;
}
