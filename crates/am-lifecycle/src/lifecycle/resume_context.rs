//! Resume exposes a previous measurement provisionally, without reviving cache warmth.
//! Session/model identity and persistent invalidation boundaries constrain history lookup.
use serde_json::{json, Value};
use sqlx::SqlitePool;

const MARKER: &str = "_agm_resumed_context";
const MODEL: &str = "_agm_context_model";
const INVALIDATED: &str = "_agm_context_invalidated";

fn has_usage(context: &Value) -> bool {
    // Startup redraws contain all-zero counters; rounded 0% with real input is a measurement.
    context.get("used_percentage").and_then(Value::as_f64).is_some_and(|n| n > 0.0)
        || context.get("total_input_tokens").and_then(Value::as_u64).is_some_and(|n| n > 0)
        || context.get("current_usage").and_then(Value::as_object)
            .is_some_and(|u| ["input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"]
                .iter().any(|key| u.get(*key).and_then(Value::as_u64).is_some_and(|n| n > 0)))
}

fn session_matches(status: &Value, session: &str) -> bool {
    let mut found = false;
    for key in ["session_id", MARKER, INVALIDATED] {
        if let Some(id) = status.get(key) {
            found = true;
            if id.as_str() != Some(session) {
                return false;
            }
        }
    }
    // Legacy rows without payload identity are not evidence of this session.
    found
}

fn model_matches(source: &str, target: &str) -> bool {
    if source == target {
        return true;
    }
    // Only a bare alias and a full id in its family are interchangeable. Keep window suffixes
    // and distinct full model versions strict; same-window model changes are conservative too.
    let alias = |m: &str| ["opus", "sonnet", "haiku", "fable"].contains(&m);
    !source.contains('[') && !target.contains('[')
        && (alias(source) || alias(target))
        && crate::models::claude_alias_of(source).is_some()
        && crate::models::claude_alias_of(source) == crate::models::claude_alias_of(target)
}

fn source_model<'a>(status: &'a Value, runtime_model: Option<&'a str>) -> Option<&'a str> {
    status.get(MODEL).and_then(Value::as_str)
        .or_else(|| status.pointer("/model/id").and_then(Value::as_str))
        .or(runtime_model)
}

fn snapshot(previous: &Value, session: &str, model: &str) -> Option<Value> {
    let context = previous.get("context_window")?;
    if !session_matches(previous, session) || !has_usage(context) {
        return None;
    }
    let mut measured = json!({});
    for key in ["used_percentage", "context_window_size"] {
        if let Some(value) = context.get(key).filter(|v| v.is_number()) {
            measured[key] = value.clone();
        }
    }
    // Preserve actual latest-request tokens rather than cumulative process totals.
    let usage = context.get("current_usage").and_then(Value::as_object).map(|u| {
        ["input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"]
            .iter().filter_map(|key| u.get(*key).and_then(Value::as_u64)).sum::<u64>()
    }).filter(|n| *n > 0);
    let estimate = measured["used_percentage"].as_f64().zip(measured["context_window_size"].as_u64())
        .map(|(pct, size)| (pct * size as f64 / 100.0).round() as u64).filter(|n| *n > 0);
    if let Some(tokens) = usage.or(estimate).or_else(|| context.get("total_input_tokens").and_then(Value::as_u64)) {
        measured["total_input_tokens"] = json!(tokens);
    }
    Some(json!({"session_id": session, "context_window": measured, (MARKER): session, (MODEL): model}))
}

fn boundary(session: &str) -> Value {
    json!({"session_id": session, (INVALIDATED): session})
}

