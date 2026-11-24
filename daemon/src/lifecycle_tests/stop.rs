#![allow(unused_imports)]

use crate::lifecycle_tests::prelude::*;
use crate::lifecycle::*;
use crate::lifecycle::stop::*;
use crate::lifecycle::stop::test_seams::*;
use crate::lifecycle::s6_ports::*;
use crate::hosts::HostsAccess;
use crate::capabilities::{BotLocks, Db};

#[cfg(test)]
mod bot_dir_safety_tests {
    use super::*;
    use crate::testing as tt;

    #[tokio::test]
    async fn invalid_ids_do_not_access_or_remove_local_bot_dirs() {
        let env = tt::env().await;
        let protected = env.app.data_dir.join("bots").join("keep");
        std::fs::create_dir_all(&protected).unwrap();

        for id in ["../..", "foo/bar", r"..\..", ""] {
            assert!(env.app.bot_dir(id).is_err(), "invalid id was accepted: {id:?}");
            purge_bot_dir(&env.app, id, LOCAL_HOST).await;
            assert!(protected.exists(), "purge touched the protected directory for {id:?}");
        }
    }
}

#[cfg(test)]
mod abort_tests {
    use super::*;

    /// `abort_turns` must work when `esc` cannot be delivered (no herdr here): the turn still leaves `in_flight`.
    #[tokio::test]
    async fn unlocks_even_when_the_keys_cannot_be_sent() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-abort-{}", db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::runners::am_base_tests::open_test_db(&dir.join("db.sqlite3")).await.unwrap();
        let cfg = crate::runners::app_ports_p2::load_config(dir.join("config.toml")).await.unwrap();
        let client = crate::herdr::HerdrClient::new(dir.join("herdr.sock"));
        let app = crate::state::App::new(
            pool,
            client.clone(),
            client,
            cfg,
            dir.clone(),
            dir.join("agents-managerd"),
            7799,
            "t".into(),
            "test".into(),
            false,
        );

        let (pid, bid, rid, cid) = (db::ulid(), db::ulid(), db::ulid(), db::ulid());
        let now = db::now();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?,?,?,'local',?)")
            .bind(&pid).bind("/tmp/p").bind("p").bind(&now).execute(app.db()).await.unwrap();
        sqlx::query("INSERT INTO bots (id, project_id, name, kind, hook_token, created_at) VALUES (?,?,?,?,?,?)")
            .bind(&bid).bind(&pid).bind("b").bind("claude").bind("tok").bind(&now).execute(app.db()).await.unwrap();
        sqlx::query("INSERT INTO runs (id, bot_id, state, pane_id, started_at) VALUES (?,?,'running','w1:p1',?)")
            .bind(&rid).bind(&bid).bind(&now).execute(app.db()).await.unwrap();
        sqlx::query("INSERT INTO conversations (id, bot_id, created_at) VALUES (?,?,?)")
            .bind(&cid).bind(&bid).bind(&now).execute(app.db()).await.unwrap();
        let (t_flight, t_unknown) = (db::ulid(), db::ulid());
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
            .bind(&t_flight).bind(&cid).bind(&rid).bind(&now).execute(app.db()).await.unwrap();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','failed','unknown',?)")
            .bind(&t_unknown).bind(&cid).bind(&rid).bind(&now).execute(app.db()).await.unwrap();
        let t_fallback = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, completed_at, created_at) VALUES (?,?,?,'web','completed_fallback','unknown',?,?)")
            .bind(&t_fallback).bind(&cid).bind(&rid).bind(&now).bind(&now).execute(app.db()).await.unwrap();

        let out = abort_turns(&app, &bid).await.expect("abort must not fail just because the keys did");
        assert_eq!(out["keys_sent"], false, "no herdr behind the socket");
        let aborted = out["aborted"].as_array().unwrap();
        assert_eq!(aborted.len(), 3, "in-flight、unknown-delivery 與備援關掉的那一筆: {out}");
        let fallback: (String, String) = sqlx::query_as("SELECT status, delivery FROM turns WHERE id=?")
            .bind(&t_fallback).fetch_one(app.db()).await.unwrap();
        assert_eq!(
            fallback,
            ("completed_fallback".into(), "failed".into()),
            "只解開擋住下一則 prompt 的 unknown，不把一筆已經收好的回合改寫成 failed",
        );

        let flight: (String, String) = sqlx::query_as("SELECT status, delivery FROM turns WHERE id=?")
            .bind(&t_flight).fetch_one(app.db()).await.unwrap();
        assert_eq!(flight, ("failed".into(), "ok".into()), "in-flight turn is closed, delivery untouched");
        let unknown: (String, String) = sqlx::query_as("SELECT status, delivery FROM turns WHERE id=?")
            .bind(&t_unknown).fetch_one(app.db()).await.unwrap();
        assert_eq!(unknown, ("failed".into(), "failed".into()), "unknown delivery is resolved, not left to block");
        assert!(db::in_flight_turn(app.db(), &rid).await.unwrap().is_none(), "nothing left in flight");

        let again = abort_turns(&app, &bid).await.unwrap();
        assert_eq!(again["aborted"].as_array().unwrap().len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod default_session_tests {
    //! SPEC §6.5.1: a run in the user's `default` session is observed; its pane is never closed
    //! or re-created (review 2026-09-12 #4).
    use super::*;
    use crate::testing as tt;

    async fn imported_bot(env: &tt::Env) -> (String, String, crate::herdr::PaneInfo) {
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let pane = client.tab_create(&ws.workspace_id, "/tmp/p", "mine", json!({})).await.unwrap();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, herdr_session, created_at)
             VALUES (?,?,'mine','claude','[]',0,0,'tok','default',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(db::now())
        .execute(app.db())
        .await
        .unwrap();
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, adopted, started_at)
             VALUES (?,?,'running','idle',?,?,?,'mine','default',1,?)",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .bind(&ws.workspace_id)
        .bind(&pane.tab_id)
        .bind(&pane.pane_id)
        .bind(db::now())
        .execute(app.db())
        .await
        .unwrap();
        *env.herdr.agents.lock().unwrap() = vec![json!({
            "name": "mine", "agent": "claude", "agent_status": "idle",
            "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"})];
        (bot_id, run_id, pane)
    }

    /// Stop sends ctrl+c and ends the run, but the user's pane and tab stay exactly as they were.
    #[tokio::test]
    async fn stop_never_closes_the_users_pane() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, run_id, pane) = imported_bot(&env).await;

        assert!(stop_bot(&app, &bot_id).await.unwrap());

        let run = db::run(app.db(), &run_id).await.unwrap().unwrap();
        assert_eq!(run.state, "stopped");
        let methods = env.herdr.methods();
        assert!(methods.iter().any(|m| m == "agent.send_keys"), "the agent was asked to exit");
        assert!(!methods.iter().any(|m| m == "pane.close" || m == "tab.close"), "{methods:?}");
        assert!(env.herdr.tab(&pane.tab_id).unwrap().panes.contains(&pane.pane_id), "the pane is still there");
    }

    /// Start / restart refused with a UI reason, before any ctrl+c.
    #[tokio::test]
    async fn start_and_restart_are_refused_before_touching_the_agent() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, run_id, _pane) = imported_bot(&env).await;

        let reason = |e: LcError| match e {
            LcError::Conflict(v) => v["reason"].as_str().unwrap_or_default().to_string(),
            other => panic!("expected 409, got {other:?}"),
        };
        assert_eq!(reason(restart_bot(&app, &bot_id).await.unwrap_err()), "default_session");
        assert_eq!(reason(restart_bot_with(&app, &bot_id, StartOpts { resume_native: true, ..Default::default() }).await.unwrap_err()), "default_session");
        assert_eq!(db::active_run(app.db(), &bot_id).await.unwrap().map(|r| r.id), Some(run_id.clone()), "still running");
        assert!(!env.herdr.methods().iter().any(|m| m == "agent.send_keys"), "no ctrl+c was sent");

        sqlx::query("UPDATE runs SET state='stopped', ended_at=? WHERE id=?")
            .bind(db::now())
            .bind(&run_id)
            .execute(app.db())
            .await
            .unwrap();
        assert_eq!(reason(start_bot(&app, &bot_id).await.unwrap_err()), "default_session");
        let creates = env.herdr.methods().iter().filter(|m| *m == "workspace.create").count();
        assert_eq!(creates, 1, "only the fixture's own workspace.create; the daemon made none in the user's session");
    }
}

