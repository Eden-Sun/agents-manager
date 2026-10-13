use std::sync::Arc;

use crate::db;
use crate::state::App;
use crate::capabilities::{BotLocks, BotStatusEmit, HerdrRoutes};

/// Bind a newly launched Codex from Herdr's explicit session identity, never the newest rollout.
/// The sweep also heals missed startup status edges, with a screen guard on #732's folded idle.
async fn bind_codex_session(app: &Arc<App>, run: &db::Run) {
    let Some(pane) = run.pane_id.as_deref() else { return };
    let Some(client) = app.herdr_for_run(run).await else { return };
    let Ok(Some(agent)) = client.agent_get(pane).await else { return };
    if agent.agent.as_deref() != Some("codex") || agent.pane_id != pane {
        return;
    }
    if crate::lifecycle::dead_panes::pane_target_stale(&client, run).await {
        return;
    }
    let Some(sid) = crate::lifecycle::dead_panes::herdr_session_id(&agent)
        .filter(|s| s.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))) else { return };
    // Existing hook evidence wins if the pane changed its session during this read.
    if run.native_session_id.as_deref().is_some_and(|known| known != sid) {
        return;
    }
    if run.agent_status == "unknown" && agent.agent_status.normalized().as_str() != "unknown" {
        // Keep #732's unknown→idle compatibility, but never synthesize an idle edge
        // while the actual screen is working. Startup launch_pending unknown remains unknown.
        let status = agent.agent_status.normalized().as_str();
        let can_heal = status != "idle" || client.pane_read(pane, "recent_unwrapped", 200).await
            .is_ok_and(|read| !crate::lifecycle::poller::is_screen_busy(&read.text));
        if let Some(host) = db::bot_host(&app.db, &run.bot_id).await.ok().filter(|_| can_heal) {
            let session = run.herdr_session.as_deref().unwrap_or(&app.herdr_session);
            let ev = crate::herdr::Event {
                event: "pane.agent_status_changed".into(),
                data: serde_json::json!({"pane_id": pane, "agent": "codex", "agent_status": agent.agent_status.normalized().as_str()}),
            };
            crate::runners::events::handle_status(app, &host, session, &ev).await;
        }
    }
    let path = if run.transcript_path.is_none() && db::bot_host(&app.db, &run.bot_id).await.ok().as_deref() == Some(crate::config::LOCAL_HOST) {
        match db::bot(&app.db, &run.bot_id).await {
            Ok(Some(bot)) => match crate::lifecycle::codex_home(app, &bot).await {
                Some(home) => crate::lifecycle::codex_session_log_async(home, sid.clone()).await.map(|p| p.to_string_lossy().into_owned()),
                None => None,
            },
            _ => None,
        }
    } else { None };
    if run.native_session_id.as_deref() == Some(&sid) && path.is_none() {
        return;
    }
    let wrote = sqlx::query("UPDATE runs SET native_session_id=?, transcript_path=COALESCE(transcript_path, ?) WHERE id=? AND state='running' AND pane_id=? AND (native_session_id IS NULL OR native_session_id=?)")
        .bind(&sid).bind(path).bind(&run.id).bind(pane).bind(&sid).execute(&app.db).await;
    if wrote.is_ok_and(|r| r.rows_affected() > 0) {
        app.emit_bot_status(&run.bot_id).await;
    }
}