pub(super) async fn restore(pool: &SqlitePool, bot: &str, run: &str, session: &str, model: Option<&str>) -> Result<(), sqlx::Error> {
    // Bound the lookup and memory regardless of session age. Missing/older history is unknown.
    // No index/schema change or remote transcript access is needed for this display fallback.
    let previous: Vec<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT status_json, runtime_model FROM runs
         WHERE bot_id = ? AND id != ? AND (native_session_id = ? OR resume_session_id = ?)
         ORDER BY started_at DESC, id DESC LIMIT 64",
    )
    .bind(bot).bind(run).bind(session).bind(session).fetch_all(pool).await?;
    let mut restored = None;
    for (raw, runtime_model) in previous {
        let Some(previous) = raw.and_then(|s| serde_json::from_str::<Value>(&s).ok()) else { continue };
        if !session_matches(&previous, session) {
            continue;
        }
        if previous.get(INVALIDATED).and_then(Value::as_str) == Some(session) {
            // Carry the barrier into this run too: stopping before any StatusLine cannot
            // make the next resume jump back over this compaction/session/window boundary.
            restored = Some(boundary(session));
            break;
        }
        if !previous.get("context_window").is_some_and(has_usage) {
            continue;
        }
        // A measured model change ends the search too; older compatible rows are stale.
        if let Some((source, _)) = source_model(&previous, runtime_model.as_deref()).zip(model)
            .filter(|(source, target)| model_matches(source, target)
                && runtime_model.as_deref().is_none_or(|launch| model_matches(launch, target)))
        {
            restored = snapshot(&previous, session, source);
        }
        break;
    }
    if let Some(status) = restored {
        sqlx::query("UPDATE runs SET status_json = ? WHERE id = ? AND status_json IS NULL")
            .bind(status.to_string()).bind(run).execute(pool).await?;
    }
    Ok(())
}

pub(crate) fn is_restored(status: Option<&str>) -> bool {
    status.and_then(|s| serde_json::from_str::<Value>(s).ok())
        .is_some_and(|v| v.get(MARKER).is_some() || v.get(INVALIDATED).is_some())
}

/// Blank redraws can retain compatible provisional context or the invalidation boundary.
/// A real measurement replaces both. New window/model information takes priority over history.
pub(crate) fn retain_until_usage(incoming: &mut Value, previous: Option<&str>) {
    let Some(previous) = previous.and_then(|v| serde_json::from_str::<Value>(v).ok()) else { return };
    let Some(session) = incoming.get("session_id").and_then(Value::as_str).map(str::to_owned) else {
        // An unidentified redraw cannot validate a measurement, but must not erase a barrier
        // and let a future resume fall back across an earlier compaction.
        if let Some(session) = previous.get(INVALIDATED).or_else(|| previous.get(MARKER)).and_then(Value::as_str) {
            incoming[INVALIDATED] = json!(session);
        }
        return;
    };
    if !session_matches(&previous, &session) || incoming.get("context_window").is_some_and(has_usage) {
        return;
    }
    if previous.get(INVALIDATED).is_some() {
        incoming[INVALIDATED] = json!(session);
        return;
    }
    if previous.get(MARKER).is_none() {
        return;
    }
    let window_changed = incoming.pointer("/context_window/context_window_size").and_then(Value::as_u64)
        .is_some_and(|size| previous.pointer("/context_window/context_window_size").and_then(Value::as_u64) != Some(size));
    let model_changed = incoming.pointer("/model/id").and_then(Value::as_str)
        .is_some_and(|model| source_model(&previous, None).is_none_or(|old| !model_matches(old, model)));
    if window_changed || model_changed {
        incoming[INVALIDATED] = json!(session);
        return;
    }
    incoming["context_window"] = previous["context_window"].clone();
    incoming[MARKER] = json!(session);
    incoming[MODEL] = previous[MODEL].clone();
}

