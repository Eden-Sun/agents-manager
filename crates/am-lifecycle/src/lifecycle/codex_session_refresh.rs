//! Refresh a late native session binding for a fenced Codex run.

use super::super::*;

/// Best-effort repair only for a freshly observed, fenced Codex instance.
pub(super) async fn refresh(app: &impl LcHost, run: &db::Run, bot: &db::Bot, turn: &str) -> Option<db::Run> {
    if bot.kind != "codex" || run.native_session_id.is_some() {
        return None;
    }
    let result = refresh_inner(app, run, turn).await;
    let reason = match &result { Ok(Some(_)) => "bound", Ok(None) => "not_proven", Err(_) => "read_or_write_failed" };
    let basis = if matches!(&result, Ok(Some(_))) { "same_host_session_pane_terminal" } else { "not_proven" };
    tracing::info!(run = %run.id, turn, adopted = run.adopted, reason, basis, "codex native session refresh");
    result.ok().flatten()
}

async fn refresh_inner(app: &impl LcHost, run: &db::Run, turn: &str) -> anyhow::Result<Option<db::Run>> {
    let Some(pane) = run.pane_id.as_deref().filter(|s| !s.trim().is_empty()) else { return Ok(None) };
    let Some(session) = run.herdr_session.as_deref().filter(|s| !s.trim().is_empty()) else { return Ok(None) };
    let Some(terminal) = run.terminal_id.as_deref().filter(|s| !s.trim().is_empty()) else { return Ok(None) };
    let (host, generation): (Option<String>, Option<i64>) = sqlx::query_as("SELECT host,host_generation FROM runs WHERE id=?")
        .bind(&run.id).fetch_one(app.db()).await?;
    let client = match super::super::client_for_run(app, run).await { Ok(c) => c, Err(_) => return Ok(None) };
    if host.as_deref().is_some_and(|h| h != client.fence.conn().name)
        || generation != client.fence.target_epoch() { return Ok(None) }
    let started = std::time::Instant::now();
    let observed = match tokio::time::timeout(Duration::from_secs(5), client.agent_get(pane)).await {
        Ok(Ok(Some(agent))) => agent,
        _ => return Ok(None),
    };
    if observed.pane_id != pane || observed.agent.as_deref() != Some("codex")
        || observed.terminal_id.as_deref() != Some(terminal) { return Ok(None) }
    let Some(native) = super::super::dead_panes::herdr_session_id(&observed) else { return Ok(None) };
    #[cfg(all(test, feature = "daemon-test-harness"))]
    super::super::race_point::hit("codex_session_refresh_before_write", &run.id).await;
    let result = client.fence.run_current(Box::pin(async {
        if started.elapsed() >= Duration::from_secs(5) { return Ok::<_, anyhow::Error>(None) }
        let changed = sqlx::query(
            "UPDATE runs SET native_session_id=? WHERE id=? AND bot_id=? AND adopted=?
             AND state IN ('starting','running','stopping') AND ended_at IS NULL AND native_session_id IS NULL
             AND pane_id=? AND herdr_session=? AND terminal_id=? AND host IS ? AND host_generation IS ?
             AND EXISTS(SELECT 1 FROM bots b JOIN projects p ON p.id=b.project_id
                        WHERE b.id=runs.bot_id AND b.kind='codex' AND b.deleted_at IS NULL AND p.host=?)
             AND EXISTS(SELECT 1 FROM turns t WHERE t.id=? AND t.run_id=runs.id AND t.status='in_flight' AND t.delivery='ok')
             AND NOT EXISTS(SELECT 1 FROM runs other WHERE other.bot_id=runs.bot_id AND other.id<>runs.id
                            AND other.state IN ('starting','running','stopping') AND other.ended_at IS NULL)"
        ).bind(&native).bind(&run.id).bind(&run.bot_id).bind(run.adopted).bind(pane).bind(session).bind(terminal)
            .bind(&host).bind(generation).bind(&client.fence.conn().name).bind(turn)
            .execute(app.db()).await?.rows_affected();
        if changed != 1 { return Ok(None) }
        db::run(app.db(), &run.id).await
    })).await;
    Ok(result.transpose()?.flatten())
}

#[cfg(all(test, feature = "daemon-test-harness"))]
mod tests {
    use super::super::*;
    use crate::testing as tt;

    const SESSION: &str = "01a18888-0000-7000-8000-000000000201";
    const TERMINAL: &str = "terminal-2012";