/// 巡邏先從 herdr 補 Codex session，再讀該 session 的 rollout token_count。
/// 遠端也可綁 session；rollout 路徑只查本機且只按 session id 找。
pub async fn refresh_codex(app: &Arc<App>, run: &db::Run) {
    if run.native_session_id.is_none() || run.transcript_path.is_none() || run.agent_status == "unknown" {
        bind_codex_session(app, run).await;
    }
    let fresh = db::run(&app.db, &run.id).await.ok().flatten().unwrap_or_else(|| run.clone());
    // An adopted CLI can reject its first prompt before Herdr ever observes working/idle.
    // No fallback timer or in-flight turn exists then; record the error without inventing a reply.
    if fresh.adopted != 0 && matches!(fresh.agent_status.as_str(), "unknown" | "idle" | "done") {
        let lock = app.bot_lock(&fresh.bot_id).await;
        let _guard = lock.lock().await;
        if let Ok(Some(current)) = db::run(&app.db, &fresh.id).await {
            if current.state == "running" && current.adopted != 0 && matches!(current.agent_status.as_str(), "unknown" | "idle" | "done")
                && db::in_flight_turn(&app.db, &current.id).await.is_ok_and(|t| t.is_none())
            {
                let idle_screen = match (current.pane_id.as_deref(), app.herdr_for_run(&current).await) {
                    (Some(pane), Some(client)) => !crate::lifecycle::dead_panes::pane_target_stale(&client, &current).await
                        && client.pane_read(pane, "recent_unwrapped", 200).await.is_ok_and(|read| !crate::lifecycle::poller::is_screen_busy(&read.text)),
                    _ => false,
                };
                if idle_screen {
                    if let Err(error) = crate::turn_error::capture(app, &current.bot_id, &current.id).await {
                        tracing::debug!(run = %current.id, ?error, "adopted unknown Codex error capture failed; retrying next sweep");
                    }
                }
            }
        }
    }
    crate::app_ports_r2a8::refresh_codex(app, &fresh).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing as tt;
    use serde_json::json;

    #[tokio::test]
    async fn herdr_binds_the_exact_codex_session_and_heals_a_missed_working_edge() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "codex-binding").await;
        let home = env.dir.join("codex-home");
        let day = home.join("sessions/2026/10/13");
        std::fs::create_dir_all(&day).unwrap();
        let log = day.join("rollout-2026-10-13T10-00-00-session-current.jsonl");
        std::fs::write(&log, "").unwrap();
        std::fs::write(day.join("rollout-2026-10-13T11-00-00-session-newest.jsonl"), "").unwrap();
        sqlx::query("UPDATE bots SET kind='codex', env_json=? WHERE id=?").bind(json!({"CODEX_HOME":home}).to_string()).bind(&bot.id).execute(&app.db).await.unwrap();
        let id = tt::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE runs SET agent_status='unknown' WHERE id=?").bind(&id).execute(&app.db).await.unwrap();
        let pane = format!("pane-{}", bot.id);
        env.herdr.set_unnamed_agent(&pane, Some("session-current"));
        env.herdr.agents.lock().unwrap()[0]["agent"] = json!("codex");
        bind_codex_session(&app, &db::run(&app.db, &id).await.unwrap().unwrap()).await;
        let run = db::run(&app.db, &id).await.unwrap().unwrap();
        assert_eq!(run.agent_status, "working");
        assert_eq!(run.native_session_id.as_deref(), Some("session-current"));
        assert_eq!(run.transcript_path.as_deref(), std::fs::canonicalize(log).unwrap().to_str());
    }

    #[tokio::test]
    async fn an_unbound_or_changed_codex_session_is_not_guessed_from_newest_rollout() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "unbound-codex").await;
        sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        let id = tt::fake_run(&app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        env.herdr.set_unnamed_agent(&pane, None);
        env.herdr.agents.lock().unwrap()[0]["agent"] = json!("codex");
        bind_codex_session(&app, &db::run(&app.db, &id).await.unwrap().unwrap()).await;
        assert!(db::run(&app.db, &id).await.unwrap().unwrap().native_session_id.is_none());
        sqlx::query("UPDATE runs SET native_session_id='hook-session', agent_status='unknown' WHERE id=?").bind(&id).execute(&app.db).await.unwrap();
        env.herdr.agents.lock().unwrap()[0]["agent_session"] = json!({"kind":"id","value":"other-session"});
        bind_codex_session(&app, &db::run(&app.db, &id).await.unwrap().unwrap()).await;
        let run = db::run(&app.db, &id).await.unwrap().unwrap();
        assert_eq!(run.native_session_id.as_deref(), Some("hook-session"));
        assert_eq!(run.agent_status, "unknown", "a foreign session cannot heal this run's status");
    }
    #[tokio::test]
    async fn a_foreign_named_pane_does_not_supply_session_or_status() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "foreign-pane").await;
        sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        let id = tt::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE runs SET adopted=1, agent_status='unknown' WHERE id=?").bind(&id).execute(&app.db).await.unwrap();
        let pane = format!("pane-{}", bot.id);
        env.herdr.set_agent("other-agent", &pane, true);
        env.herdr.agents.lock().unwrap()[0]["agent"] = json!("codex");
        env.herdr.agents.lock().unwrap()[0]["agent_status"] = json!("working");
        env.herdr.set_screen(&pane, "■ {\"type\":\"error\",\"status\":400,\"error\":{\"message\":\"foreign failure\"}}\n› Ask Codex to do anything\n");
        refresh_codex(&app, &db::run(&app.db, &id).await.unwrap().unwrap()).await;
        let run = db::run(&app.db, &id).await.unwrap().unwrap();
        assert!(run.native_session_id.is_none());
        assert!(run.turn_error.is_none(), "foreign pane errors cannot be attached to our run");
        assert_eq!(run.agent_status, "unknown");
    }

    #[tokio::test]
    async fn a_launch_pending_unknown_herdr_status_is_preserved() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "unknown-pane").await;
        sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        let id = tt::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE runs SET agent_status='unknown' WHERE id=?").bind(&id).execute(&app.db).await.unwrap();
        let pane = format!("pane-{}", bot.id);
        env.herdr.set_unnamed_agent(&pane, Some("explicit-session"));
        env.herdr.agents.lock().unwrap()[0]["agent"] = json!("codex");
        env.herdr.agents.lock().unwrap()[0]["agent_status"] = json!("unknown");
        env.herdr.agents.lock().unwrap()[0]["launch_pending"] = json!(true);
        bind_codex_session(&app, &db::run(&app.db, &id).await.unwrap().unwrap()).await;
        let run = db::run(&app.db, &id).await.unwrap().unwrap();
        assert_eq!(run.native_session_id.as_deref(), Some("explicit-session"));
        assert_eq!(run.agent_status, "unknown");
    }

    #[tokio::test]
    async fn an_adopted_unknown_json_rejection_is_recorded_without_a_completed_exchange() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "unknown-error").await;
        sqlx::query("UPDATE bots SET kind='codex', inject_hooks=0 WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        let id = tt::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE runs SET adopted=1, agent_status='unknown' WHERE id=?").bind(&id).execute(&app.db).await.unwrap();
        let pane = format!("pane-{}", bot.id);
        let screen = r#"› 做事
