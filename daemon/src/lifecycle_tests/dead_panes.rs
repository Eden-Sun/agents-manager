#![allow(unused_imports)]
mod nr_target_tests {
    use crate::lifecycle::dead_panes::*;
    use crate::lifecycle::*;
    use crate::lifecycle_tests::prelude::*;
    use crate::testing as tt;

    async fn fixture() -> (tt::Env, db::Bot, db::Run) {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "old-name").await;
        let id = tt::fake_run(&env.app, &bot.id).await;
        let run = db::run(&env.app.db, &id).await.unwrap().unwrap();
        (env, bot, run)
    }

    #[tokio::test]
    async fn nr_target_is_the_pane_even_after_names_change_or_are_reused() {
        let (env, mut bot, mut run) = fixture().await;
        let pane = run.pane_id.clone().unwrap();
        env.herdr.set_agent("renamed", &pane, true);
        env.herdr.set_agent("agent", "foreign-pane", true);
        bot.name = "new-display-name".into();
        assert_eq!(db::run_target(&run, &bot), pane);
        run.agent_name = None;
        assert_eq!(db::run_target(&run, &bot), pane);
        run.pane_id = None;
        assert!(db::run_target(&run, &bot).is_empty(), "missing ID must never fall back to a name");
    }

    #[tokio::test]
    async fn nr_target_rename_without_session_evidence_does_not_retire_the_run() {
        let (env, _, run) = fixture().await;
        env.herdr.set_agent("renamed", run.pane_id.as_deref().unwrap(), false);
        assert!(!pane_gone(&env.app, &run).await);
        assert!(sweep(&env.app).await.is_empty());
    }

    #[tokio::test]
    async fn nr_target_old_name_elsewhere_does_not_redirect_keys_or_delivery() {
        let (env, bot, mut run) = fixture().await;
        let pane = run.pane_id.as_deref().unwrap();
        env.herdr.set_agent("agent", "foreign-pane", true);
        env.herdr.set_agent("renamed", pane, true);
        run.native_session_id = Some("sess-1".into());
        let client = client_for_run(&env.app, &run).await.unwrap();
        assert!(!pane_target_stale(&client, &run).await);
        crate::lifecycle::slash::send_keys(&env.app, &bot.id, vec!["esc".into()], None).await.unwrap();
        assert_eq!(env.herdr.calls_to("agent.send_keys")[0]["target"], pane);
        let plan = crate::lifecycle::delivery::plan_delivery(&env.app, &client, &run, &bot, "hello", false, false).await.unwrap();
        match plan {
            Ok(crate::lifecycle::delivery::Plan::AgentPrompt { target }) => assert_eq!(target, pane),
            _ => panic!("bound original pane must remain the delivery target"),
        }
    }

    #[tokio::test]
    async fn nr_target_missing_pane_sends_no_keys() {
        let (env, bot, run) = fixture().await;
        env.herdr.set_agent("old-name", "foreign-pane", true);
        sqlx::query("UPDATE runs SET pane_id=NULL WHERE id=?")
            .bind(&run.id).execute(&env.app.db).await.unwrap();
        assert!(crate::lifecycle::slash::send_keys(&env.app, &bot.id, vec!["esc".into()], None).await.is_err());
        assert!(env.herdr.calls_to("agent.send_keys").is_empty());
    }

    #[tokio::test]
    async fn nr_target_moved_native_session_is_stale_but_not_dead() {
        let (env, _, mut run) = fixture().await;
        env.herdr.set_agent("agent", run.pane_id.as_deref().unwrap(), true);
        env.herdr.set_unnamed_agent("moved-pane", Some("original-session"));
        run.native_session_id = Some("original-session".into());
        let client = client_for_run(&env.app, &run).await.unwrap();
        assert!(pane_target_stale(&client, &run).await);
        assert!(!pane_id_reused(&client, &run).await);
    }

    #[tokio::test]
    async fn nr_target_native_session_moved_out_of_an_empty_pane_blocks_keys() {
        let (env, _, mut run) = fixture().await;
        env.herdr.set_unnamed_agent("moved-pane", Some("original-session"));
        run.native_session_id = Some("original-session".into());
        let client = client_for_run(&env.app, &run).await.unwrap();
        assert!(pane_target_stale(&client, &run).await);
    }

    #[tokio::test]
    async fn nr_target_same_name_cannot_hide_a_different_native_session() {
        let (env, bot, mut run) = fixture().await;
        env.herdr.set_agent("agent", run.pane_id.as_deref().unwrap(), true);
        run.native_session_id = Some("original-session".into());
        sqlx::query("UPDATE runs SET native_session_id=? WHERE id=?")
            .bind(&run.native_session_id).bind(&run.id).execute(&env.app.db).await.unwrap();
        let client = client_for_run(&env.app, &run).await.unwrap();
        assert!(pane_target_stale(&client, &run).await);
        assert!(pane_id_reused(&client, &run).await);
        assert!(crate::lifecycle::slash::send_keys(&env.app, &bot.id, vec!["esc".into()], None).await.is_err());
        assert!(crate::lifecycle::stop::stop_bot(&env.app, &bot.id).await.is_err());
        assert_eq!(db::run(&env.app.db, &run.id).await.unwrap().unwrap().state, "running");
        let aborted = crate::lifecycle::stop::abort_turns(&env.app, &bot.id).await.unwrap();
        assert_eq!(aborted["keys_sent"], false);
        assert!(env.herdr.calls_to("agent.send_keys").is_empty());
        let plan = crate::lifecycle::delivery::plan_delivery(&env.app, &client, &run, &bot, "hello", false, false).await.unwrap();
        assert!(plan.is_err(), "a bound but foreign session must not receive agent.prompt");
    }
}