    async fn fixture(remote: bool) -> (tt::Env, String, String, Option<tt::MockHerdr>) {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "late-codex").await;
        sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        let run = tt::fake_run(&app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        sqlx::query("UPDATE runs SET adopted=1, terminal_id=?, native_session_id=NULL WHERE id=?")
            .bind(TERMINAL).bind(&run).execute(&app.db).await.unwrap();
        let remote_herdr = if remote {
            let host = format!("synthetic-remote-{}", db::ulid());
            let socket = env.dir.join("remote-refresh.sock");
            let mock = tt::MockHerdr::start(socket.clone());
            let cfg = crate::config::HostCfg { daemon_url: None, shared_session: false, name: host.clone(), ssh: "fake".into(), ssh_port: 22, ssh_opts: vec![], herdr_session: "test".into(), remote_path: String::new() };
            app.hosts.insert_remote_with_client_for_test(cfg, crate::herdr::HerdrClient::new(socket)).await;
            let fence = app.hosts.fence(&host).await.unwrap();
            sqlx::query("UPDATE runs SET host=?,host_generation=? WHERE id=?").bind(&host).bind(fence.target_epoch()).bind(&run).execute(&app.db).await.unwrap();
            sqlx::query("UPDATE projects SET host=? WHERE id=?").bind(&host).bind(&env.project_id).execute(&app.db).await.unwrap();
            Some(mock)
        } else { None };
        let herdr = remote_herdr.as_ref().unwrap_or(&env.herdr);
        herdr.agents.lock().unwrap().push(json!({
            "name": "agent", "agent": "codex", "pane_id": pane,
            "terminal_id": TERMINAL, "agent_status": "idle", "workspace_id": "ws-1", "tab_id": "tab-1",
            "agent_session": {"agent": "codex", "kind": "id", "value": SESSION}
        }));
        herdr.set_screen(&pane, "› synthetic prompt\n• synthetic screen answer\n› ");
        let turn = crate::lifecycle::run_state::a_turn(&app, &bot.id, Some(&run), "in_flight").await;
        sqlx::query("UPDATE turns SET prompt_text='synthetic prompt', created_at='2026-10-25T00:59:50Z', delivered_at='2026-10-25T00:59:50Z' WHERE id=?")
            .bind(&turn).execute(&app.db).await.unwrap();
        (env, run, turn, remote_herdr)
    }