#[cfg(test)]
mod stop_commit_tests {
    //! #146：`stopping`／`stopped` 寫不進去時，stop（與子 agent 原地重啟）不能照做破壞性的副作用、不能回成功。
    use crate::lifecycle::run_state as rs;
    use super::*;
    use crate::testing as tt;

    fn since(env: &tt::Env, n: usize) -> Vec<String> {
        env.herdr.methods().into_iter().skip(n).collect()
    }

    async fn state(app: &Arc<App>, run: &str) -> String {
        db::run(app.db(), run).await.unwrap().unwrap().state
    }

    #[tokio::test]
    async fn d3b2_quarantined_api_stop_is_pre_cas_and_cli_stop_still_works() {
        let env = tt::env().await;
        let (cli, cli_run) = running_bot(&env).await;
        let now = db::now();
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES('d3b2-api',?,'d3b2-api','claude-api','fake',?)").bind(&env.project_id).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES('d3b2-run','d3b2-api','running',?)").bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO conversations(id,bot_id,created_at) VALUES('d3b2-conv','d3b2-api',?)").bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES('d3b2-turn','d3b2-conv','d3b2-run','web','in_flight','ok',?)").bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO turns(id,conversation_id,origin,status,delivery,created_at) VALUES('d3b2-queued','d3b2-conv','web','queued','pending',?)").bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO api_bot_sessions(id,bot_id,owner_run_id,owner_turn_id,generation,checkpoint_json,created_at,updated_at,expires_at) VALUES('d3b2-session','d3b2-api','d3b2-run','d3b2-turn',1,'bad-json',?,?,'9999-01-01T00:00:00Z')").bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,final_text,created_at) VALUES('d3b2-session','d3b2-api','d3b2-run','d3b2-turn',1,0,'completed','fake',?)").bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO api_bot_attempts(id,session_id,bot_id,run_id,turn_id,generation,ordinal,request_hash,requested_speed,state,reserved_microusd,pricing_version,created_at) VALUES('d3b2-attempt','d3b2-session','d3b2-api','d3b2-run','d3b2-turn',1,0,'fake','fast','reserved',123,'fixture',?)").bind(&now).execute(&env.app.db).await.unwrap();
        let app = tt::restart_app(&env).await;
        app.set_startup_ready(false);
        crate::api_bot::startup_gate::barrier(&app).await;
        app.set_startup_ready(true);
        sqlx::query("ALTER TABLE bots RENAME TO d3b2_hidden_bots").execute(&app.db).await.unwrap();
        assert!(stop_bot(&app, "d3b2-api").await.is_err());
        let failed_snapshot: (String,String,String,String,i64,String) = sqlx::query_as("SELECT r.state,t.status,j.state,a.state,a.reserved_microusd,q.status FROM runs r JOIN turns t ON t.run_id=r.id JOIN api_bot_join_receipts j ON j.turn_id=t.id JOIN api_bot_attempts a ON a.turn_id=t.id JOIN turns q ON q.id='d3b2-queued' WHERE r.id='d3b2-run'").fetch_one(&app.db).await.unwrap();
        assert_eq!(failed_snapshot,("running".into(),"in_flight".into(),"pending".into(),"reserved".into(),123,"queued".into()));
        sqlx::query("ALTER TABLE d3b2_hidden_bots RENAME TO bots").execute(&app.db).await.unwrap();
        let result = stop_bot(&app, "d3b2-api").await;
        assert!(matches!(result, Err(LcError::Unavailable(ref value)) if value["error"] == "api_startup_quarantined"));
        let before: (String,String,String,String,i64) = sqlx::query_as("SELECT r.state,t.status,j.state,a.state,a.reserved_microusd FROM runs r JOIN turns t ON t.run_id=r.id JOIN api_bot_join_receipts j ON j.turn_id=t.id JOIN api_bot_attempts a ON a.turn_id=t.id WHERE r.id='d3b2-run'").fetch_one(&app.db).await.unwrap();
        assert_eq!(before,("running".into(),"in_flight".into(),"pending".into(),"reserved".into(),123));
        let queued: String = sqlx::query_scalar("SELECT status FROM turns WHERE id='d3b2-queued'").fetch_one(&app.db).await.unwrap();
        assert_eq!(queued, "queued");
        assert!(matches!(stop_bot(&app, "d3b2-missing").await, Err(LcError::NotFound(_))));
        assert!(stop_bot(&app, &cli).await.unwrap());
        assert_eq!(state(&app, &cli_run).await, "stopped");
    }

    #[tokio::test]
    async fn same_run_stop_transaction_fences_and_rolls_back_shutdown() {
        use crate::lifecycle::api_bot_port::{ApiBeginStopMode as Mode, ApiBeginStopResult as Result, ApiBeginStopError as Error, ApiRunKey};
        let env = tt::env().await;
        let now = db::now();
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES('same-bot',?,'same-bot','claude-api','fake',?)").bind(&env.project_id).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO runs(id,bot_id,state,agent_status,started_at) VALUES('same-run','same-bot','running','idle',?)").bind(&now).execute(&env.app.db).await.unwrap();
        let key = ApiRunKey { bot_id: "same-bot".into(), run_id: "same-run".into() };
        let bad = ApiRunKey { bot_id: "missing".into(), run_id: key.run_id.clone() };
        let shutdown = tokio_util::sync::CancellationToken::new();
        assert!(matches!(api_begin_stop_transaction(&env.app.db, &bad, Mode::Idle, &shutdown).await, Err(Error::IdentityMismatch)));
        assert_eq!(state(&env.app, &key.run_id).await, "running");
        let cancel = shutdown.clone();
        crate::race_point::arm("api_begin_stop_before_commit", &key.run_id, move || async move { cancel.cancel(); });
        assert!(matches!(api_begin_stop_transaction(&env.app.db, &key, Mode::User { withdraw: false }, &shutdown).await, Err(Error::Busy)));
        assert_eq!(state(&env.app, &key.run_id).await, "running");
        let fresh = tokio_util::sync::CancellationToken::new();
        assert!(matches!(api_begin_stop_transaction(&env.app.db, &key, Mode::Idle, &fresh).await, Ok(Result::Applied { .. })));
        assert_eq!(state(&env.app, &key.run_id).await, "stopping");
    }

    #[tokio::test]
    async fn same_run_stop_withdraw_rollback_and_cli_pair() {
        use crate::lifecycle::api_bot_port::{ApiBeginStopMode as Mode, ApiBeginStopResult as Result, ApiBeginStopError as Error, ApiRunKey};
        let env = tt::env().await;
        let (cli, cli_run) = running_bot(&env).await;
        let now = db::now();
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES('tx-bot',?,'tx-bot','claude-api','fake',?)").bind(&env.project_id).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO runs(id,bot_id,state,agent_status,started_at) VALUES('tx-run','tx-bot','running','idle',?)").bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO conversations(id,bot_id,created_at) VALUES('tx-conv','tx-bot',?)").bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO turns(id,conversation_id,origin,status,delivery,awaits_start,created_at) VALUES('tx-turn','tx-conv','web','queued','pending',1,?)").bind(&now).execute(&env.app.db).await.unwrap();
        let key = ApiRunKey { bot_id: "tx-bot".into(), run_id: "tx-run".into() };
        let shutdown = tokio_util::sync::CancellationToken::new();
        assert!(matches!(api_begin_stop_transaction(&env.app.db, &key, Mode::Idle, &shutdown).await, Ok(Result::Lost)));
        sqlx::query("CREATE TRIGGER fixture_refuse_withdraw BEFORE UPDATE ON turns WHEN OLD.id='tx-turn' BEGIN SELECT RAISE(ABORT,'fixture'); END").execute(&env.app.db).await.unwrap();
        assert!(matches!(api_begin_stop_transaction(&env.app.db, &key, Mode::User { withdraw: true }, &shutdown).await, Err(Error::Storage)));
        let error = stop_bot(&env.app, "tx-bot").await.expect_err("transaction failure must not stop the bot");
        assert!(matches!(&error, LcError::Upstream(message) if message == "api_bot_stop_unconfirmed:storage"));
        use axum::response::IntoResponse;
        let response = error.into_response();
        assert_eq!(response.status(), axum::http::StatusCode::BAD_GATEWAY);
        let body = axum::body::to_bytes(response.into_body(), 4096).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"], "upstream");
        assert_eq!(body["message"], "api_bot_stop_unconfirmed:storage");
        assert_eq!(state(&env.app, &key.run_id).await, "running");
        let queued: String = sqlx::query_scalar("SELECT status FROM turns WHERE id='tx-turn'").fetch_one(&env.app.db).await.unwrap();
        assert_eq!(queued, "queued");
        sqlx::query("DROP TRIGGER fixture_refuse_withdraw").execute(&env.app.db).await.unwrap();
        assert!(matches!(api_begin_stop_transaction(&env.app.db, &key, Mode::User { withdraw: true }, &shutdown).await, Ok(Result::Applied { withdrawn }) if withdrawn.len() == 1));
        assert_eq!(state(&env.app, &key.run_id).await, "stopping");
        let queued: String = sqlx::query_scalar("SELECT status FROM turns WHERE id='tx-turn'").fetch_one(&env.app.db).await.unwrap();
        assert_eq!(queued, "failed");
        assert!(stop_bot(&env.app, &cli).await.unwrap());
        assert_eq!(state(&env.app, &cli_run).await, "stopped");
    }

    #[tokio::test]
    async fn same_run_stop_shutdown_after_commit_is_ambiguous() {
        use crate::lifecycle::api_bot_port::{ApiBeginStopMode as Mode, ApiBeginStopError as Error, ApiRunKey};
        let env = tt::env().await;
        let now = db::now();
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES('after-bot',?,'after-bot','claude-api','fake',?)").bind(&env.project_id).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES('after-run','after-bot','running',?)").bind(&now).execute(&env.app.db).await.unwrap();
        let key = ApiRunKey { bot_id: "after-bot".into(), run_id: "after-run".into() };
        let shutdown = tokio_util::sync::CancellationToken::new();
        let cancel = shutdown.clone();
        crate::race_point::arm("api_begin_stop_after_commit", &key.run_id, move || async move { cancel.cancel(); });
        assert!(matches!(api_begin_stop_transaction(&env.app.db, &key, Mode::User { withdraw: false }, &shutdown).await, Err(Error::Ambiguous)));
        assert_eq!(state(&env.app, &key.run_id).await, "stopping", "a confirmed post-commit fault cannot be reported as rollback");
    }

    #[tokio::test]
    async fn same_run_stop_commit_error_is_ambiguous() {
        use crate::lifecycle::api_bot_port::{ApiBeginStopMode as Mode, ApiBeginStopError as Error, ApiRunKey};
        let env = tt::env().await;
        let now = db::now();
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES('fault-bot',?,'fault-bot','claude-api','fake',?)").bind(&env.project_id).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES('fault-run','fault-bot','running',?)").bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("CREATE TABLE fixture_parent(id INTEGER PRIMARY KEY)").execute(&env.app.db).await.unwrap();
        sqlx::query("CREATE TABLE fixture_child(parent INTEGER REFERENCES fixture_parent(id) DEFERRABLE INITIALLY DEFERRED)").execute(&env.app.db).await.unwrap();
        sqlx::query("CREATE TRIGGER fixture_commit_error AFTER UPDATE OF state ON runs WHEN NEW.id='fault-run' BEGIN INSERT INTO fixture_child(parent) VALUES(7); END").execute(&env.app.db).await.unwrap();
        let key = ApiRunKey { bot_id: "fault-bot".into(), run_id: "fault-run".into() };
        let shutdown = tokio_util::sync::CancellationToken::new();
        assert!(matches!(api_begin_stop_transaction(&env.app.db, &key, Mode::User { withdraw: false }, &shutdown).await, Err(Error::Ambiguous)));
    }

    /// 一顆在跑的 bot：自己的 tab、pane、有名字的 agent（mock 收到 ctrl+c 就讓它離開）。不走 `start_bot`：
    /// 遠端編譯機沒裝 claude，preflight 會擋（#139）。
    async fn running_bot(env: &tt::Env) -> (String, String) {
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let pane = client.tab_create(&ws.workspace_id, "/tmp/p", "alfa", json!({})).await.unwrap();
        let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
        let run = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,'proj-alfa','test',?)",
        )
        .bind(&run)
        .bind(&bot.id)
        .bind(&ws.workspace_id)
        .bind(&pane.tab_id)
        .bind(&pane.pane_id)
        .bind(db::now())
        .execute(app.db())
        .await
        .unwrap();
        env.herdr.agents.lock().unwrap().push(json!({
            "name": "proj-alfa", "agent": "claude", "agent_status": "idle",
            "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"}));
        (bot.id, run)
    }

    /// 驗收 1：`stopping` 記不下來 → 一步都不做：不收 in-flight、不送 ctrl+c、不關 pane、不撤佇列。
    #[tokio::test]
    async fn a_stop_that_cannot_record_stopping_does_nothing_to_the_agent() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, run) = running_bot(&env).await;
        let in_flight = rs::a_turn(&app, &bot, Some(&run), "in_flight").await;
        let queued = rs::a_turn(&app, &bot, None, "queued").await;
        let watcher = crate::lifecycle_tests::support::watch_run_pane(&app, &run).await;
        rs::refuse_run_state(&app, "stopping").await;
        let n = env.herdr.methods().len();

        let err = stop_bot(&app, &bot).await.expect_err("沒記下「正在停」就不是在停");
        assert!(matches!(err, LcError::Upstream(_)), "{err:?}");
        let calls = since(&env, n);
        assert!(!calls.iter().any(|m| m == "agent.send_keys" || m == "pane.close"), "{calls:?}");
        assert_eq!(state(&app, &run).await, "running");
        assert_eq!(rs::turn_status(&app, &in_flight).await, "in_flight");
        assert_eq!(rs::turn_status(&app, &queued).await, "queued");
        assert!(crate::lifecycle_tests::support::watched(&app, &watcher).await);
    }

    /// 驗收 2：pane 真的關了，`stopped` 卻寫不進去——不回一般的成功；終態還沒成立，佇列與 watcher 先不動。
    /// DB 恢復後補記 `stopped`（使用者要它停），收尾照常、只做一次。
    #[tokio::test]
    async fn a_stop_whose_stopped_state_cannot_be_recorded_is_not_reported_as_done() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, run) = running_bot(&env).await;
        let pane = db::run(app.db(), &run).await.unwrap().unwrap().pane_id.unwrap();
        let queued = rs::a_turn(&app, &bot, None, "queued").await;
        let watcher = crate::lifecycle_tests::support::watch_run_pane(&app, &run).await;
        rs::refuse_run_state(&app, "stopped").await;

        match stop_bot(&app, &bot).await {
            Err(LcError::Uncommitted(v)) => assert_eq!((v["error"].as_str(), v["run_id"].as_str()), (Some("stop_state_uncommitted"), Some(run.as_str()))),
            other => panic!("DB 沒記下 stopped，要 503 stop_state_uncommitted，拿到 {other:?}"),
        }
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        assert!(client.pane_get(&pane).await.unwrap().is_none(), "pane 真的關了");
        assert_eq!(state(&app, &run).await, "stopping");
        assert_eq!(rs::turn_status(&app, &queued).await, "queued", "終態還沒寫進去，佇列先不撤");
        assert!(crate::lifecycle_tests::support::watched(&app, &watcher).await);
        assert_eq!(rs::scheduled(&run), vec![rs::Settle::FinishStop], "排了補記的重試");

        rs::accept_run_state(&app, "stopped").await;
        assert!(rs::settle_once(&app, &run, &rs::Settle::FinishStop).await);
        assert_eq!(state(&app, &run).await, "stopped", "使用者要它停");
        assert_eq!(rs::turn_status(&app, &queued).await, "failed");
        assert_eq!(rs::system_notes(&app, &queued).await, 1);
        assert!(!crate::lifecycle_tests::support::watched(&app, &watcher).await);
    }

    /// 補記 `stopped` 的重試（`FinishStop`）讀不到主機：這一輪什麼都不寫——收尾要拆那台主機上的 watcher，以前退回 local，
    /// 遠端那台的 watcher 就留著。讀得到之後照常補記、收尾。
    #[tokio::test]
    async fn a_finish_stop_retry_that_cannot_read_the_host_records_nothing_yet() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, run) = running_bot(&env).await;
        let watcher = crate::lifecycle_tests::support::watch_run_pane(&app, &run).await;
        rs::refuse_run_state(&app, "stopped").await;
        assert!(stop_bot(&app, &bot).await.is_err());
        rs::accept_run_state(&app, "stopped").await;

        sqlx::query("ALTER TABLE projects RENAME TO projects_unreadable").execute(app.db()).await.unwrap();
        assert!(!rs::settle_once(&app, &run, &rs::Settle::FinishStop).await, "讀不到主機：還沒補上");
        sqlx::query("ALTER TABLE projects_unreadable RENAME TO projects").execute(app.db()).await.unwrap();
        assert_eq!(state(&app, &run).await, "stopping", "一步都沒寫");
        assert!(crate::lifecycle_tests::support::watched(&app, &watcher).await);

        assert!(rs::settle_once(&app, &run, &rs::Settle::FinishStop).await);
        assert_eq!(state(&app, &run).await, "stopped");
        assert!(!crate::lifecycle_tests::support::watched(&app, &watcher).await, "拆的是那台主機上的 watcher");
    }

    /// #208：遠端 bot 的 `FinishStop` 補記時讀不到主機。以前退回 local 去拆 watcher——拆掉的是本機同名 session、同 pane id
    /// 那顆 bot 的（它從此收不到狀態事件、回合收不掉），遠端該拆的反而留著。現在這一輪什麼都不寫；讀得到才補記、拆遠端那一個。
    #[tokio::test]
    async fn a_finish_stop_that_cannot_read_the_host_never_unwatches_a_local_pane_of_the_same_name() {
        let env = tt::env().await;
        let app = env.app.clone();
        let remote = db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, '/r/p', 'r', 'remote1', ?)")
            .bind(&remote)
            .bind(db::now())
            .execute(app.db())
            .await
            .unwrap();
        let far = tt::claude_bot(&app, &remote, "far").await;
        let near = tt::claude_bot(&app, &env.project_id, "near").await;
        let far_run = tt::fake_run(&app, &far.id).await;
        let near_run = tt::fake_run(&app, &near.id).await;
        sqlx::query("UPDATE runs SET pane_id='p-same' WHERE id IN (?, ?)").bind(&far_run).bind(&near_run).execute(app.db()).await.unwrap();
        sqlx::query("UPDATE runs SET state='stopping' WHERE id=?").bind(&far_run).execute(app.db()).await.unwrap();
        let key = |host: &str| (host.to_string(), "test".to_string(), "p-same".to_string());
        {
            let mut w = app.pane_watchers.lock().await;
            w.insert(key(LOCAL_HOST), tokio::spawn(std::future::pending::<()>()));
            w.insert(key("remote1"), tokio::spawn(std::future::pending::<()>()));
        }

        sqlx::query("ALTER TABLE projects RENAME TO projects_unreadable").execute(app.db()).await.unwrap();
        assert!(!rs::settle_once(&app, &far_run, &rs::Settle::FinishStop).await, "讀不到主機：這一輪不補");
        sqlx::query("ALTER TABLE projects_unreadable RENAME TO projects").execute(app.db()).await.unwrap();
        assert_eq!(state(&app, &far_run).await, "stopping");
        assert!(crate::lifecycle_tests::support::watched(&app, &key(LOCAL_HOST)).await && crate::lifecycle_tests::support::watched(&app, &key("remote1")).await, "兩個 watcher 都沒動");

        assert!(rs::settle_once(&app, &far_run, &rs::Settle::FinishStop).await);
        assert_eq!(state(&app, &far_run).await, "stopped");
        assert!(!crate::lifecycle_tests::support::watched(&app, &key("remote1")).await, "拆的是遠端那一個");
        assert!(crate::lifecycle_tests::support::watched(&app, &key(LOCAL_HOST)).await, "本機同名的那顆照舊收得到狀態事件");
    }

    /// The host can be repointed after the restart's fence check but before stop resolves the run's
    /// session. That lookup must use the captured authority (or fail closed), never H's new client.
    #[tokio::test]
    async fn a_fenced_restart_does_not_send_stop_to_a_repointed_host() {
        let env = tt::env().await;
        let app = env.app.clone();
        let host = "restart-stop-repoint";
        let cfg = |ssh: &str| crate::config::HostCfg { daemon_url: None,
            shared_session: false,
            name: host.into(),
            ssh: ssh.into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "test".into(),
            remote_path: String::new(),
        };
        app.hosts.insert_remote_for_test(cfg("target-a")).await;
        let fence = app.hosts.fence(host).await.expect("A is configured");

        let a_socket = crate::hosts::short_dir(None).join(format!("{host}.sock"));
        std::fs::create_dir_all(a_socket.parent().unwrap()).unwrap();
        let _a_herdr = tt::MockHerdr::start(a_socket);
        let b_instance = format!("restart-stop-{}", db::ulid());
        app.set_instance(Some(b_instance.clone()));
        let b_socket = crate::hosts::short_dir(Some(&b_instance)).join(format!("{host}.sock"));
        std::fs::create_dir_all(b_socket.parent().unwrap()).unwrap();
        let b_herdr = tt::MockHerdr::start(b_socket);

        sqlx::query("UPDATE projects SET host=? WHERE id=?").bind(host).bind(&env.project_id).execute(app.db()).await.unwrap();
        let bot = tt::claude_bot(&app, &env.project_id, "fenced-stop").await;
        let run = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle','ws-a','pane-a','tab-a','proj-fenced-stop','test',?)",
        )
        .bind(&run)
        .bind(&bot.id)
        .bind(db::now())
        .execute(app.db())
        .await
        .unwrap();

        let app_to_repoint = app.clone();
        let replacement = cfg("target-b");
        crate::race_point::arm("restart_before_stop_client_lookup", &bot.id, move || async move {
            app_to_repoint.hosts.replace_remote_for_test(&app_to_repoint, replacement).await;
        });

        let result = crate::lifecycle::restart_bot_with_host_fence(
            &app,
            &bot.id,
            StartOpts { resume_native: true, require_idle: true, ..Default::default() },
            &fence,
        )
        .await;

        assert!(matches!(result, Err(LcError::Conflict(ref value)) if value["reason"] == "host_superseded"), "stale A authority must be reported as superseded: {result:?}");
        assert!(!b_herdr.methods().iter().any(|method| method == "agent.send_keys"), "B's bot must never receive A's stop: {:?}", b_herdr.methods());
        assert_eq!(db::run(app.db(), &run).await.unwrap().unwrap().state, "running", "a stale authority must not stop B's active run");
    }

    /// The no-run cleanup path can still have a preview pane. If H is repointed before that
    /// cleanup, it must not resolve H to B and close a pane there.
    #[tokio::test]
    async fn a_fenced_restart_does_not_clean_a_preview_on_a_repointed_host() {
        let env = tt::env().await;
        let app = env.app.clone();
        let host = "restart-preview-repoint";
        let cfg = |ssh: &str| crate::config::HostCfg { daemon_url: None,
            shared_session: false,
            name: host.into(),
            ssh: ssh.into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "test".into(),
            remote_path: String::new(),
        };
        app.hosts.insert_remote_for_test(cfg("target-a")).await;
        let fence = app.hosts.fence(host).await.expect("A is configured");

        let a_socket = crate::hosts::short_dir(None).join(format!("{host}.sock"));
        std::fs::create_dir_all(a_socket.parent().unwrap()).unwrap();
        let _a_herdr = tt::MockHerdr::start(a_socket.clone());
        let b_instance = format!("restart-preview-{}", db::ulid());
        app.set_instance(Some(b_instance.clone()));
        let b_socket = crate::hosts::short_dir(Some(&b_instance)).join(format!("{host}.sock"));
        std::fs::create_dir_all(b_socket.parent().unwrap()).unwrap();
        struct TmpSockets(Vec<std::path::PathBuf>);
        impl Drop for TmpSockets {
            fn drop(&mut self) {
                for p in &self.0 {
                    let _ = if p.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) };
                }
            }
        }
        let _tmp_sockets = TmpSockets(vec![a_socket.clone(), b_socket.parent().unwrap().to_path_buf()]);
        let b_herdr = tt::MockHerdr::start(b_socket);

        sqlx::query("UPDATE projects SET host=? WHERE id=?").bind(host).bind(&env.project_id).execute(app.db()).await.unwrap();
        let bot = tt::claude_bot(&app, &env.project_id, "fenced-preview").await;
        sqlx::query(
            "INSERT INTO bot_previews (bot_id, host, pane_id, port, dir, status, updated_at, source)
             VALUES (?, ?, 'preview-pane', 5173, '/project', 'running', ?, 'spawned')",
        )
        .bind(&bot.id)
        .bind(host)
        .bind(db::now())
        .execute(app.db())
        .await
        .unwrap();

        let app_to_repoint = app.clone();
        let replacement = cfg("target-b");
        crate::race_point::arm("restart_before_no_run_preview_cleanup", &bot.id, move || async move {
            app_to_repoint.hosts.replace_remote_for_test(&app_to_repoint, replacement).await;
        });

        let lock = app.bot_lock(&bot.id).await;
        let _bot_guard = lock.lock().await;
        let result = stop_for_restart_locked_with_host_fence(&app, &bot.id, &fence).await;

        assert!(!b_herdr.methods().iter().any(|method| method == "pane.close"), "B's preview pane must not be touched: {:?}", b_herdr.methods());
        assert!(matches!(result, Err(LcError::Conflict(ref value)) if value["reason"] == "host_superseded"), "stale authority must report superseded: {result:?}");
        let preview = crate::preview::row(app.db(), &bot.id).await.unwrap().unwrap();
        assert_eq!(preview.status, "running", "the stale request must leave B's preview row alone");
    }

    /// #208（同檔同類）：強制中止讀不到在飛的那一筆——以前 esc 已經送出去了，讀錯又當成「沒有」，打斷的紀錄沒記，
    /// 那一回合的 StopFailure 回聲之後會被當成真的失敗。現在先讀再按：讀不到就一步都不做。
    #[tokio::test]
    async fn a_force_abort_that_cannot_read_the_in_flight_turn_sends_no_esc() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, run) = running_bot(&env).await;
        rs::a_turn(&app, &bot, Some(&run), "in_flight").await;
        let n = env.herdr.methods().len();
        sqlx::query("ALTER TABLE turns RENAME TO turns_unreadable").execute(app.db()).await.unwrap();
        assert!(abort_turns(&app, &bot).await.is_err());
        sqlx::query("ALTER TABLE turns_unreadable RENAME TO turns").execute(app.db()).await.unwrap();
        let calls = since(&env, n);
        assert!(!calls.iter().any(|m| m == "agent.send_keys"), "讀不到就不按 esc：{calls:?}");
    }

    /// #208（同檔同類）：強制中止收掉 `delivery = unknown` 的那一筆，說明寫不進去——以前 `let _` 吞掉，回合解開了、對話裡卻沒有
    /// 一句話，API 照回 `aborted`。現在收掉與說明同一個交易：寫不進去就兩個都沒發生、回錯；寫得進去再按一次，一起成立。
    #[tokio::test]
    async fn a_force_abort_whose_note_cannot_be_written_changes_nothing() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "unknown").await;
        let turn = rs::a_turn(&app, &bot.id, None, "failed").await;
        sqlx::query("UPDATE turns SET delivery='unknown' WHERE id=?").bind(&turn).execute(app.db()).await.unwrap();
        sqlx::query(&format!(
            "CREATE TRIGGER refuse_abort_note BEFORE INSERT ON messages WHEN NEW.turn_id = '{turn}' BEGIN SELECT RAISE(ABORT, 'injected'); END"
        ))
        .execute(app.db())
        .await
        .unwrap();
        let delivery = || {
            let app = app.clone();
            let turn = turn.clone();
            async move { sqlx::query_scalar::<_, String>("SELECT delivery FROM turns WHERE id=?").bind(&turn).fetch_one(app.db()).await.unwrap() }
        };

        assert!(abort_turns(&app, &bot.id).await.is_err(), "說明寫不進去：不回 aborted");
        assert_eq!(delivery().await, "unknown", "解開也沒發生");
        assert_eq!(rs::system_notes(&app, &turn).await, 0);

        sqlx::query("DROP TRIGGER refuse_abort_note").execute(app.db()).await.unwrap();
        let out = abort_turns(&app, &bot.id).await.unwrap();
        assert_eq!(out["aborted"], json!([turn.clone()]));
        assert_eq!((delivery().await, rs::system_notes(&app, &turn).await), ("failed".to_string(), 1));
    }

    /// 驗收 2 的另一半：重試還沒補上 daemon 就重啟了（排的重試跟著沒了）——開機的對帳照證據把它收掉，
    /// 不會留一顆擋住 start 的 `stopping`。
    #[tokio::test]
    async fn a_stop_left_in_stopping_is_settled_by_the_reconcile_after_a_restart() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, run) = running_bot(&env).await;
        rs::refuse_run_state(&app, "stopped").await;
        assert!(stop_bot(&app, &bot).await.is_err());
        rs::accept_run_state(&app, "stopped").await;

        crate::reconcile::reconcile_host(&app, LOCAL_HOST).await.unwrap();
        assert!(db::active_run(app.db(), &bot).await.unwrap().is_none(), "不再擋住下一次 start：{}", state(&app, &run).await);
    }

    /// 驗收 3：default session 的 pane 不能強制關；agent 對 ctrl+c 沒反應、還活著，就不能記成 `stopped`。
    #[tokio::test]
    async fn a_default_session_agent_that_ignores_ctrl_c_is_not_recorded_as_stopped() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let pane = client.tab_create(&ws.workspace_id, "/tmp/p", "mine", json!({})).await.unwrap();
        let bot = tt::claude_bot(&app, &env.project_id, "mine").await.id;
        sqlx::query("UPDATE bots SET herdr_session='default' WHERE id=?").bind(&bot).execute(app.db()).await.unwrap();
        let run = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, adopted, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'default',1,?)",
        )
        .bind(&run)
        .bind(&bot)
        .bind(&ws.workspace_id)
        .bind(&pane.tab_id)
        .bind(&pane.pane_id)
        .bind(&pane.pane_id)
        .bind(db::now())
        .execute(app.db())
        .await
        .unwrap();
        *env.herdr.agents.lock().unwrap() = vec![json!({
            "name": "mine", "agent": "claude", "agent_status": "idle", "ignore_ctrl_c": true,
            "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"})];
        let queued = rs::a_turn(&app, &bot, None, "queued").await;

        let err = stop_bot(&app, &bot).await.expect_err("agent 還活著，不是停好了");
        assert!(matches!(&err, LcError::Upstream(m) if m.starts_with("stop_not_confirmed")), "{err:?}");
        assert_eq!(state(&app, &run).await, "running", "agent 還在，run 放回 running");
        assert_eq!(rs::turn_status(&app, &queued).await, "queued");
        assert!(!env.herdr.methods().iter().any(|m| m == "pane.close"), "使用者的 pane 照舊不關");
    }

    /// 驗收 4：讀完 active run 之後，不拿 bot 鎖的 pane-exit 事件先把它收成 `exited`——stop 輸了 CAS，
    /// 不再對它送 ctrl+c／關 pane，也不把它拉回 `stopping`。
    #[tokio::test]
    async fn a_stop_that_finds_its_run_already_ended_does_not_touch_the_pane() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, run) = running_bot(&env).await;
        let (app2, run2) = (app.clone(), run.clone());
        crate::race_point::arm("stop_before_stopping", &bot, move || async move {
            mark_run_exited(&app2, &run2, "pane exited").await;
        });
        let n = env.herdr.methods().len();

        assert!(!stop_bot(&app, &bot).await.unwrap(), "沒有東西要停");
        let calls = since(&env, n);
        assert!(!calls.iter().any(|m| m == "agent.send_keys" || m == "pane.close"), "{calls:?}");
        assert_eq!(state(&app, &run).await, "exited", "終態不被拉回 stopping");
    }

    /// stop 自己關的 pane 觸發 herdr 的 pane_closed 事件，事件那邊的 `mark_run_exited` 搶在 stop 記
    /// `stopped` 之前寫了 `exited`：仍然記成使用者要的 `stopped`（autostart 的 bot 才不會被報 `bot_stopped`），
    /// 收尾不做第二份。
    #[tokio::test]
    async fn a_pane_exit_that_lands_during_a_stop_still_leaves_it_recorded_as_a_user_stop() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, run) = running_bot(&env).await;
        let queued = rs::a_turn(&app, &bot, None, "queued").await;
        let (app2, run2) = (app.clone(), run.clone());
        crate::race_point::arm("stop_before_stopped", &bot, move || async move {
            mark_run_exited(&app2, &run2, "pane exited").await;
        });

        assert!(stop_bot(&app, &bot).await.unwrap());
        assert_eq!(state(&app, &run).await, "stopped");
        assert_eq!(rs::turn_status(&app, &queued).await, "failed");
        assert_eq!(rs::system_notes(&app, &queued).await, 1, "撤一次");
    }

    /// #146 重開 A：stop 自己關的 pane 觸發的 pane-exit 事件先寫了 `exited`，改標回 `stopped` 卻寫不進去。以前被當成 CAS
    /// 輸了：回一般的成功、不排重試，DB 永遠留 `exited`——autostart 的 bot 從此被 incident 探針報成「自己掛了」。現在回
    /// 503 `stop_state_uncommitted`、排 `FinishStop`；DB 恢復後改標成 `stopped`、探針不再報，stop 才有的撤回也在那時補上。
    #[tokio::test]
    async fn a_stop_whose_relabel_after_a_pane_exit_cannot_be_written_is_not_reported_as_done() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, run) = running_bot(&env).await;
        sqlx::query("UPDATE bots SET autostart=1 WHERE id=?").bind(&bot).execute(app.db()).await.unwrap();
        let waiting = rs::a_turn(&app, &bot, None, "queued").await;
        sqlx::query("UPDATE turns SET awaits_start=1 WHERE id=?").bind(&waiting).execute(app.db()).await.unwrap();
        let (app2, run2) = (app.clone(), run.clone());
        crate::race_point::arm("stop_before_stopped", &bot, move || async move {
            mark_run_exited(&app2, &run2, "pane exited").await;
        });
        rs::refuse_run_state(&app, "stopped").await;

        match stop_bot(&app, &bot).await {
            Err(LcError::Uncommitted(v)) => assert_eq!((v["error"].as_str(), v["run_id"].as_str()), (Some("stop_state_uncommitted"), Some(run.as_str()))),
            other => panic!("改標寫不進去不是停好了，要 503 stop_state_uncommitted，拿到 {other:?}"),
        }
        assert_eq!(state(&app, &run).await, "exited");
        assert_eq!(rs::scheduled(&run), vec![rs::Settle::FinishStop], "排了補改標的重試");
        assert!(crate::lifecycle_tests::support::bot_stopped_reported(&app, &bot).await, "前提：留著 exited，探針說它自己掛了");
        assert_eq!(rs::turn_status(&app, &waiting).await, "failed", "等它起來的那一則在記 `stopping` 時就一起撤了（#199）");

        rs::accept_run_state(&app, "stopped").await;
        assert!(rs::settle_once(&app, &run, &rs::Settle::FinishStop).await);
        assert_eq!(state(&app, &run).await, "stopped", "使用者要它停");
        assert!(!crate::lifecycle_tests::support::bot_stopped_reported(&app, &bot).await, "故意停的 autostart bot 不是 outage");
        assert_eq!(rs::system_notes(&app, &waiting).await, 1, "只撤一次");
    }

    /// 重啟那一半不改標：重啟不是「使用者要它停」。pane-exit 先寫了 `exited` 就留著（開不回來時要的正是它），重啟照常往下走。
    #[tokio::test]
    async fn a_restart_whose_old_run_a_pane_exit_ended_keeps_it_exited() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, run) = running_bot(&env).await;
        let (app2, run2) = (app.clone(), run.clone());
        crate::race_point::arm("stop_before_stopped", &bot, move || async move {
            mark_run_exited(&app2, &run2, "pane exited").await;
        });
        assert!(stop_for_restart_locked(&app, &bot).await.unwrap(), "pane 關了、舊 run 收掉了：重啟照常往下走");
        assert_eq!(state(&app, &run).await, "exited", "不改標成 stopped：沒開回來時探針才看得到");
        assert!(rs::scheduled(&run).is_empty(), "沒有要補的");
    }

    /// #199：使用者按停止時，等它起來才送的那一則撤不掉（DB 寫不進去）——撤回跟記 `stopping` 同一個交易，所以連停都不算開始：
    /// 回 502、run 照舊 running、不送 ctrl+c、不關 pane、那一則還在。寫得進去之後再按一次，兩個一起成立。
    #[tokio::test]
    async fn a_stop_whose_withdrawal_cannot_be_written_does_nothing_at_all() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, run) = running_bot(&env).await;
        let waiting = rs::a_turn(&app, &bot, None, "queued").await;
        sqlx::query("UPDATE turns SET awaits_start=1 WHERE id=?").bind(&waiting).execute(app.db()).await.unwrap();
        rs::refuse_turn_close(&app, &waiting).await;
        let n = env.herdr.methods().len();

        let err = stop_bot(&app, &bot).await.expect_err("撤回寫不進去：不算開始停");
        assert!(matches!(err, LcError::Upstream(_)), "{err:?}");
        let calls = since(&env, n);
        assert!(!calls.iter().any(|m| m == "agent.send_keys" || m == "pane.close"), "{calls:?}");
        assert_eq!((state(&app, &run).await, rs::turn_status(&app, &waiting).await), ("running".to_string(), "queued".to_string()), "兩個都沒成立");

        rs::accept_turn_close(&app).await;
        assert!(stop_bot(&app, &bot).await.unwrap());
        assert_eq!((state(&app, &run).await, rs::turn_status(&app, &waiting).await), ("stopped".to_string(), "failed".to_string()));
        assert_eq!(rs::system_notes(&app, &waiting).await, 1);
    }

    /// #199 驗收 2：撤回跟著 `stopping` 落地之後，`stopped` 寫不進去、重試補上之前 daemon 就重啟——開機對帳把那顆收成 `exited`，
    /// 等它起來的那一則已經撤了，開機不替它啟動、永遠不送。（撤回若放在記 `stopped` 那一步，這裡就會被開機再啟動、送出去。）
    #[tokio::test]
    async fn a_withdrawal_made_with_stopping_survives_a_restart_before_stopped_is_recorded() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, run) = running_bot(&env).await;
        let waiting = rs::a_turn(&app, &bot, None, "queued").await;
        sqlx::query("UPDATE turns SET awaits_start=1 WHERE id=?").bind(&waiting).execute(app.db()).await.unwrap();
        rs::refuse_run_state(&app, "stopped").await;
        assert!(matches!(stop_bot(&app, &bot).await, Err(LcError::Uncommitted(_))));
        assert_eq!((state(&app, &run).await, rs::turn_status(&app, &waiting).await), ("stopping".to_string(), "failed".to_string()));
        rs::accept_run_state(&app, "stopped").await;

        let fresh = tt::restart_app(&env).await;
        crate::reconcile::reconcile_host(&fresh, LOCAL_HOST).await.unwrap();
        assert!(db::active_run(fresh.db(), &bot).await.unwrap().is_none(), "開機對帳收掉了卡在 stopping 的 run");
        assert_eq!(crate::lifecycle::start_send::resume_after_boot(&fresh, LOCAL_HOST).await, 0, "沒有在等它起來的：不替它啟動");
        assert_eq!(rs::turn_status(&fresh, &waiting).await, "failed");
    }

    /// 重啟那一半的 stop 不撤等它起來的訊息：那段沒有 run 是暫時的（`restart_hold`），重啟不是「不要了」。
    #[tokio::test]
    async fn a_restart_does_not_withdraw_what_waits_for_the_start() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, _run) = running_bot(&env).await;
        let waiting = rs::a_turn(&app, &bot, None, "queued").await;
        sqlx::query("UPDATE turns SET awaits_start=1 WHERE id=?").bind(&waiting).execute(app.db()).await.unwrap();
        let restarting = crate::lifecycle::restart_hold::begin(&bot);
        assert!(stop_for_restart_locked(&app, &bot).await.unwrap());
        drop(restarting);
        assert_eq!(rs::turn_status(&app, &waiting).await, "queued", "重啟不是不要了");
    }

    /// 同一個競態，排著的是「bot 沒在跑時送、等它起來」的那一種（#122 的 `awaits_start`）：撤孤兒那一支刻意不撤它，
    /// 只有使用者的 stop 會撤——改標成 `stopped` 的這條路也要補上，不留到下次啟動又送出去。
    #[tokio::test]
    async fn a_user_stop_that_raced_a_pane_exit_still_withdraws_what_waited_for_the_start() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, run) = running_bot(&env).await;
        let waiting = rs::a_turn(&app, &bot, None, "queued").await;
        sqlx::query("UPDATE turns SET awaits_start=1 WHERE id=?").bind(&waiting).execute(app.db()).await.unwrap();
        let (app2, run2) = (app.clone(), run.clone());
        crate::race_point::arm("stop_before_stopped", &bot, move || async move {
            mark_run_exited(&app2, &run2, "pane exited").await;
        });

        assert!(stop_bot(&app, &bot).await.unwrap());
        assert_eq!(state(&app, &run).await, "stopped");
        assert_eq!(rs::turn_status(&app, &waiting).await, "failed", "使用者停了，等它起來的那一則撤回");
        assert_eq!(rs::system_notes(&app, &waiting).await, 1);
    }

    /// 子 agent：父開的 pane 裡一顆名字叫 `proj-alfa-ui` 的 agent，run 在跑。
    async fn a_child(env: &tt::Env) -> (String, String, crate::herdr::PaneInfo) {
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
        let parent = tt::claude_bot(&app, &env.project_id, "alfa").await;
        let kid = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, parent_bot_id, created_at)
             VALUES (?,?,'ui','claude','[]',0,0,'tok','child',?,?)",
        )
        .bind(&kid)
        .bind(&env.project_id)
        .bind(&parent.id)
        .bind(db::now())
        .execute(app.db())
        .await
        .unwrap();
        let run = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, adopted, started_at)
             VALUES (?,?,'running','idle',?,?,?,'proj-alfa-ui','test',1,?)",
        )
        .bind(&run)
        .bind(&kid)
        .bind(&ws.workspace_id)
        .bind(&pane.tab_id)
        .bind(&pane.pane_id)
        .bind(db::now())
        .execute(app.db())
        .await
        .unwrap();
        *env.herdr.agents.lock().unwrap() = vec![json!({
            "name": "proj-alfa-ui", "agent": "claude", "agent_status": "idle",
            "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"})];
        (kid, run, pane)
    }

    /// #156：in-flight 那一筆收不成（寫不進去）就不停——這時 agent 還沒被打斷、那一回合真的還在跑。不送 ctrl+c、
    /// 不關 pane、不撤佇列，run 放回 running，回 503（不是普通的成功，也不假裝回合收掉了）。DB 好了再按一次照常停，
    /// 收尾各只做一次。
    #[tokio::test]
    async fn a_stop_whose_in_flight_turn_cannot_be_closed_does_not_touch_the_agent() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, run) = running_bot(&env).await;
        let in_flight = rs::a_turn(&app, &bot, Some(&run), "in_flight").await;
        let queued = rs::a_turn(&app, &bot, None, "queued").await;
        let watcher = crate::lifecycle_tests::support::watch_run_pane(&app, &run).await;
        rs::refuse_turn_close(&app, &in_flight).await;
        let n = env.herdr.methods().len();

        let err = stop_bot(&app, &bot).await.expect_err("回合收不成就沒有停");
        let LcError::Unavailable(body) = &err else { panic!("要 503、可重試：{err:?}") };
        assert_eq!(body["error"], "turn_state_unwritable", "{body}");
        assert_eq!(body["turn_id"], in_flight.as_str(), "{body}");
        let calls = since(&env, n);
        assert!(!calls.iter().any(|m| m == "agent.send_keys" || m == "pane.close"), "{calls:?}");
        assert_eq!(state(&app, &run).await, "running", "沒停就還是 running");
        assert_eq!(rs::turn_status(&app, &in_flight).await, "in_flight");
        assert_eq!(rs::system_notes(&app, &in_flight).await, 0);
        assert_eq!(rs::turn_status(&app, &queued).await, "queued", "沒停就不撤");
        assert!(crate::lifecycle_tests::support::watched(&app, &watcher).await);

        rs::accept_turn_close(&app).await;
        assert!(stop_bot(&app, &bot).await.expect("DB 好了照常停"));
        assert_eq!(state(&app, &run).await, "stopped");
        assert_eq!((rs::turn_status(&app, &in_flight).await, rs::system_notes(&app, &in_flight).await), ("failed".to_string(), 1));
        assert_eq!(rs::turn_status(&app, &queued).await, "failed");
    }

    /// #156：子 agent 原地重啟一樣——舊 run 的 in-flight 收不成就不送 ctrl+c、不重開，run 放回 running。
    #[tokio::test]
    async fn a_child_restart_whose_in_flight_turn_cannot_be_closed_does_not_touch_the_child() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (kid, run, _pane) = a_child(&env).await;
        let in_flight = rs::a_turn(&app, &kid, Some(&run), "in_flight").await;
        rs::refuse_turn_close(&app, &in_flight).await;
        let n = env.herdr.methods().len();

        let err = restart_child_in_pane(&app, &kid).await.expect_err("回合收不成就不重啟");
        let LcError::Unavailable(body) = &err else { panic!("要 503、可重試：{err:?}") };
        assert_eq!(body["error"], "turn_state_unwritable", "{body}");
        let calls = since(&env, n);
        assert!(!calls.iter().any(|m| m == "agent.send_keys" || m == "agent.start"), "{calls:?}");
        assert_eq!(state(&app, &run).await, "running");
        assert_eq!(rs::turn_status(&app, &in_flight).await, "in_flight");
    }

    /// #146 留言：子 agent 原地重啟的舊 run 走同一套——`stopping` 記不下來就不收 in-flight、不送 ctrl+c。
    #[tokio::test]
    async fn a_child_restart_that_cannot_record_stopping_does_not_touch_the_child() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (kid, run, _pane) = a_child(&env).await;
        let in_flight = rs::a_turn(&app, &kid, Some(&run), "in_flight").await;
        rs::refuse_run_state(&app, "stopping").await;
        let n = env.herdr.methods().len();

        assert!(restart_child_in_pane(&app, &kid).await.is_err());
        let calls = since(&env, n);
        assert!(!calls.iter().any(|m| m == "agent.send_keys" || m == "agent.start"), "{calls:?}");
        assert_eq!(state(&app, &run).await, "running");
        assert_eq!(rs::turn_status(&app, &in_flight).await, "in_flight");
    }

    /// #146 留言：舊 run 的 `stopped` 沒寫進去之前不能寫新 run、不能 `agent.start`——不是靠 active-run 唯一索引
    /// 碰巧擋住。失敗照 #129：沒開回來的重啟放掉 `restart_hold`，舊 run 收成終態之後排著的派工才照舊撤。
    #[tokio::test]
    async fn a_child_restart_starts_no_replacement_before_the_old_run_is_recorded_stopped() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (kid, run, _pane) = a_child(&env).await;
        let queued = rs::a_turn(&app, &kid, None, "queued").await;
        rs::refuse_run_state(&app, "stopped").await;

        let err = restart_child_in_pane(&app, &kid).await.expect_err("舊 run 沒記下 stopped");
        assert!(matches!(&err, LcError::Uncommitted(v) if v["error"] == "stop_state_uncommitted"), "明確擋下，不是撞唯一索引：{err:?}");
        assert!(!env.herdr.methods().iter().any(|m| m == "agent.start"));
        let runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE bot_id=?").bind(&kid).fetch_one(app.db()).await.unwrap();
        assert_eq!(runs, 1, "沒有寫新 run");
        assert_eq!(state(&app, &run).await, "stopping");
        assert!(!crate::lifecycle::restart_hold::in_progress(&kid), "重啟結束了");
        assert_eq!(rs::turn_status(&app, &queued).await, "queued", "舊 run 還沒成終態，不撤");
        assert_eq!(rs::scheduled(&run), vec![rs::Settle::Reconcile { stuck: "stopping".into() }]);

        rs::accept_run_state(&app, "stopped").await;
        assert!(rs::settle_once(&app, &run, &rs::Settle::Reconcile { stuck: "stopping".into() }).await);
        assert!(db::active_run(app.db(), &kid).await.unwrap().is_none());
        assert_eq!(rs::turn_status(&app, &queued).await, "failed", "沒開回來：照 #129 撤掉");
    }

    /// 子 agent 在原 pane 裡起來了，新 run 的 `running` 卻寫不進去：同 #145，不回成功、排重試。
    #[tokio::test]
    async fn a_child_restart_whose_new_run_cannot_be_recorded_running_is_not_reported_as_done() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (kid, _run, _pane) = a_child(&env).await;
        rs::refuse_run_state(&app, "running").await;

        let err = restart_child_in_pane(&app, &kid).await.expect_err("新 run 沒記下 running");
        assert!(matches!(&err, LcError::Uncommitted(v) if v["error"] == "start_state_uncommitted"), "{err:?}");
        let fresh = db::active_run(app.db(), &kid).await.unwrap().expect("新 run 還在");
        assert_eq!(fresh.state, "starting");
        assert_eq!(rs::scheduled(&fresh.id), vec![rs::Settle::Reconcile { stuck: "starting".into() }]);
    }

    /// 同上，AGM 有一則排著的派工：重啟中不撤（#129），但對帳把新 run 收成 `running` 不會叫 flush，它起來時的 idle 邊又早在
    /// `starting` 就過了——要自己等它收斂再叫，不然那一則要等 30 分鐘的排隊保險絲把它撤掉。
    #[tokio::test]
    async fn a_child_restart_whose_new_run_cannot_be_recorded_running_still_flushes_the_queue_once_it_settles() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (kid, _run, _pane) = a_child(&env).await;
        let queued = rs::a_turn(&app, &kid, None, "queued").await;
        rs::refuse_run_state(&app, "running").await;

        restart_child_in_pane(&app, &kid).await.expect_err("新 run 沒記下 running");
        let fresh = db::active_run(app.db(), &kid).await.unwrap().expect("新 run 還在");
        assert_eq!(rs::turn_status(&app, &queued).await, "queued", "重啟中：不當孤兒撤");
        assert!(crate::lifecycle::start_send::watching_for_running(&fresh.id), "排了「收成 running 就叫 flush」");
    }

    /// 子 agent 不理 ctrl+c（10 秒後放棄），`stopping → running` 放回去也寫不進去：排重試，DB 恢復後放回。
    #[tokio::test]
    async fn a_child_restart_that_gives_up_retries_putting_the_run_back() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (kid, run, _pane) = a_child(&env).await;
        env.herdr.agents.lock().unwrap()[0]["name"] = serde_json::Value::Null;
        env.herdr.agents.lock().unwrap()[0]["ignore_ctrl_c"] = json!(true);
        rs::refuse_run_state(&app, "running").await;

        assert!(restart_child_in_pane(&app, &kid).await.is_err());
        assert_eq!(state(&app, &run).await, "stopping");
        assert_eq!(rs::scheduled(&run), vec![rs::Settle::BackToRunning]);

        rs::accept_run_state(&app, "running").await;
        assert!(rs::settle_once(&app, &run, &rs::Settle::BackToRunning).await);
        assert_eq!(state(&app, &run).await, "running");
    }
}

mod force_abort_tests;
