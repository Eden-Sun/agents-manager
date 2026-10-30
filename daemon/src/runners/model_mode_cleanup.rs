//! Mode-specific retirement of idle child agents.
use std::{collections::HashMap, sync::Arc};
use crate::{config::ModelMode, state::App};

#[derive(sqlx::FromRow)]
struct Child { id: String, parent: String, name: String, kind: String, idle_at: String, notified: bool }

pub async fn sweep(app: &Arc<App>) {
    if app.cfg.agents_fresh().await.model_mode != ModelMode::ClaudeCodex { return; }
    if let Err(error) = sweep_inner(app).await { tracing::warn!(?error, "model mode child cleanup failed"); }
}

async fn sweep_inner(app: &Arc<App>) -> anyhow::Result<()> {
    let children: Vec<Child> = sqlx::query_as(
        "SELECT b.id, b.parent_bot_id parent, b.name, b.kind, d.idle_at,
          (d.active=0 AND d.notified_at IS NOT NULL AND d.pending_turn_id IS NULL AND d.notified_background=0
           AND NOT EXISTS(SELECT 1 FROM child_done_sources s WHERE s.child_id=b.id AND s.covered=0)
           AND NOT EXISTS(SELECT 1 FROM turns t JOIN conversations c ON c.id=t.conversation_id WHERE c.bot_id=b.id AND t.status IN ('completed','completed_fallback','failed') AND t.rowid>d.covered_rowid)) notified
         FROM bots b JOIN runs r ON r.bot_id=b.id AND r.state='running'
         JOIN child_done_state d ON d.child_id=b.id
         WHERE b.managed_by='child' AND b.deleted_at IS NULL AND b.parent_bot_id IS NOT NULL
           AND b.kind IN ('claude','codex') AND r.agent_status='idle'
           AND (r.agent_status_since IS NULL OR julianday(r.agent_status_since)<=julianday(d.idle_at))
           AND NOT EXISTS(SELECT 1 FROM turns t WHERE t.run_id=r.id AND t.status='in_flight')
         ORDER BY d.idle_at DESC, b.id"
    ).fetch_all(&app.db).await?;
    let mut codex_counts: HashMap<String, usize> = HashMap::new();
    for child in children {
        let eligible = if child.kind == "claude" {
            chrono::DateTime::parse_from_rfc3339(&child.idle_at).is_ok_and(|t| chrono::Utc::now().signed_duration_since(t).num_seconds() > 70 * 60)
        } else {
            let count = codex_counts.entry(child.parent.clone()).or_default();
            *count += 1;
            *count > 3
        };
        if !eligible || !child.notified { continue; }
        let lock = app.bot_lock(&child.id).await;
        let _guard = lock.lock_owned().await;
        // Completion delivery and idleness may have changed while waiting for the bot lock.
        let safe: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM bots b JOIN runs r ON r.bot_id=b.id AND r.state='running' JOIN child_done_state d ON d.child_id=b.id WHERE b.id=? AND b.deleted_at IS NULL AND r.agent_status='idle' AND (r.agent_status_since IS NULL OR julianday(r.agent_status_since)<=julianday(d.idle_at)) AND d.active=0 AND d.idle_at=? AND d.notified_at IS NOT NULL AND d.pending_turn_id IS NULL AND d.notified_background=0 AND NOT EXISTS(SELECT 1 FROM child_done_sources s WHERE s.child_id=b.id AND s.covered=0) AND NOT EXISTS(SELECT 1 FROM turns t JOIN conversations c ON c.id=t.conversation_id WHERE c.bot_id=b.id AND t.status IN ('completed','completed_fallback','failed') AND t.rowid>d.covered_rowid) AND NOT EXISTS(SELECT 1 FROM turns t WHERE t.run_id=r.id AND t.status='in_flight'))")
            .bind(&child.id).bind(&child.idle_at).fetch_one(&app.db).await?;
        if !safe || app.cfg.agents_fresh().await.model_mode != ModelMode::ClaudeCodex { continue; }
        match crate::lifecycle::stop::stop_bot_locked_if_notified(app, &child.id, &child.idle_at).await {
            Ok(true) => {
                let _ = crate::child_retire::retire(app.as_ref(), &child.id, "model_mode_idle_cleanup", crate::child_retire::Mode::Implicit).await?;
                let conv = crate::db::conversation_id(&app.db, &child.parent).await?;
                let why = if child.kind == "claude" { "閒置超過 70 分鐘" } else { "閒置 codex child 超過 3 顆" };
                let message = format!("模型用法模式：已收掉 child {}（{}）。", child.name, why);
                crate::lifecycle::insert_message(app.as_ref(), &conv, None, "system", &message, "system", false, None).await?;
            }
            Ok(false) => {},
            Err(error) => tracing::warn!(child=%child.name, ?error, "idle child was not stopped"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{db, testing};

    async fn child(env: &testing::Env, parent: &str, kind: &str, age: i64, notified: bool) -> (String, String) {
        let id = db::ulid();
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,args_json,autostart,inject_hooks,hook_token,managed_by,parent_bot_id,created_at) VALUES(?,?,?,?,'[]',0,1,'child-token','child',?,?)")
            .bind(&id).bind(&env.project_id).bind(&id).bind(kind).bind(parent).bind(db::now()).execute(&env.app.db).await.unwrap();
        let run = testing::fake_run(&env.app, &id).await;
        let idle = db::iso_in(-age);
        sqlx::query("INSERT INTO child_done_state(child_id,idle_at,idle_turn_id,initialized_at,active,notified_at) VALUES(?,?,?,?,0,?)")
            .bind(&id).bind(&idle).bind(&run).bind(&idle).bind(notified.then_some(db::now())).execute(&env.app.db).await.unwrap();
        (id, run)
    }

    async fn running(env: &testing::Env, run: &str) -> bool {
        sqlx::query_scalar::<_, String>("SELECT state FROM runs WHERE id=?").bind(run).fetch_one(&env.app.db).await.unwrap() == "running"
    }

    #[tokio::test]
    async fn cleanup_only_runs_in_mode_and_never_closes_unnotified_or_busy_children() {
        let env = testing::env().await;
        let parent = testing::claude_bot(&env.app, &env.project_id, "parent").await;
        let (_, cold) = child(&env, &parent.id, "claude", 4300, true).await;
        let (_, warm) = child(&env, &parent.id, "claude", 4100, true).await;
        let (_, unnotified) = child(&env, &parent.id, "claude", 4300, false).await;
        let (busy_id, busy) = child(&env, &parent.id, "claude", 4300, true).await;
        let conv = db::conversation_id(&env.app.db, &busy_id).await.unwrap();
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES(?,?,?,'web','in_flight','ok',?)")
            .bind(db::ulid()).bind(conv).bind(&busy).bind(db::now()).execute(&env.app.db).await.unwrap();
        sweep(&env.app).await;
        assert!(running(&env, &cold).await);
        env.app.cfg.update(|cfg| { cfg.agents.model_mode = ModelMode::ClaudeCodex; Ok(()) }).await.unwrap();
        sweep(&env.app).await;
        assert!(!running(&env, &cold).await);
        for run in [&warm, &unnotified, &busy] { assert!(running(&env, run).await); }
        let parent_conv = db::conversation_id(&env.app.db, &parent.id).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE conversation_id=? AND role='system' AND content LIKE '%70%'").bind(parent_conv).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn excess_idle_codex_evicts_oldest_per_parent() {
        let env = testing::env().await;
        env.app.cfg.update(|cfg| { cfg.agents.model_mode = ModelMode::ClaudeCodex; Ok(()) }).await.unwrap();
        let parent = testing::claude_bot(&env.app, &env.project_id, "parent").await;
        let mut runs = Vec::new();
        for age in [100, 200, 300, 400] { runs.push(child(&env, &parent.id, "codex", age, true).await.1); }
        sweep(&env.app).await;
        for run in &runs[..3] { assert!(running(&env, run).await); }
        assert!(!running(&env, &runs[3]).await);
        let parent_conv = db::conversation_id(&env.app.db, &parent.id).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE conversation_id=? AND role='system' AND content LIKE '%codex%'").bind(parent_conv).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(count, 1);
    }
}
