#![allow(unused_imports)]
//! #1862 L2-1：從 am-lifecycle 搬來、需要真 daemon App／DB 的測試。

mod tests {
    use crate::lifecycle_tests::prelude::*;
    use crate::default_session::{same_workdir, sync, SESSION};
    use crate::db;
    use crate::testing as tt;
    use serde_json::json;
    use std::sync::atomic::Ordering;
    #[tokio::test]
    async fn nr1791_default_import_survives_rename_without_following_reused_name() {
        let env = tt::env().await;
        let app = &env.app;
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let other = client.tab_create(&ws.workspace_id, "/tmp/p", "other", json!({})).await.unwrap();
        let entry = |name: &str, pane: &crate::herdr::PaneInfo, sid: &str| json!({
            "name":name,"agent":"claude","agent_status":"idle","cwd":env.repo.to_string_lossy(),
            "workspace_id":ws.workspace_id,"tab_id":pane.tab_id,"pane_id":pane.pane_id,
            "agent_session":{"kind":"id","value":sid}});
        app.cfg.update(|cfg| {
            cfg.projects.push(crate::config::ProjectCfg { mode: "parent_child".into(), role_policy: Default::default(), deputy: None, specialist_dispatch: true,
                id: Some(env.project_id.clone()), path: env.repo.to_string_lossy().into_owned(),
                label: "proj".into(), host: "local".into(), handed_off_to: None, bots: vec![],
            });
            Ok(())
        }).await.unwrap();
        *env.herdr.agents.lock().unwrap() = vec![entry("old", &root, "native-1")];
        sync(app).await.unwrap();
        let imported: String = sqlx::query_scalar("SELECT bot_id FROM runs WHERE herdr_session='default'").fetch_one(&app.db).await.unwrap();
        *env.herdr.agents.lock().unwrap() = vec![entry("renamed", &root, "native-1"), entry("old", &other, "native-2")];
        sync(app).await.unwrap();
        let kept = db::active_run(&app.db, &imported).await.unwrap().unwrap();
        assert_eq!(kept.pane_id.as_deref(), Some(root.pane_id.as_str()));
        assert_eq!(kept.agent_name.as_deref(), Some("renamed"));
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bots WHERE herdr_session='default'").fetch_one(&app.db).await.unwrap();
        assert_eq!(count, 2, "only the new native identity creates a second bot");
        *env.herdr.agents.lock().unwrap() = vec![entry("replacement", &root, "different-instance"), entry("old", &other, "native-2")];
        sync(app).await.unwrap();
        let unknown = db::active_run(&app.db, &imported).await.unwrap().unwrap();
        assert_eq!(unknown.id, kept.id);
        assert_eq!(unknown.native_session_id.as_deref(), Some("native-1"));
        assert_eq!(unknown.agent_name.as_deref(), Some("renamed"));
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bots WHERE herdr_session='default'").fetch_one(&app.db).await.unwrap();
        assert_eq!(count, 2, "a conflicting occupant stays unknown instead of creating or overwriting a bot");

    }
    #[tokio::test]
    async fn nr1791_default_run_without_a_pane_id_stays_unknown() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "imported").await;
        sqlx::query("UPDATE bots SET herdr_session='default' WHERE id=?").bind(&bot.id).execute(&env.app.db).await.unwrap();
        let run = tt::fake_run(&env.app, &bot.id).await;
        sqlx::query("UPDATE runs SET pane_id=NULL, herdr_session='default', agent_name='old' WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
        sync(&env.app).await.unwrap();
        assert_eq!(db::active_run(&env.app.db, &bot.id).await.unwrap().unwrap().id, run);
    }
    /// `agent.list` 暫時回空、但 `agent.get` 還找得到：不能把 default-session 的 run 標成 exited。
    #[tokio::test]
    async fn an_empty_agent_list_does_not_exit_a_default_session_run_that_is_still_there() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let pane = client.tab_create(&ws.workspace_id, env.repo.to_str().unwrap(), "imported", json!({})).await.unwrap();
        let bot = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, herdr_session, created_at)
             VALUES (?,?,?,'claude','[]',0,1,'tok',?,?)",
        )
        .bind(&bot)
        .bind(&env.project_id)
        .bind("imported")
        .bind(SESSION)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let run = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'default',?)",
        )
        .bind(&run)
        .bind(&bot)
        .bind(&ws.workspace_id)
        .bind(&pane.tab_id)
        .bind(&pane.pane_id)
        .bind("imported")
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        *env.herdr.agents.lock().unwrap() = vec![json!({
            "name": "imported", "agent": "claude", "agent_status": "idle",
            "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id,
            "cwd": env.repo.to_string_lossy(),
        })];
        env.herdr.hide_agent_list.store(true, Ordering::SeqCst);

        sync(&app).await.unwrap();

        let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(&run).fetch_one(&app.db).await.unwrap();
        assert_eq!(state, "running", "list 空、get 還在：不能當成 agent 消失");
    }
    /// list 空、get 也說沒有：這次是真的消失，run 才該收。
    #[tokio::test]
    async fn a_default_session_run_still_exits_when_herdr_confirms_the_agent_is_gone() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, herdr_session, created_at)
             VALUES (?,?,?,'claude','[]',0,1,'tok',?,?)",
        )
        .bind(&bot)
        .bind(&env.project_id)
        .bind("gone")
        .bind(SESSION)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let run = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,'default',?)",
        )
        .bind(&run)
        .bind(&bot)
        .bind("w1:pGone")
        .bind("gone")
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();

        sync(&app).await.unwrap();

        let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(&run).fetch_one(&app.db).await.unwrap();
        assert_eq!(state, "exited");
    }
}