■ {"type":"error","status":400,"error":{"type":"invalid_request_error","message":"The 'gpt-6.1-sol' model is not supported when using Codex with a ChatGPT account."}}
› Ask Codex to do anything
"#;
        env.herdr.set_unnamed_agent(&pane, Some("explicit-session"));
        env.herdr.agents.lock().unwrap()[0]["agent"] = json!("codex");
        env.herdr.agents.lock().unwrap()[0]["agent_status"] = json!("unknown");
        for pending in [true, false] {
            env.herdr.agents.lock().unwrap()[0]["launch_pending"] = json!(pending);
            sqlx::query("UPDATE runs SET agent_status='unknown', turn_error=NULL WHERE id=?").bind(&id).execute(&app.db).await.unwrap();
            let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
            sqlx::query("DELETE FROM messages WHERE conversation_id=?").bind(&conv).execute(&app.db).await.unwrap();
            env.herdr.set_screen(&pane, screen);
            refresh_codex(&app, &db::run(&app.db, &id).await.unwrap().unwrap()).await;
            let run = db::run(&app.db, &id).await.unwrap().unwrap();
            assert_eq!(run.agent_status, if pending { "unknown" } else { "idle" });
            assert_eq!(run.turn_error.as_deref(), Some("API 拒絕這次請求（400 invalid_request_error）：The 'gpt-6.1-sol' model is not supported when using Codex with a ChatGPT account."));
            let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE run_id=?").bind(&id).fetch_one(&app.db).await.unwrap();
            assert_eq!(turns, 0);
            let messages: Vec<(String, String)> = sqlx::query_as("SELECT role,content FROM messages WHERE conversation_id=?").bind(&conv).fetch_all(&app.db).await.unwrap();
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0], ("system".into(), "API 拒絕這次請求（400 invalid_request_error）：The 'gpt-6.1-sol' model is not supported when using Codex with a ChatGPT account.".into()));
            refresh_codex(&app, &run).await;
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id=?").bind(&conv).fetch_one(&app.db).await.unwrap();
            assert_eq!(count, 1, "the next sweep does not duplicate the API error");
        }
    }

    #[tokio::test]
    async fn a_folded_codex_idle_does_not_heal_unknown_while_the_screen_is_busy() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "busy-folded-idle").await;
        sqlx::query("UPDATE bots SET kind='codex', inject_hooks=0 WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        let id = tt::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE runs SET adopted=1, agent_status='unknown' WHERE id=?").bind(&id).execute(&app.db).await.unwrap();
        let pane = format!("pane-{}", bot.id);
        env.herdr.set_unnamed_agent(&pane, Some("explicit-session"));
        env.herdr.agents.lock().unwrap()[0]["agent"] = json!("codex");
        env.herdr.agents.lock().unwrap()[0]["agent_status"] = json!("unknown");
        env.herdr.set_screen(&pane, "› 做事\n• Working (6s • esc to interrupt)\n› Ask Codex to do anything\n");
        refresh_codex(&app, &db::run(&app.db, &id).await.unwrap().unwrap()).await;
        let run = db::run(&app.db, &id).await.unwrap().unwrap();
        assert_eq!(run.native_session_id.as_deref(), Some("explicit-session"));
        assert_eq!(run.agent_status, "unknown");
        assert!(run.turn_error.is_none());
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE run_id=?").bind(&id).fetch_one(&app.db).await.unwrap();
        assert_eq!(turns, 0);
    }

}