/// Retain a persistent barrier even when no provisional measurement was present.
pub(crate) async fn invalidate(pool: &SqlitePool, run: &str, session: &str) -> Result<(), sqlx::Error> {
    let raw: Option<String> = sqlx::query_scalar("SELECT status_json FROM runs WHERE id = ?").bind(run).fetch_one(pool).await?;
    let mut status = raw.and_then(|s| serde_json::from_str::<Value>(&s).ok()).filter(Value::is_object).unwrap_or_else(|| json!({}));
    let object = status.as_object_mut().expect("object");
    for key in ["context_window", MARKER, MODEL] {
        object.remove(key);
    }
    status["session_id"] = json!(session);
    status[INVALIDATED] = json!(session);
    sqlx::query("UPDATE runs SET status_json = ? WHERE id = ?")
        .bind(status.to_string()).bind(run).execute(pool).await?;
    Ok(())
}

pub(crate) async fn on_identity(pool: &SqlitePool, run: &crate::db::Run, session: Option<&str>, compact: bool) -> Result<(), sqlx::Error> {
    // Compact may omit the payload id; the current run still identifies the session whose
    // measurement is no longer valid. Never let an incomplete compact hook revive history.
    let session = if compact {
        session.or(run.native_session_id.as_deref()).or(run.resume_session_id.as_deref())
    } else {
        session
    };
    let Some(session) = session else { return Ok(()) };
    let old: Option<Value> = run.status_json.as_deref().and_then(|s| serde_json::from_str(s).ok());
    let changed = old.as_ref().map(|s| !session_matches(s, session))
        .unwrap_or_else(|| run.native_session_id.as_deref().is_some_and(|sid| sid != session));
    if compact || changed {
        invalidate(pool, &run.id, session).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn measured(session: &str) -> Value {
        json!({"session_id": session, "model": {"id": "opus"},
            "context_window": {"used_percentage": 42, "context_window_size": 1000000,
                "total_input_tokens": 9999999, "current_usage": {"input_tokens": 420000}},
            "prompt_cache": {"warm": true}, "cost": {"total_cost_usd": 12},
            "account_email": "old@example.com", "rate_limits": {"five_hour": {"used_percentage": 99}}})
    }

    #[test]
    fn restores_only_context_with_strict_payload_session_identity() {
        let old = measured("s1");
        let restored = snapshot(&old, "s1", "opus").unwrap();
        assert_eq!(restored, json!({"session_id": "s1", "context_window": {
            "used_percentage": 42, "context_window_size": 1000000, "total_input_tokens": 420000},
            (MARKER): "s1", (MODEL): "opus"}));
        assert!(snapshot(&old, "s2", "opus").is_none());
        let mut legacy = old.clone();
        legacy.as_object_mut().unwrap().remove("session_id");
        assert!(snapshot(&legacy, "s1", "opus").is_none(), "legacy data without payload identity is unknown");
        let mut conflicting = restored.clone();
        conflicting[MARKER] = json!("other");
        assert!(snapshot(&conflicting, "s1", "opus").is_none());
        assert!(snapshot(&restored, "s1", "opus").is_some(), "validated provisional data can survive another resume");
    }

    #[test]
    fn blank_redraw_preserves_context_until_real_input_even_rounded_zero() {
        let restored = snapshot(&measured("s1"), "s1", "opus").unwrap().to_string();
        for pct in [Value::Null, json!(0)] {
            let mut blank = json!({"session_id": "s1", "context_window": {
                "used_percentage": pct, "total_input_tokens": 0, "total_output_tokens": 0,
                "context_window_size": 1000000}, "model": {"id": "opus"}});
            retain_until_usage(&mut blank, Some(&restored));
            assert_eq!(blank["context_window"]["used_percentage"], 42);
            assert_eq!(blank[MARKER], "s1");
        }
        for pct in [0, 23] {
            let mut fresh = json!({"session_id": "s1", "context_window": {"used_percentage": pct, "total_input_tokens": 300}});
            let expected = fresh.clone();
            retain_until_usage(&mut fresh, Some(&restored));
            assert_eq!(fresh, expected);
        }
        let mut other = json!({"session_id": "other"});
        retain_until_usage(&mut other, Some(&restored));
        assert!(other.get("context_window").is_none());
    }

    #[test]
    fn a_new_window_or_model_invalidates_the_old_ratio_and_keeps_a_barrier() {
        let restored = snapshot(&measured("s1"), "s1", "opus").unwrap().to_string();
        for (model, size) in [("opus", 200000), ("sonnet", 1000000)] {
            let mut incoming = json!({"session_id": "s1", "model": {"id": model},
                "context_window": {"used_percentage": 0, "total_input_tokens": 0, "context_window_size": size}});
            retain_until_usage(&mut incoming, Some(&restored));
            assert_eq!(incoming["context_window"]["context_window_size"], size);
            assert_ne!(incoming["context_window"]["used_percentage"], 42);
            assert!(incoming.get(MARKER).is_none());
            assert_eq!(incoming[INVALIDATED], "s1");
            let mut blank = json!({"session_id": "s1"});
            retain_until_usage(&mut blank, Some(&incoming.to_string()));
            assert_eq!(blank[INVALIDATED], "s1", "blank redraw cannot erase the invalidation boundary");
            let mut unidentified = json!({"context_window": {"used_percentage": 0}});
            retain_until_usage(&mut unidentified, Some(&blank.to_string()));
            assert_eq!(unidentified[INVALIDATED], "s1", "an unidentified redraw cannot erase the barrier");
        }
        assert!(model_matches("opus", "claude-opus-4-6"));
        assert!(!model_matches("opus[1m]", "opus"));
        assert!(!model_matches("claude-opus-4-6", "claude-opus-4-5"));
    }
}

#[cfg(all(test, feature = "daemon-test-harness"))]
mod database_tests {
    use super::*;
    use crate::capabilities::Db;
    use crate::{db, testing};

    async fn row(pool: &SqlitePool, id: &str, bot: &str, sid: &str, status: Option<Value>, at: &str) {
        sqlx::query("INSERT INTO runs (id, bot_id, state, native_session_id, status_json, started_at)
            VALUES (?,?,'stopped',?,?,?)").bind(id).bind(bot).bind(sid)
            .bind(status.map(|s| s.to_string())).bind(at).execute(pool).await.unwrap();
    }

    fn context(session: &str, pct: u64) -> Value {
        json!({"session_id": session, "model": {"id": "opus"},
            "context_window": {"used_percentage": pct, "context_window_size": 1000000}})
    }

    async fn status(pool: &SqlitePool, id: &str) -> Option<Value> {
        db::run(pool, id).await.unwrap().unwrap().status_json.map(|s| serde_json::from_str(&s).unwrap())
    }

    #[tokio::test]
    async fn history_lookup_is_scoped_and_rejects_session_model_and_unknown_identity() {
        let e = testing::env().await;
        let bot = testing::claude_bot(&e.app, &e.project_id, "context-source").await;
        let other = testing::claude_bot(&e.app, &e.project_id, "context-other").await;
        row(e.app.db(), "old", &bot.id, "same", Some(context("same", 42)), "2026-09-01").await;
        row(e.app.db(), "empty", &bot.id, "same", Some(context("same", 0)), "2026-09-02").await;
        row(e.app.db(), "other-session", &bot.id, "other", Some(context("other", 77)), "2026-09-03").await;
        row(e.app.db(), "other-bot", &other.id, "same", Some(context("same", 99)), "2026-09-04").await;
        // Row session agrees but payload identity does not; this must never become 77% for "same".
        row(e.app.db(), "wrong-payload", &bot.id, "same", Some(context("other", 77)), "2026-09-05").await;
        row(e.app.db(), "new", &bot.id, "same", None, "2026-09-06").await;
        restore(e.app.db(), &bot.id, "new", "same", Some("opus")).await.unwrap();
        assert_eq!(status(e.app.db(), "new").await.unwrap()["context_window"]["used_percentage"], 42);
        for model in [Some("sonnet"), Some("opus[1m]"), None] {
            sqlx::query("UPDATE runs SET status_json=NULL WHERE id='new'").execute(e.app.db()).await.unwrap();
            restore(e.app.db(), &bot.id, "new", "same", model).await.unwrap();
            assert!(status(e.app.db(), "new").await.is_none(), "changed/unknown startup model is conservative: {model:?}");
        }
        restore(e.app.db(), &bot.id, "new", "missing", Some("opus")).await.unwrap();
        assert!(status(e.app.db(), "new").await.is_none());
        // No matching payload identity, even when the row claims the target.
        row(e.app.db(), "legacy", &bot.id, "legacy", Some(json!({"model": {"id": "opus"},
            "context_window": {"used_percentage": 77}})), "2026-09-07").await;
        restore(e.app.db(), &bot.id, "new", "legacy", Some("opus")).await.unwrap();
        assert!(status(e.app.db(), "new").await.is_none());
    }

    #[tokio::test]
    async fn a_compact_boundary_survives_restore_stop_and_another_restore() {
        let e = testing::env().await;
        let bot = testing::claude_bot(&e.app, &e.project_id, "context-compact").await;
        row(e.app.db(), "a", &bot.id, "same", Some(context("same", 42)), "2026-09-01").await;
        row(e.app.db(), "b", &bot.id, "same", None, "2026-09-02").await;
        restore(e.app.db(), &bot.id, "b", "same", Some("opus")).await.unwrap();
        assert_eq!(status(e.app.db(), "b").await.unwrap()["context_window"]["used_percentage"], 42);
        let b = db::run(e.app.db(), "b").await.unwrap().unwrap();
        on_identity(e.app.db(), &b, None, true).await.unwrap();
        // All fixture runs are stopped: the history boundary, not active-run state, protects C/D.
        row(e.app.db(), "c", &bot.id, "same", None, "2026-09-03").await;
        restore(e.app.db(), &bot.id, "c", "same", Some("opus")).await.unwrap();
        let c = status(e.app.db(), "c").await.unwrap();
        assert!(c.get("context_window").is_none());
        assert_eq!(c[INVALIDATED], "same");
        let mut blank = json!({"session_id": "same", "context_window": {"used_percentage": 0}});
        retain_until_usage(&mut blank, Some(&c.to_string()));
        sqlx::query("UPDATE runs SET status_json=? WHERE id='c'").bind(blank.to_string()).execute(e.app.db()).await.unwrap();
        row(e.app.db(), "d", &bot.id, "same", None, "2026-09-04").await;
        restore(e.app.db(), &bot.id, "d", "same", Some("opus")).await.unwrap();
        assert!(status(e.app.db(), "d").await.unwrap().get("context_window").is_none());
        // A genuine post-compact reading lifts the barrier and becomes the new source.
        sqlx::query("UPDATE runs SET status_json=? WHERE id='c'").bind(context("same", 7).to_string()).execute(e.app.db()).await.unwrap();
        sqlx::query("UPDATE runs SET status_json=NULL WHERE id='d'").execute(e.app.db()).await.unwrap();
        restore(e.app.db(), &bot.id, "d", "same", Some("opus")).await.unwrap();
        assert_eq!(status(e.app.db(), "d").await.unwrap()["context_window"]["used_percentage"], 7);
    }

    #[tokio::test]
    async fn history_lookup_does_not_read_past_64_candidates() {
        let e = testing::env().await;
        let bot = testing::claude_bot(&e.app, &e.project_id, "context-bound").await;
        row(e.app.db(), "old", &bot.id, "same", Some(context("same", 42)), "0000").await;
        for i in 0..64 {
            row(e.app.db(), &format!("empty-{i:03}"), &bot.id, "same", None, &format!("1000-{i:03}")).await;
        }
        row(e.app.db(), "new", &bot.id, "same", None, "9999").await;
        restore(e.app.db(), &bot.id, "new", "same", Some("opus")).await.unwrap();
        assert!(status(e.app.db(), "new").await.is_none());
    }
}