    #[tokio::test]
    async fn late_local_binding_recovers_rollout_before_terminal_fallback() {
        let (env, run, turn, _remote) = fixture(false).await;
        let home = env.app.data_dir.join("synthetic-codex-home");
        let dir = home.join("sessions/2026/10/25");
        std::fs::create_dir_all(&dir).unwrap();
        let log = format!("{}\n{}\n", json!({"type":"response_item","timestamp":"2026-10-25T01:00:00Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"synthetic prompt"}]}}), json!({"type":"event_msg","timestamp":"2026-10-25T01:00:05Z","payload":{"type":"task_complete","last_agent_message":"synthetic native answer"}}));
        std::fs::write(dir.join(format!("rollout-2026-10-25T01-00-00-{SESSION}.jsonl")), log).unwrap();
        sqlx::query("UPDATE bots SET env_json=? WHERE id=(SELECT bot_id FROM runs WHERE id=?)")
            .bind(json!({"CODEX_HOME": home.to_string_lossy()}).to_string()).bind(&run).execute(&env.app.db).await.unwrap();
        assert!(super::super::try_fallback(&env.app, &run, Some(&turn)).await.unwrap());
        let native: Option<String> = sqlx::query_scalar("SELECT native_session_id FROM runs WHERE id=?").bind(&run).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(native.as_deref(), Some(SESSION), "late native binding must be refreshed before fallback");
        let msg: (String, String, i64) = sqlx::query_as("SELECT content,source,incomplete FROM messages WHERE turn_id=? AND role='assistant'").bind(&turn).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(msg, ("synthetic native answer".into(), "transcript".into(), 0));
    }

    #[tokio::test]
    async fn late_remote_binding_is_refreshed_without_remote_rollout() {
        let (env, run, turn, _remote) = fixture(true).await;
        assert!(super::super::try_fallback(&env.app, &run, Some(&turn)).await.unwrap());
        let native: Option<String> = sqlx::query_scalar("SELECT native_session_id FROM runs WHERE id=?").bind(&run).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(native.as_deref(), Some(SESSION), "remote adopted run needs the same late binding refresh");
        let source: (String, i64) = sqlx::query_as("SELECT source,incomplete FROM messages WHERE turn_id=? AND role='assistant'").bind(&turn).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(source, ("terminal_fallback".into(), 1));
    }

    #[tokio::test]
    async fn managed_local_binding_recovers_rollout_before_terminal_fallback() {
        let (env, run, turn, _remote) = fixture(false).await;
        sqlx::query("UPDATE runs SET adopted=0 WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
        let home = env.app.data_dir.join("synthetic-codex-home");
        let dir = home.join("sessions/2026/10/25");
        std::fs::create_dir_all(&dir).unwrap();
        let log = format!("{}\n{}\n", json!({"type":"response_item","timestamp":"2026-10-25T01:00:00Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"synthetic prompt"}]}}), json!({"type":"event_msg","timestamp":"2026-10-25T01:00:05Z","payload":{"type":"task_complete","last_agent_message":"synthetic native answer"}}));
        std::fs::write(dir.join(format!("rollout-2026-10-25T01-00-00-{SESSION}.jsonl")), log).unwrap();
        sqlx::query("UPDATE bots SET env_json=? WHERE id=(SELECT bot_id FROM runs WHERE id=?)")
            .bind(json!({"CODEX_HOME": home.to_string_lossy()}).to_string()).bind(&run).execute(&env.app.db).await.unwrap();
        assert!(super::super::try_fallback(&env.app, &run, Some(&turn)).await.unwrap());
        let native: Option<String> = sqlx::query_scalar("SELECT native_session_id FROM runs WHERE id=?").bind(&run).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(native.as_deref(), Some(SESSION), "late native binding must be refreshed before fallback");
        let msg: (String, String, i64) = sqlx::query_as("SELECT content,source,incomplete FROM messages WHERE turn_id=? AND role='assistant'").bind(&turn).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(msg, ("synthetic native answer".into(), "transcript".into(), 0));
    }

    #[tokio::test]
    async fn managed_remote_binding_is_refreshed_without_remote_rollout() {
        let (env, run, turn, _remote) = fixture(true).await;
        sqlx::query("UPDATE runs SET adopted=0 WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
        assert!(super::super::try_fallback(&env.app, &run, Some(&turn)).await.unwrap());
        let native: Option<String> = sqlx::query_scalar("SELECT native_session_id FROM runs WHERE id=?").bind(&run).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(native.as_deref(), Some(SESSION), "remote managed run needs the same late binding refresh");
        let source: (String, i64) = sqlx::query_as("SELECT source,incomplete FROM messages WHERE turn_id=? AND role='assistant'").bind(&turn).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(source, ("terminal_fallback".into(), 1));
    }

    #[tokio::test]
    async fn rejects_unproven_instance_and_existing_binding() {
        for case in ["terminal_missing", "terminal_mismatch", "pane_mismatch", "kind_mismatch", "session_missing", "native_empty", "existing_native", "wrong_generation", "wrong_host", "exited", "ended_at", "read_error"] {
            let (env, run_id, turn, _remote) = fixture(false).await;
            match case {
                "terminal_missing" => { sqlx::query("UPDATE runs SET terminal_id=NULL WHERE id=?").bind(&run_id).execute(&env.app.db).await.unwrap(); }
                "terminal_mismatch" => env.herdr.agents.lock().unwrap()[0]["terminal_id"] = json!("other"),
                "pane_mismatch" => env.herdr.agents.lock().unwrap()[0]["pane_id"] = json!("other-pane"),
                "kind_mismatch" => env.herdr.agents.lock().unwrap()[0]["agent"] = json!("claude"),
                "session_missing" => { sqlx::query("UPDATE runs SET herdr_session=NULL WHERE id=?").bind(&run_id).execute(&env.app.db).await.unwrap(); }
                "native_empty" => env.herdr.agents.lock().unwrap()[0]["agent_session"]["value"] = json!(" "),
                "existing_native" => { sqlx::query("UPDATE runs SET native_session_id='existing' WHERE id=?").bind(&run_id).execute(&env.app.db).await.unwrap(); }
                "wrong_generation" => { sqlx::query("UPDATE runs SET host_generation=99 WHERE id=?").bind(&run_id).execute(&env.app.db).await.unwrap(); }
                "wrong_host" => { sqlx::query("UPDATE runs SET host='other' WHERE id=?").bind(&run_id).execute(&env.app.db).await.unwrap(); }
                "exited" => { sqlx::query("UPDATE runs SET state='exited' WHERE id=?").bind(&run_id).execute(&env.app.db).await.unwrap(); }
                "ended_at" => { sqlx::query("UPDATE runs SET ended_at='2026-10-27T00:00:00Z' WHERE id=?").bind(&run_id).execute(&env.app.db).await.unwrap(); }
                "read_error" => env.herdr.fail_next("agent.get", tt::Fault::Refuse),
                _ => unreachable!(),
            }
            let run = db::run(&env.app.db, &run_id).await.unwrap().unwrap();
            let bot = db::bot(&env.app.db, &run.bot_id).await.unwrap().unwrap();
            assert!(super::refresh(&env.app, &run, &bot, &turn).await.is_none(), "{case}");
            let native: Option<String> = sqlx::query_scalar("SELECT native_session_id FROM runs WHERE id=?").bind(&run_id).fetch_one(&env.app.db).await.unwrap();
            assert_eq!(native.as_deref(), if case == "existing_native" { Some("existing") } else { None }, "{case}");
        }
    }

    #[tokio::test]
    async fn cas_rejects_changes_after_observation() {
        for change in ["ended", "pane", "session", "terminal", "native", "adopted", "run_host", "host_generation", "turn", "project_host", "new_run"] {
            let (env, run_id, turn, _remote) = fixture(false).await;
            let run = db::run(&env.app.db, &run_id).await.unwrap().unwrap();
            let bot = db::bot(&env.app.db, &run.bot_id).await.unwrap().unwrap();
            let app = env.app.clone();
            let run_for_race = run_id.clone();
            let turn_for_race = turn.clone();
            let project = env.project_id.clone();
            let change = change.to_owned();
            let race_change = change.clone();
            super::super::super::race_point::arm("codex_session_refresh_before_write", &run_id, move || async move {
                if race_change == "new_run" {
                    sqlx::query("UPDATE runs SET state='exited',ended_at='2026-10-27T00:00:00Z' WHERE id=?").bind(&run_for_race).execute(&app.db).await.unwrap();
                }
                let sql = match race_change.as_str() {
                    "ended" => "UPDATE runs SET state='exited',ended_at='2026-10-27T00:00:00Z' WHERE id=?",
                    "pane" => "UPDATE runs SET pane_id='other' WHERE id=?",
                    "session" => "UPDATE runs SET herdr_session='other' WHERE id=?",
                    "terminal" => "UPDATE runs SET terminal_id='other' WHERE id=?",
                    "native" => "UPDATE runs SET native_session_id='existing' WHERE id=?",
                    "adopted" => "UPDATE runs SET adopted=0 WHERE id=?",
                    "run_host" => "UPDATE runs SET host='other' WHERE id=?",
                    "host_generation" => "UPDATE runs SET host_generation=99 WHERE id=?",
                    "turn" => "UPDATE turns SET status='completed' WHERE id=?",
                    "project_host" => "UPDATE projects SET host='other' WHERE id=?",
                    "new_run" => "INSERT INTO runs (id,bot_id,state,agent_status,started_at) SELECT 'new-run',bot_id,'running','idle',started_at FROM runs WHERE id=?",
                    _ => unreachable!(),
                };
                let id = match race_change.as_str() { "turn" => &turn_for_race, "project_host" => &project, _ => &run_for_race };
                sqlx::query(sql).bind(id).execute(&app.db).await.unwrap();
            });
            assert!(super::refresh(&env.app, &run, &bot, &turn).await.is_none(), "{change}");
            let native: Option<String> = sqlx::query_scalar("SELECT native_session_id FROM runs WHERE id=?").bind(&run_id).fetch_one(&env.app.db).await.unwrap();
            assert_eq!(native.as_deref(), if change == "native" { Some("existing") } else { None }, "{change}");
        }
    }


    #[tokio::test]
    async fn stale_fence_or_expired_observation_never_binds() {
        for case in ["fence", "expired"] {
            let (env, run_id, turn, _remote) = fixture(false).await;
            let run = db::run(&env.app.db, &run_id).await.unwrap().unwrap();
            let bot = db::bot(&env.app.db, &run.bot_id).await.unwrap().unwrap();
            let fence = super::super::super::client_for_run(&env.app, &run).await.unwrap().fence;
            super::super::super::race_point::arm("codex_session_refresh_before_write", &run_id, move || async move {
                if case == "fence" { fence.conn().bump_generation_for_test(); }
                else { tokio::time::sleep(Duration::from_millis(5100)).await; }
            });
            assert!(super::refresh(&env.app, &run, &bot, &turn).await.is_none(), "{case}");
            assert!(db::run(&env.app.db, &run_id).await.unwrap().unwrap().native_session_id.is_none(), "{case}");
        }
    }

}
