//! Old (shared-tab, `tab_id` NULL) and new bots side by side (SPEC §6.5).
use crate::db;
use crate::state::App;
use crate::testing as tt;
use serde_json::{json, Value};
use std::sync::Arc;

async fn a_bot(env: &tt::Env, name: &str) -> String {
    let id = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,?,'claude','[]',0,1,'tok',?)",
    )
    .bind(&id)
    .bind(&env.project_id)
    .bind(name)
    .bind(db::now())
    .execute(&env.app.db)
    .await
    .unwrap();
    id
}

async fn run_of(app: &Arc<App>, bot_id: &str) -> Option<db::Run> {
    sqlx::query_as::<_, db::Run>("SELECT * FROM runs WHERE bot_id = ? ORDER BY started_at DESC LIMIT 1")
        .bind(bot_id)
        .fetch_optional(&app.db)
        .await
        .unwrap()
}

#[tokio::test]
async fn d3b2_host_reconcile_skips_api_but_reconciles_cli_and_fails_closed_on_read_error() {
    let env = tt::env().await;
    let api = a_bot(&env, "d3b2-api").await;
    let cli = a_bot(&env, "d3b2-cli").await;
    let missing = a_bot(&env, "d3b2-missing-host").await;
    sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES('d3b2-missing-host-run',?,'running',?)").bind(&missing).bind(db::now()).execute(&env.app.db).await.unwrap();
    let mut conn = env.app.db.acquire().await.unwrap();
    sqlx::query("PRAGMA foreign_keys=OFF").execute(&mut *conn).await.unwrap();
    sqlx::query("DELETE FROM bots WHERE id=?").bind(&missing).execute(&mut *conn).await.unwrap();
    sqlx::query("PRAGMA foreign_keys=ON").execute(&mut *conn).await.unwrap();
    drop(conn);
    assert!(!db::live_bots_on_host(&env.app.db, crate::config::LOCAL_HOST).await.unwrap().iter().any(|b| b.id == missing));
    sqlx::query("UPDATE bots SET kind='claude-api' WHERE id=?").bind(&api).execute(&env.app.db).await.unwrap();
    for (id, bot) in [("d3b2-api-run", &api), ("d3b2-cli-run", &cli)] {
        sqlx::query("INSERT INTO runs(id,bot_id,state,agent_status,herdr_session,started_at) VALUES(?,?,'running','idle','test',?)").bind(id).bind(bot).bind(db::now()).execute(&env.app.db).await.unwrap();
    }
    sqlx::query("UPDATE runs SET pane_id='d3b2-missing' WHERE id='d3b2-cli-run'").execute(&env.app.db).await.unwrap();
    env.herdr.agents.lock().unwrap().clear();
    super::reconcile_host(&env.app, crate::config::LOCAL_HOST).await.unwrap();
    assert_eq!(run_of(&env.app, &api).await.unwrap().state, "running");
    assert_eq!(run_of(&env.app, &cli).await.unwrap().state, "exited");
    let missing_state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id='d3b2-missing-host-run'").fetch_one(&env.app.db).await.unwrap();
    assert_eq!(missing_state, "running");
    sqlx::query("ALTER TABLE bots RENAME TO d3b2_hidden_bots").execute(&env.app.db).await.unwrap();
    assert!(super::reconcile_host(&env.app, crate::config::LOCAL_HOST).await.is_err());
    sqlx::query("ALTER TABLE d3b2_hidden_bots RENAME TO bots").execute(&env.app.db).await.unwrap();
    assert_eq!(run_of(&env.app, &api).await.unwrap().state, "running");
}

#[tokio::test]
async fn d3b2_rearm_keeps_api_and_read_error_debt_but_adopts_cli() {
    let env = tt::env().await;
    let api = a_bot(&env, "d3b2-api-progress").await;
    let cli = a_bot(&env, "d3b2-cli-progress").await;
    sqlx::query("UPDATE bots SET kind='claude-api' WHERE id=?").bind(&api).execute(&env.app.db).await.unwrap();
    for (run, turn, bot) in [("d3b2-api-run", "d3b2-api-turn", &api), ("d3b2-cli-run", "d3b2-cli-turn", &cli)] {
        let conv = db::conversation_id(&env.app.db, bot).await.unwrap();
        sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES(?,?,'running',?)").bind(run).bind(bot).bind(db::now()).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES(?,?,?,'web','in_flight','pending',?)").bind(turn).bind(conv).bind(run).bind(db::now()).execute(&env.app.db).await.unwrap();
    }
    let mut recovery = super::Recovery::new();
    recovery.boot = "9999-01-01".into();
    sqlx::query("ALTER TABLE bots RENAME TO d3b2_hidden_bots").execute(&env.app.db).await.unwrap();
    recovery.rearm_in_flight(&env.app).await;
    assert_eq!(recovery.runs.as_ref().unwrap().len(), 2);
    sqlx::query("ALTER TABLE d3b2_hidden_bots RENAME TO bots").execute(&env.app.db).await.unwrap();
    recovery.rearm_in_flight(&env.app).await;
    let rows: Vec<(String,String)> = sqlx::query_as("SELECT id,delivery FROM turns WHERE id IN ('d3b2-api-turn','d3b2-cli-turn') ORDER BY id").fetch_all(&env.app.db).await.unwrap();
    assert_eq!(rows, vec![("d3b2-api-turn".into(),"pending".into()), ("d3b2-cli-turn".into(),"unknown".into())]);
}

#[tokio::test]
async fn d3b2_rearm_missing_bot_keeps_debt_and_processes_cli() {
    let env = tt::env().await;
    let missing = a_bot(&env, "d3b2-missing").await;
    let cli = a_bot(&env, "d3b2-cli").await;
    for (run, turn, bot) in [("d3b2-missing-run", "d3b2-missing-turn", &missing), ("d3b2-valid-run", "d3b2-valid-turn", &cli)] {
        let conv = db::conversation_id(&env.app.db, bot).await.unwrap();
        sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES(?,?,'running',?)").bind(run).bind(bot).bind(db::now()).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES(?,?,?,'web','in_flight','pending',?)").bind(turn).bind(conv).bind(run).bind(db::now()).execute(&env.app.db).await.unwrap();
    }
    let mut conn = env.app.db.acquire().await.unwrap();
    sqlx::query("PRAGMA foreign_keys=OFF").execute(&mut *conn).await.unwrap();
    sqlx::query("DELETE FROM bots WHERE id=?").bind(&missing).execute(&mut *conn).await.unwrap();
    sqlx::query("PRAGMA foreign_keys=ON").execute(&mut *conn).await.unwrap();
    drop(conn);
    assert!(db::bot(&env.app.db, &missing).await.unwrap().is_none());
    let mut recovery = super::Recovery::new();
    recovery.boot = "9999-01-01".into();
    recovery.rearm_in_flight(&env.app).await;
    assert!(recovery.runs.as_ref().unwrap().contains("d3b2-missing-run"));
    let rows: Vec<(String,String)> = sqlx::query_as("SELECT id,delivery FROM turns WHERE id IN ('d3b2-missing-turn','d3b2-valid-turn') ORDER BY id").fetch_all(&env.app.db).await.unwrap();
    assert_eq!(rows, vec![("d3b2-missing-turn".into(),"pending".into()),("d3b2-valid-turn".into(),"unknown".into())]);
}

#[tokio::test]
async fn nr1791_unmapped_start_exits_only_when_herdr_confirms_no_agents() {
    let env = tt::env().await;
    let app = &env.app;
    let bot = a_bot(&env, "alfa").await;
    let run = db::ulid();
    sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,herdr_session,started_at) VALUES (?,?,'starting','idle','test',?)")
        .bind(&run).bind(&bot).bind(db::now()).execute(&app.db).await.unwrap();
    *env.herdr.agents.lock().unwrap() = vec![json!({"name":"unidentified","agent":"claude","agent_status":"idle","workspace_id":"unknown-workspace","tab_id":"unknown-tab","pane_id":"unknown-pane"})];
    super::reconcile_host(app, crate::config::LOCAL_HOST).await.unwrap();
    assert_eq!(run_of(app, &bot).await.unwrap().state, "starting", "missing identity with a possible agent stays unknown");
    env.herdr.agents.lock().unwrap().clear();
    env.herdr.fail_next("agent.list", tt::Fault::RefuseWith("unavailable"));
    assert!(super::reconcile_host(app, crate::config::LOCAL_HOST).await.is_err());
    assert_eq!(run_of(app, &bot).await.unwrap().state, "starting", "RPC failure is not evidence of absence");
    super::reconcile_host(app, crate::config::LOCAL_HOST).await.unwrap();
    assert_eq!(run_of(app, &bot).await.unwrap().state, "exited", "confirmed empty herdr cannot keep a starting zombie");
}

// #1791: names are snapshots; only the pane/native session establishes identity.
#[tokio::test]
async fn nr1791_renamed_run_ignores_reused_names_and_project_labels() {
    let env = tt::env().await;
    let app = &env.app;
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let stranger = client.tab_create(&ws.workspace_id, "/tmp/p", "other", json!({})).await.unwrap();
    let bot = a_bot(&env, "alfa").await;
    let old_name = crate::config::agent_name("proj", &bot);
    let run = db::ulid();
    sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,pane_id,herdr_session,agent_name,started_at) VALUES (?,?,'running','idle',?,'test',?,?)")
        .bind(&run).bind(&bot).bind(&root.pane_id).bind(&old_name).bind(db::now()).execute(&app.db).await.unwrap();
    sqlx::query("UPDATE projects SET label='changed' WHERE id=?").bind(&env.project_id).execute(&app.db).await.unwrap();
    *env.herdr.agents.lock().unwrap() = vec![
        json!({"name":"renamed","agent":"claude","agent_status":"working","workspace_id":ws.workspace_id,"tab_id":root.tab_id,"pane_id":root.pane_id}),
        json!({"name":old_name,"agent":"claude","agent_status":"idle","workspace_id":ws.workspace_id,"tab_id":stranger.tab_id,"pane_id":stranger.pane_id})];
    super::reconcile_host(app, crate::config::LOCAL_HOST).await.unwrap();
    let kept = db::active_run(&app.db, &bot).await.unwrap().unwrap();
    assert_eq!(kept.id, run);
    assert_eq!(kept.pane_id.as_deref(), Some(root.pane_id.as_str()));
    assert_eq!(kept.agent_name.as_deref(), Some("renamed"));
    assert!(all_children(app).await.is_empty(), "a reused name does not establish parenthood");
}

#[tokio::test]
async fn nr1791_parent_claim_does_not_prove_a_child_instance() {
    let env = tt::env().await;
    let app = &env.app;
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let parent = a_bot(&env, "alfa").await;
    let (kid, run) = a_child(&env, &parent, "kid", "old", &ws.workspace_id, &root.tab_id, &root.pane_id).await;
    crate::spawn_hints::claim(app, &parent, &root.pane_id).await.unwrap();
    *env.herdr.agents.lock().unwrap() = vec![json!({"name":"replacement","agent":"claude","agent_status":"working",
        "workspace_id":ws.workspace_id,"tab_id":root.tab_id,"pane_id":root.pane_id})];
    super::reconcile_host(app, crate::config::LOCAL_HOST).await.unwrap();
    let kept = db::active_run(&app.db, &kid).await.unwrap().expect("unknown is not an exit");
    assert_eq!(kept.id, run);
    assert_eq!(kept.agent_name.as_deref(), Some("old"), "parent claim cannot attach a different child");
    assert!(db::bot(&app.db, &kid).await.unwrap().unwrap().deleted_at.is_none());
    sqlx::query("UPDATE runs SET native_session_id='original-instance' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
    env.herdr.agents.lock().unwrap()[0]["agent_session"] = json!({"kind":"id","value":"different-instance"});
    super::reconcile_host(app, crate::config::LOCAL_HOST).await.unwrap();
    let kept = db::active_run(&app.db, &kid).await.unwrap().unwrap();
    assert_eq!(kept.id, run);
    assert_eq!(kept.agent_name.as_deref(), Some("old"));
    assert_eq!(kept.native_session_id.as_deref(), Some("original-instance"));
    assert_eq!(all_children(app).await.len(), 1);
}

#[tokio::test]
async fn nr1791_adoption_reuses_native_identity_instead_of_child_nickname() {
    let env = tt::env().await;
    let app = &env.app;
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let other = client.tab_create(&ws.workspace_id, "/tmp/p", "other", json!({})).await.unwrap();
    let parent = a_bot(&env, "alfa").await;
    let (kid, run) = a_child(&env, &parent, "display-renamed", "old", &ws.workspace_id, &root.tab_id, &root.pane_id).await;
    sqlx::query("UPDATE runs SET state='exited', native_session_id='native-child' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
    let parent = db::bot(&app.db, &parent).await.unwrap().unwrap();
    let agent: crate::herdr::AgentInfo = serde_json::from_value(json!({"name":"renamed","agent":"claude","agent_status":"idle",
        "workspace_id":ws.workspace_id,"tab_id":root.tab_id,"pane_id":root.pane_id,
        "agent_session":{"kind":"id","value":"native-child"}})).unwrap();
    let id = super::adopt_child(app,"local",&client,"test",&agent,"renamed",&parent,"new-display","new-display","claude").await.unwrap();
    assert_eq!(id, kid, "renaming must reuse the proven native identity");
    sqlx::query("UPDATE runs SET state='exited' WHERE bot_id=?").bind(&kid).execute(&app.db).await.unwrap();
    let mut reused = agent.clone();
    reused.pane_id = other.pane_id;
    reused.tab_id = other.tab_id;
    reused.agent_session = Some(json!({"kind":"id","value":"another-native"}));
    let id = super::adopt_child(app,"local",&client,"test",&reused,"old",&parent,"display-renamed","display-renamed","claude").await.unwrap();
    assert_ne!(id, kid, "reusing a nickname must not inherit an old child");
}

#[tokio::test]
async fn nr1791_child_rename_with_native_evidence_is_independent_of_claimed_names() {
    let env = tt::env().await;
    let app = &env.app;
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let pane = client.tab_create(&ws.workspace_id,"/tmp/p","kid",json!({})).await.unwrap();
    let parent = a_bot(&env,"alfa").await;
    let parent_run = db::ulid();
    sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,pane_id,herdr_session,agent_name,started_at) VALUES (?,?,'running','idle',?,'test','renamed',?)")
        .bind(&parent_run).bind(&parent).bind(&root.pane_id).bind(db::now()).execute(&app.db).await.unwrap();
    let (kid, run) = a_child(&env,&parent,"kid","old",&ws.workspace_id,&pane.tab_id,&pane.pane_id).await;
    sqlx::query("UPDATE runs SET native_session_id='child-instance' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
    *env.herdr.agents.lock().unwrap() = vec![
        json!({"name":"renamed","agent":"claude","agent_status":"idle","workspace_id":ws.workspace_id,"tab_id":root.tab_id,"pane_id":root.pane_id}),
        json!({"name":"renamed","agent":"claude","agent_status":"working","workspace_id":ws.workspace_id,"tab_id":pane.tab_id,"pane_id":pane.pane_id,"agent_session":{"kind":"id","value":"child-instance"}})];
    super::reconcile_host(app,"local").await.unwrap();
    let kept = db::active_run(&app.db,&kid).await.unwrap().unwrap();
    assert_eq!(kept.id,run);
    assert_eq!(kept.agent_name.as_deref(),Some("renamed"));
    assert_eq!(kept.agent_status,"working");
    assert_eq!(db::bot(&app.db,&kid).await.unwrap().unwrap().parent_bot_id.as_deref(),Some(parent.as_str()));
    assert_eq!(all_children(app).await.len(),1);
}

#[tokio::test]
async fn nr1791_parent_replacement_requires_explicit_ownership_after_parent_rename() {
    let env = tt::env().await;
    let app = &env.app;
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let old = client.tab_create(&ws.workspace_id,"/tmp/p","old",json!({})).await.unwrap();
    let next = client.tab_create(&ws.workspace_id,"/tmp/p","next",json!({})).await.unwrap();
    let parent = a_bot(&env,"alfa").await;
    sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,pane_id,herdr_session,agent_name,started_at) VALUES (?,?,'running','idle',?,'test','stale-name',?)")
        .bind(db::ulid()).bind(&parent).bind(&root.pane_id).bind(db::now()).execute(&app.db).await.unwrap();
    let (child, _) = a_child(&env,&parent,"old","stale-name-old",&ws.workspace_id,&old.tab_id,&old.pane_id).await;
    client.pane_close(&old.pane_id).await.unwrap();
    let child = db::bot(&app.db,&child).await.unwrap().unwrap();
    let agents: Vec<crate::herdr::AgentInfo> = serde_json::from_value(json!([
        {"name":"changed","agent":"claude","agent_status":"idle","workspace_id":ws.workspace_id,"tab_id":root.tab_id,"pane_id":root.pane_id},
        {"name":"changed-successor","agent":"claude","agent_status":"idle","workspace_id":ws.workspace_id,"tab_id":root.tab_id,"pane_id":next.pane_id}])).unwrap();
    for (owner, expected) in [(None,false),(Some(parent.clone()),true),(Some("other-bot".into()),false)] {
        let hints = owner.map(|o| std::collections::HashMap::from([(next.pane_id.clone(),o)])).unwrap_or_default();
        let claimed = std::collections::HashSet::new();
        let result = super::parent_replaced_child(app,"local","test",&client,&child,
            super::ParentReplacementEvidence { agents:&agents,hints:&hints,claimed:&claimed }).await.unwrap();
        assert_eq!(result,expected,"only the explicit parent ID establishes replacement ownership");
    }
}

#[tokio::test]
async fn an_explicit_claim_still_adopts_a_named_child_after_its_parent_has_ended() {
    let env = tt::env().await;
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let parent = a_bot(&env, "alfa").await;
    crate::spawn_hints::claim(&env.app, &parent, &root.pane_id).await.unwrap();
    *env.herdr.agents.lock().unwrap() = vec![json!({"name":"unrelated-display", "agent":"claude", "agent_status":"idle",
        "workspace_id":ws.workspace_id,"tab_id":root.tab_id,"pane_id":root.pane_id})];
    super::reconcile_host(&env.app,"local").await.unwrap();
    let children = all_children(&env.app).await;
    assert_eq!(children.len(),1);
    assert_eq!(children[0].parent_bot_id.as_deref(),Some(parent.as_str()));
    assert_eq!(db::active_run(&env.app.db,&children[0].id).await.unwrap().unwrap().pane_id.as_deref(),Some(root.pane_id.as_str()));
}

async fn bind_listed_instances(env: &tt::Env) {
    let agents = {
        let mut agents = env.herdr.agents.lock().unwrap();
        for a in agents.iter_mut() {
            let pane = a["pane_id"].as_str().unwrap().to_string();
            a["agent_session"] = json!({"kind":"id","value":pane});
        }
        agents.clone()
    };
    for a in agents {
        let pane = a["pane_id"].as_str().unwrap();
        sqlx::query("UPDATE runs SET native_session_id=? WHERE pane_id=? AND herdr_session='test'")
            .bind(pane).bind(pane).execute(&env.app.db).await.unwrap();
    }
}

async fn register_ended_identity(env: &tt::Env, bot: &str, pane: &crate::herdr::PaneInfo) {
    sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,pane_id,herdr_session,native_session_id,started_at,ended_at) VALUES (?,?,'exited','idle',?,'test',?,'2020-01-01T00:00:00Z','2020-01-01T00:01:00Z')")
        .bind(db::ulid()).bind(bot).bind(&pane.pane_id).bind(&pane.pane_id).execute(&env.app.db).await.unwrap();
    bind_listed_instances(env).await;
}

async fn assert_adoption_waits_for_delete_lock_and_rechecks(project_deleted: bool) {
    let env = tt::env().await;
    let app = env.app.clone();
    let parent_id = a_bot(&env, "adoption-parent").await;
    let parent = db::bot(&app.db, &parent_id).await.unwrap().unwrap();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let pane = client.tab_create(&ws.workspace_id, "/tmp/p", "kid", json!({})).await.unwrap();
    let agent: crate::herdr::AgentInfo = serde_json::from_value(json!({
        "name": "proj-adoption-parent-kid", "agent": "claude", "agent_status": "idle",
        "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"
    }))
    .unwrap();

    // delete_bot holds this lock across its snapshot, soft delete, and cleanup. Adoption must
    // queue behind it and then reject the stale parent/project snapshot after deletion.
    let guard = app.bot_lock(&parent.id).await.lock_owned().await;
    let (app2, client2, parent2, agent2) = (app.clone(), client.clone(), parent.clone(), agent.clone());
    let mut adoption = tokio::spawn(async move {
        super::adopt_child(&app2, "local", &client2, "test", &agent2, "proj-adoption-parent-kid", &parent2, "kid", "kid", "claude").await
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut adoption).await.is_err(),
        "adoption must share the parent lock instead of inserting a child during delete"
    );
    if project_deleted {
        sqlx::query("UPDATE projects SET deleted_at=? WHERE id=?")
            .bind(db::now())
            .bind(&parent.project_id)
            .execute(&app.db)
            .await
            .unwrap();
    } else {
        sqlx::query("UPDATE bots SET deleted_at=? WHERE id=?")
            .bind(db::now())
            .bind(&parent.id)
            .execute(&app.db)
            .await
            .unwrap();
    }
    drop(guard);
    // 放鎖之後 adopt_child 還要拿 parent 的鎖、重讀 DB；忙的機器上 2 秒不夠。只是放棄的上限，結束得早就早回。
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), adoption).await.unwrap().unwrap();
    assert!(result.is_err(), "stale reconciliation state must not resurrect a deleted owner: {result:?}");
    let bots: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bots WHERE parent_bot_id=?")
        .bind(&parent.id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    let runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE bot_id IN (SELECT id FROM bots WHERE parent_bot_id=?)")
        .bind(&parent.id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(bots, 0, "deleted parent/project must not acquire an orphan child");
    assert_eq!(runs, 0, "rejected adoption must not leave a live run");
}

#[tokio::test]
async fn child_adoption_waits_for_parent_delete_lock_and_rechecks_parent_liveness() {
    assert_adoption_waits_for_delete_lock_and_rechecks(false).await;
}

#[tokio::test]
async fn child_adoption_waits_for_project_delete_lock_and_rechecks_project_liveness() {
    assert_adoption_waits_for_delete_lock_and_rechecks(true).await;
}

/// 重啟時卡在送出途中的那一筆，開機要有人收尾——否則那顆 bot 之後每則 prompt 都 409，
/// 而且 AGM 的 safety 會把它讀成「daemon 正在打字」，重啟窗口永遠拿不到。
#[tokio::test]
async fn a_prompt_caught_mid_delivery_by_a_restart_is_marked_unknown() {
    let env = tt::env().await;
    let app = &env.app;
    let bot = a_bot(&env, "mid-flight").await;
    let run = db::ulid();
    sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES (?,?,'running','idle',?)")
        .bind(&run).bind(&bot).bind(db::now()).execute(&app.db).await.unwrap();
    let conv = db::conversation_id(&app.db, &bot).await.unwrap();
    let turn = db::ulid();
    sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,created_at) VALUES (?,?,?,'web','in_flight','pending',?)")
        .bind(&turn).bind(&conv).bind(&run).bind(db::now()).execute(&app.db).await.unwrap();

    super::rearm_progress(app).await;

    let t = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn).fetch_one(&app.db).await.unwrap();
    assert_eq!(t.delivery, "unknown", "鍵可能按下去了，不能當成沒送");
    assert_eq!(t.status, "in_flight", "收尾的是送達狀態，不是把回合結掉——要留給人決定");
    assert_eq!(t.auto_resend, 0, "鍵可能按過了：舊版本留下的可重送預設也關掉（#149）");
    assert!(t.delivered_at.is_some(), "送出最晚就是重啟那一刻：不退回 created_at（#149）");

    // 已經證出來送到的那種不要動它。
    sqlx::query("UPDATE turns SET delivery='ok' WHERE id=?").bind(&turn).execute(&app.db).await.unwrap();
    super::rearm_progress(app).await;
    let t = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn).fetch_one(&app.db).await.unwrap();
    assert_eq!(t.delivery, "ok");
}

/// review 2026-09-16 deliv L3：排隊半小時、剛剛才 flush 送出就遇上重啟的那一筆，watchdog 要補回來——
/// 「剛送出」看 `delivered_at`，不看排隊時的 `created_at`；沒有 `delivered_at` 的舊列照舊看 `created_at`。
#[tokio::test]
async fn a_queued_turn_sent_just_before_a_restart_gets_its_watchdog_back() {
    let env = tt::env().await;
    let app = &env.app;
    let long_ago = (chrono::Utc::now() - chrono::Duration::minutes(30)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut runs = Vec::new();
    for (name, delivered_at) in [("flushed", Some(db::now())), ("old", None)] {
        let bot = a_bot(&env, name).await;
        let run = db::ulid();
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES (?,?,'running','idle',?)")
            .bind(&run).bind(&bot).bind(db::now()).execute(&app.db).await.unwrap();
        let conv = db::conversation_id(&app.db, &bot).await.unwrap();
        sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,created_at,delivered_at) VALUES (?,?,?,'web','in_flight','ok',?,?)")
            .bind(db::ulid()).bind(&conv).bind(&run).bind(&long_ago).bind(delivered_at).execute(&app.db).await.unwrap();
        runs.push(run);
    }
    super::rearm_progress(app).await;
    let timers = app.stall_timers.lock().await;
    assert!(timers.contains_key(&runs[0]), "排隊很久、剛送出：要補");
    assert!(!timers.contains_key(&runs[1]), "沒有 delivered_at 的舊列照舊看 created_at：半小時前的不補");
}

/// `delivered_at` 由 `mark_delivery` 寫、只記第一次：poller 事後補證據再記一次，不能讓舊的看起來像剛送出。
#[tokio::test]
async fn the_delivery_time_is_recorded_once() {
    let env = tt::env().await;
    let app = &env.app;
    let bot = a_bot(&env, "sent").await;
    let conv = db::conversation_id(&app.db, &bot).await.unwrap();
    let turn = db::ulid();
    let long_ago = (chrono::Utc::now() - chrono::Duration::minutes(30)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    sqlx::query("INSERT INTO turns (id,conversation_id,origin,status,delivery,created_at) VALUES (?,?,'web','in_flight','pending',?)")
        .bind(&turn).bind(&conv).bind(&long_ago).execute(&app.db).await.unwrap();
    let at = |app: Arc<App>, turn: String| async move {
        sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(turn).fetch_one(&app.db).await.unwrap().delivered_at
    };
    crate::lifecycle::mark_delivery(app, &turn, crate::lifecycle::Delivered::Handed.record().unwrap(), &db::now()).await.unwrap();
    let first = at(app.clone(), turn.clone()).await.expect("送出時記下");
    assert!(first > long_ago);
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    crate::lifecycle::mark_delivery(app, &turn, crate::lifecycle::Delivered::Unverified.record().unwrap(), &db::now()).await.unwrap();
    assert_eq!(at(app.clone(), turn).await, Some(first), "只記第一次");
}

/// 補 stall watchdog 只補剛送出的：對幾小時前的 turn 補，等於 12 秒後把舊訊息再送一次。
#[test]
fn only_a_freshly_sent_turn_gets_its_watchdog_back() {
    let now = chrono::Utc::now();
    let iso = |d: chrono::Duration| (now - d).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    // 固定的「現在」：iso 與判斷用同一個時刻，邊界不受 runner 快慢影響。
    let fresh = |d: chrono::Duration| super::fresh_enough_at(&iso(d), now);
    assert!(fresh(chrono::Duration::seconds(5)));
    assert!(fresh(chrono::Duration::seconds(119)));
    assert!(fresh(chrono::Duration::seconds(120)), "剛好 120 秒還算新");
    assert!(!fresh(chrono::Duration::seconds(121)));
    assert!(!fresh(chrono::Duration::seconds(-1)), "未來的送出時間不該當成剛送出");
    assert!(!fresh(chrono::Duration::hours(3)));
    assert!(!super::fresh_enough_at("not-a-time", now), "讀不懂時間就不要補");
    assert!(super::fresh_enough(&iso(chrono::Duration::seconds(5))), "帶牆鐘的版本只驗大方向");
}

// ---- #75 重開：開機恢復讀不到不算做完 ----

fn ago(d: chrono::Duration) -> String {
    (chrono::Utc::now() - d).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

async fn a_run(app: &Arc<App>, bot: &str, state: &str, started_at: &str, ended_at: Option<&str>) -> String {
    let run = db::ulid();
    sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at,ended_at) VALUES (?,?,?,'idle',?,?)")
        .bind(&run)
        .bind(bot)
        .bind(state)
        .bind(started_at)
        .bind(ended_at)
        .execute(&app.db)
        .await
        .unwrap();
    run
}

/// 直接寫進 `table`（讀不到的那段時間 `turns` 改了名，要塞「重啟之後才有」的列就寫進改名後的表）。
#[allow(clippy::too_many_arguments)]
async fn a_turn(app: &Arc<App>, table: &str, bot: &str, run: Option<&str>, status: &str, delivery: &str, created_at: &str, delivered_at: Option<&str>) -> String {
    let conv = db::conversation_id(&app.db, bot).await.unwrap();
    let id = db::ulid();
    sqlx::query(&format!(
        "INSERT INTO {table} (id,conversation_id,run_id,origin,status,delivery,created_at,delivered_at,next_flush_at,prompt_text)
             VALUES (?,?,?,'web',?,?,?,?,?,'hi')"
    ))
    .bind(&id)
    .bind(&conv)
    .bind(run)
    .bind(status)
    .bind(delivery)
    .bind(created_at)
    .bind(delivered_at)
    // 排著的那一則：一小時後才到期，timer 掛上之後會一直掛著，看得到。
    .bind((status == "queued").then(|| ago(chrono::Duration::hours(-1))))
    .execute(&app.db)
    .await
    .unwrap();
    id
}

async fn turn_state(app: &Arc<App>, table: &str, id: &str) -> (String, String) {
    sqlx::query_as(&format!("SELECT status, delivery FROM {table} WHERE id=?")).bind(id).fetch_one(&app.db).await.unwrap()
}

async fn rename(app: &Arc<App>, from: &str, to: &str) {
    sqlx::query(&format!("ALTER TABLE {from} RENAME TO {to}")).execute(&app.db).await.unwrap();
}

async fn polled(app: &Arc<App>, run: &str) -> bool {
    app.progress_pollers.lock().await.contains_key(run)
}

async fn stall_gen(app: &Arc<App>, run: &str) -> Option<u64> {
    app.stall_timers.lock().await.get(run).copied()
}

use crate::testing::eventually;

/// **#75 重開驗收**：開機那一次讀 `turns` 失敗（busy／I/O）。真相都還在 DB，但把它變回 poller／timer 的只有這一步——
/// 以前讀不到就回 0／跳過，那個回合重啟後沒人盯，閒著的 bot 也不會再有事件叫醒排著的那一則。現在 DB 恢復之後，
/// **沒有任何 hook／狀態邊／新 prompt**，背景重試自己把每一件接回來；而重啟之後才出現的（這個行程自己的）一件都不碰。
#[tokio::test]
async fn a_startup_recovery_that_cannot_read_turns_catches_up_once_the_db_does() {
    let env = tt::env().await;
    let app = &env.app;
    let long_ago = ago(chrono::Duration::minutes(30));
    // 重啟前就在的五件。
    let flying = a_bot(&env, "flying").await;
    let r_flying = a_run(app, &flying, "running", &long_ago, None).await;
    a_turn(app, "turns", &flying, Some(&r_flying), "in_flight", "ok", &long_ago, Some(&db::now())).await;
    let orphan = a_bot(&env, "orphan").await;
    let r_orphan = a_run(app, &orphan, "running", &long_ago, None).await;
    let t_orphan = a_turn(app, "turns", &orphan, Some(&r_orphan), "in_flight", "pending", &long_ago, None).await;
    let waiting = a_bot(&env, "waiting").await;
    a_turn(app, "turns", &waiting, None, "queued", "pending", &long_ago, None).await;
    let send_now = a_bot(&env, "send-now").await;
    let t_send_now = a_turn(app, "turns", &send_now, None, "in_flight", "pending", &long_ago, None).await;
    let ended = a_bot(&env, "ended").await;
    let r_ended = a_run(app, &ended, "exited", &long_ago, Some(&long_ago)).await;
    let t_ended = a_turn(app, "turns", &ended, Some(&r_ended), "in_flight", "ok", &long_ago, Some(&long_ago)).await;

    rename(app, "turns", "turns_unreadable").await;
    super::rearm_progress(app).await;
    assert!(!polled(app, &r_flying).await && !polled(app, &r_orphan).await, "讀不到：這一次什麼都沒接回");
    assert!(stall_gen(app, &r_flying).await.is_none());
    assert!(!crate::lifecycle::queue_retry_timer_armed(&waiting));
    assert_eq!(turn_state(app, "turns_unreadable", &t_orphan).await.1, "pending");

    // 重啟之後才有的：這個行程自己送到一半的插隊送出、剛結束的 run 上還沒收的回合（收尾在這個行程的記憶體帳上）。
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let live_send_now = a_bot(&env, "live-send-now").await;
    let t_live_send_now = a_turn(app, "turns_unreadable", &live_send_now, None, "in_flight", "pending", &db::now(), None).await;
    let live_ended = a_bot(&env, "live-ended").await;
    let r_live_ended = a_run(app, &live_ended, "exited", &long_ago, Some(&db::now())).await;
    let t_live_ended = a_turn(app, "turns_unreadable", &live_ended, Some(&r_live_ended), "in_flight", "ok", &long_ago, Some(&long_ago)).await;

    rename(app, "turns_unreadable", "turns").await;
    let caught_up = eventually!(
        polled(app, &r_flying).await
            && polled(app, &r_orphan).await
            && stall_gen(app, &r_flying).await.is_some()
            && crate::lifecycle::queue_retry_timer_armed(&waiting)
            && turn_state(app, "turns", &t_orphan).await.1 == "unknown"
            && turn_state(app, "turns", &t_send_now).await.0 == "failed"
            && turn_state(app, "turns", &t_ended).await.0 == "failed"
    );
    assert!(caught_up, "DB 恢復之後，沒有任何事件也要全部接回");
    assert_eq!(turn_state(app, "turns", &t_send_now).await, ("failed".into(), "unknown".into()), "送出鍵生效了沒人知道");
    assert!(stall_gen(app, &r_orphan).await.is_none(), "送到一半的那一筆不補 watchdog（不能重送）");

    let armed = stall_gen(app, &r_flying).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(stall_gen(app, &r_flying).await, armed, "接回之後不再掛第二次");
    assert_eq!(turn_state(app, "turns", &t_live_send_now).await, ("in_flight".into(), "pending".into()), "重啟之後才送的插隊送出不是孤兒");
    assert_eq!(turn_state(app, "turns", &t_live_ended).await.0, "in_flight", "重啟之後才結束的 run，收尾歸這個行程的帳");
}

/// 只補還欠著的：一個 run 接回來了、另一個還寫不進去時，背景每一輪只碰欠著的那一個——接回來的 watchdog 不會一輪被換一次，
/// 開機時就確定沒事的 run 之後有了這個行程自己的回合（送達證不出來、所以沒掛 poller 的那種）也不被接手。
#[tokio::test]
async fn the_startup_recovery_retries_only_what_it_still_owes() {
    let env = tt::env().await;
    let app = &env.app;
    let long_ago = ago(chrono::Duration::minutes(30));
    let flying = a_bot(&env, "flying").await;
    let r_flying = a_run(app, &flying, "running", &long_ago, None).await;
    a_turn(app, "turns", &flying, Some(&r_flying), "in_flight", "ok", &long_ago, Some(&db::now())).await;
    let orphan = a_bot(&env, "orphan").await;
    let r_orphan = a_run(app, &orphan, "running", &long_ago, None).await;
    let t_orphan = a_turn(app, "turns", &orphan, Some(&r_orphan), "in_flight", "pending", &long_ago, None).await;
    let idle = a_bot(&env, "idle").await;
    let r_idle = a_run(app, &idle, "running", &long_ago, None).await;
    sqlx::query(
        "CREATE TRIGGER orphan_unwritable BEFORE UPDATE OF delivery ON turns WHEN NEW.delivery = 'unknown'
             BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END",
    )
    .execute(&app.db)
    .await
    .unwrap();

    super::rearm_progress(app).await;
    let armed = stall_gen(app, &r_flying).await;
    assert!(armed.is_some() && polled(app, &r_flying).await, "讀得到的那一個當場接回");
    assert!(!polled(app, &r_orphan).await, "收不成 unknown 就先不掛：欠著");
    // 重試期間：這個行程送到 `idle`，送達證不出來（`unknown`，照設計不掛 poller）。
    a_turn(app, "turns", &idle, Some(&r_idle), "in_flight", "unknown", &db::now(), Some(&db::now())).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!polled(app, &r_idle).await, "開機時就沒欠的 run，之後的回合是這個行程自己的");
    assert_eq!(stall_gen(app, &r_flying).await, armed, "背景重試只碰還欠著的");
    assert_eq!(turn_state(app, "turns", &t_orphan).await.1, "pending");

    sqlx::query("DROP TRIGGER orphan_unwritable").execute(&app.db).await.unwrap();
    assert!(
        eventually!(polled(app, &r_orphan).await && turn_state(app, "turns", &t_orphan).await.1 == "unknown"),
        "寫得進去之後接回"
    );
    assert_eq!(stall_gen(app, &r_flying).await, armed, "從頭到尾只掛一次");
}

/// 連 run 清單都讀不到時，欠著的是「開機前就在跑的 run」：重啟之後才開始的 run 不碰；重試期間這個行程自己已經盯上的 run
/// （新回合送出時掛了 poller）也不再掛一次。
#[tokio::test]
async fn the_startup_recovery_leaves_what_this_process_already_owns_alone() {
    let env = tt::env().await;
    let app = &env.app;
    let long_ago = ago(chrono::Duration::minutes(30));
    let flying = a_bot(&env, "flying").await;
    let r_flying = a_run(app, &flying, "running", &long_ago, None).await;
    a_turn(app, "turns", &flying, Some(&r_flying), "in_flight", "ok", &long_ago, Some(&db::now())).await;
    let taken = a_bot(&env, "taken").await;
    let r_taken = a_run(app, &taken, "running", &long_ago, None).await;
    let t_taken = a_turn(app, "turns", &taken, Some(&r_taken), "in_flight", "ok", &long_ago, Some(&db::now())).await;

    rename(app, "runs", "runs_unreadable").await;
    super::rearm_progress(app).await;
    assert!(!polled(app, &r_flying).await, "run 清單讀不到：這一次什麼都沒接回");

    // 重試期間：這個行程送出了 `taken` 的回合、自己掛上 poller 與 watchdog；另一顆 bot 重啟之後才開始跑。
    crate::lifecycle::arm_progress(app, &r_taken, &taken, &t_taken).await;
    crate::lifecycle::arm_stall(app, &r_taken, &taken, &t_taken).await;
    let own = stall_gen(app, &r_taken).await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let fresh = a_bot(&env, "fresh").await;
    let r_fresh = db::ulid();
    sqlx::query("INSERT INTO runs_unreadable (id,bot_id,state,agent_status,started_at) VALUES (?,?,'running','idle',?)")
        .bind(&r_fresh)
        .bind(&fresh)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
    a_turn(app, "turns", &fresh, Some(&r_fresh), "in_flight", "ok", &db::now(), Some(&db::now())).await;

    rename(app, "runs_unreadable", "runs").await;
    assert!(
        eventually!(polled(app, &r_flying).await && stall_gen(app, &r_flying).await.is_some()),
        "開機前就在跑的那一個接回來"
    );
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(stall_gen(app, &r_taken).await, own, "這個行程已經盯著的不再掛一次");
    assert!(!polled(app, &r_fresh).await && stall_gen(app, &r_fresh).await.is_none(), "重啟之後才開始的 run 不是開機恢復的事");
}

/// A give-up stop's `stopping` run is healed while herdr lists the agent (review 2026-09-12 #1).
#[tokio::test]
async fn a_run_stuck_in_stopping_is_healed_while_its_agent_is_listed() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let pane = client.tab_create(&ws.workspace_id, "/tmp/p", "alfa", json!({})).await.unwrap();
    let bot = a_bot(&env, "alfa").await;
    let agent = crate::config::agent_name("proj", &bot);
    let run = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'stopping','idle',?,?,?,?,'test',?)",
    )
    .bind(&run)
    .bind(&bot)
    .bind(&ws.workspace_id)
    .bind(&pane.tab_id)
    .bind(&pane.pane_id)
    .bind(&agent)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    *env.herdr.agents.lock().unwrap() = vec![json!({
        "name": agent, "agent": "claude", "agent_status": "idle",
        "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id,
        "cwd": "/tmp/p"})];

    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    let r = run_of(&app, &bot).await.unwrap();
    assert_eq!(r.id, run);
    assert_eq!(r.state, "running");
}

/// **Name collisions must not end the reconcile** (review 2026-09-12 #2): two parents' `ui`,
/// and a child `review` next to the user's bot `review`.
#[tokio::test]
async fn colliding_child_names_are_stored_under_their_agent_name_and_never_abort_the_pass() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let bravo_pane = client.tab_create(&ws.workspace_id, "/tmp/p", "bravo", json!({})).await.unwrap();
    let alfa_ui = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
    let alfa_review = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
    let bravo_ui = client.pane_split(&bravo_pane.pane_id, "right", "/tmp/p", json!({})).await.unwrap();

    let alfa = a_bot(&env, "alfa").await;
    let bravo = a_bot(&env, "bravo").await;
    let review = a_bot(&env, "review").await;
    let alfa_agent = crate::config::agent_name("proj", &alfa);
    let bravo_agent = crate::config::agent_name("proj", &bravo);
    for (bot, agent, pane) in [(&alfa, &alfa_agent, &root), (&bravo, &bravo_agent, &bravo_pane)] {
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, started_at)
                 VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
        )
        .bind(db::ulid())
        .bind(bot)
        .bind(&ws.workspace_id)
        .bind(&pane.tab_id)
        .bind(&pane.pane_id)
        .bind(agent)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
    }
    let entry = |name: String, pane: &crate::herdr::PaneInfo| {
        json!({"name": name, "agent": "claude", "agent_status": "idle",
               "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"})
    };
    *env.herdr.agents.lock().unwrap() = vec![
        entry(alfa_agent.clone(), &root),
        entry(bravo_agent.clone(), &bravo_pane),
        entry(format!("{alfa_agent}-ui"), &alfa_ui),
        entry(format!("{alfa_agent}-review"), &alfa_review),
        entry(format!("{bravo_agent}-ui"), &bravo_ui),
    ];

    crate::spawn_hints::claim(&app, &alfa, &alfa_ui.pane_id).await.unwrap();
    crate::spawn_hints::claim(&app, &alfa, &alfa_review.pane_id).await.unwrap();
    crate::spawn_hints::claim(&app, &bravo, &bravo_ui.pane_id).await.unwrap();
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.expect("one bad name must not end the pass");

    let kids = |parent: String| {
        let db = app.db.clone();
        async move {
            sqlx::query_as::<_, db::Bot>(
                "SELECT * FROM bots WHERE parent_bot_id = ? AND managed_by = 'child' AND deleted_at IS NULL ORDER BY name",
            )
            .bind(&parent)
            .fetch_all(&db)
            .await
            .unwrap()
        }
    };
    let alfa_kids = kids(alfa.clone()).await;
    let bravo_kids = kids(bravo.clone()).await;
    assert_eq!(
        alfa_kids.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
        [format!("{alfa_agent}-review").as_str(), "ui"],
        "alfa's `ui` keeps the short name; its `review` yields to the user's bot of that name"
    );
    assert_eq!(
        bravo_kids.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
        [format!("{bravo_agent}-ui").as_str()],
        "bravo's `ui` is a different child, stored under its full agent name"
    );
    let user_review = db::bot(&app.db, &review).await.unwrap().unwrap();
    assert_eq!(user_review.managed_by, "user");
    assert!(user_review.parent_bot_id.is_none(), "the user's bot was not touched");
    for k in alfa_kids.iter().chain(bravo_kids.iter()) {
        let runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE bot_id = ?")
            .bind(&k.id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(runs, 1, "{}: one adopted run", k.name);
    }

    // A second pass never re-parents or duplicates an occupied child pane.
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
    assert_eq!(kids(alfa.clone()).await.len(), 2);
    assert_eq!(kids(bravo.clone()).await.len(), 1);
    let all_runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runs").fetch_one(&app.db).await.unwrap();
    assert_eq!(all_runs, 5, "two parents + three children, one run each");
}

#[tokio::test]
async fn an_adopted_agent_records_the_tab_it_is_in() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let pane = client.tab_create(&ws.workspace_id, "/tmp/p", "alfa", json!({})).await.unwrap();

    let bot = a_bot(&env, "alfa").await;
    let agent = crate::config::agent_name("proj", &bot);
    *env.herdr.agents.lock().unwrap() = vec![json!({
        "name": agent, "agent": "claude", "agent_status": "working",
        "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id,
        "cwd": "/tmp/p"})];

    register_ended_identity(&env, &bot, &pane).await;
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    let r = run_of(&app, &bot).await.expect("the agent was adopted");
    assert_eq!(r.state, "running");
    assert_eq!(r.adopted, 1);
    assert_eq!(r.pane_id.as_deref(), Some(pane.pane_id.as_str()));
    assert_eq!(r.tab_id.as_deref(), Some(pane.tab_id.as_str()));
}

/// 隔離實例（`serve --config` 到別的目錄）不能收編既有 pane：那顆 pane 的 hook 寫著別顆
/// daemon 的資料目錄，收編之後兩顆會互相吃對方的 spool（sol 複審二輪）。
#[tokio::test]
async fn an_isolated_instance_refuses_to_adopt_an_existing_pane() {
    let env = tt::env().await;
    let app = env.app.clone();
    app.set_instance(Some("iso-test".into()));
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let pane = client.tab_create(&ws.workspace_id, "/tmp/p", "alfa", json!({})).await.unwrap();

    let bot = a_bot(&env, "alfa").await;
    let agent = crate::config::agent_name("proj", &bot);
    *env.herdr.agents.lock().unwrap() = vec![json!({
        "name": agent, "agent": "claude", "agent_status": "working",
        "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id,
        "cwd": "/tmp/p"})];

    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
    assert!(run_of(&app, &bot).await.is_none(), "隔離實例不該把正式 daemon 的 pane 收編成自己的 run");

    // 正式實例照收（同一組輸入，只差這個旗標）。
    app.set_instance(None);
    register_ended_identity(&env, &bot, &pane).await;
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
    assert_eq!(run_of(&app, &bot).await.expect("正式實例照舊收編").adopted, 1);
}

/// 正式實例的收編沒被隔離閘門關掉——不靠測試 setter：`App::new` 從行程層級的實例名初始化，
/// 測試行程從沒叫過 `startup::set_instance`，所以這就是 `slug = None` 的真實接線（sol 三輪 non-blocking）。
#[tokio::test]
async fn a_production_instance_still_adopts_through_reconcile_and_the_default_session() {
    let env = tt::env().await;
    let app = env.app.clone();
    assert_eq!(crate::startup::instance(), None);
    assert_eq!(app.instance(), None);
    assert!(!app.isolated());
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();

    // reconcile：自己開的 bot pane。
    let pane = client.tab_create(&ws.workspace_id, "/tmp/p", "alfa", json!({})).await.unwrap();
    let bot = a_bot(&env, "alfa").await;
    let agent = crate::config::agent_name("proj", &bot);
    *env.herdr.agents.lock().unwrap() = vec![json!({
        "name": agent, "agent": "claude", "agent_status": "working",
        "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id,
        "cwd": "/tmp/p"})];
    register_ended_identity(&env, &bot, &pane).await;
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
    assert_eq!(run_of(&app, &bot).await.expect("reconcile 照收").adopted, 1);

    // default session：使用者自己在專案目錄裡開的 agent。收編會寫回 config.toml，所以專案要在裡面。
    let repo = env.repo.to_string_lossy().to_string();
    let (pid, path, alfa) = (env.project_id.clone(), repo.clone(), bot.clone());
    app.cfg
        .update(move |cfg| {
            let b: crate::config::BotCfg =
                toml::from_str(&format!("id = '{alfa}'\nname = 'alfa'\nkind = 'claude'\n")).unwrap();
            cfg.projects = vec![crate::config::ProjectCfg { mode: "parent_child".into(), role_policy: Default::default(), deputy: None, specialist_dispatch: true,
                handed_off_to: None,
                id: Some(pid),
                path,
                label: "proj".into(),
                host: crate::config::LOCAL_HOST.into(),
                bots: vec![b],
            }];
            Ok(())
        })
        .await
        .unwrap();
    let user_pane = client.tab_create(&ws.workspace_id, &repo, "mine", json!({})).await.unwrap();
    *env.herdr.agents.lock().unwrap() = vec![json!({
        "name": "users-own", "agent": "claude", "agent_status": "idle",
        "workspace_id": ws.workspace_id, "tab_id": user_pane.tab_id, "pane_id": user_pane.pane_id,
        "cwd": repo})];
    crate::default_session::sync(&app).await.unwrap();
    let adopted: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE pane_id = ? AND adopted = 1")
        .bind(&user_pane.pane_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(adopted, 1, "default session 照收");
}

/// Until the mock herdr was asked `method`, i.e. the reconcile is heading for a bot's lock.
async fn wait_for_call(env: &tt::Env, method: &str) {
    for _ in 0..200 {
        if env.herdr.methods().iter().any(|m| m == method) {
            // One beat more so it is actually parked on the lock, not still between calls.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("reconcile never called {method}");
}

/// **2026-09-10 23:02 (AGM 停 5.5 小時).** Reconcile parked on the lock behind `stop_bot`
/// adopted the stale-listed agent, so `start_bot` refused. Gone by lock time → not adopted.
#[tokio::test]
async fn an_agent_that_left_while_reconcile_waited_for_the_lock_is_not_adopted() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let pane = client.tab_create(&ws.workspace_id, "/tmp/p", "alfa", json!({})).await.unwrap();
    let bot = a_bot(&env, "alfa").await;
    let agent = crate::config::agent_name("proj", &bot);
    let run_id = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
    )
    .bind(&run_id)
    .bind(&bot)
    .bind(&ws.workspace_id)
    .bind(&pane.tab_id)
    .bind(&pane.pane_id)
    .bind(&agent)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    *env.herdr.agents.lock().unwrap() = vec![json!({
        "name": agent, "agent": "claude", "agent_status": "idle",
        "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"})];

    // `stop_bot` is holding the bot's lock…
    let guard = app.bot_lock(&bot).await.lock_owned().await;
    let app2 = app.clone();
    let rec = tokio::spawn(async move { super::reconcile_host(&app2, crate::config::LOCAL_HOST).await });
    wait_for_call(&env, "agent.list").await;

    // …and finishes the stop while the reconcile waits: agent gone, pane closed, run ended.
    env.herdr.agents.lock().unwrap().clear();
    client.pane_close(&pane.pane_id).await.unwrap();
    sqlx::query("UPDATE runs SET state='stopped', ended_at=? WHERE id=?")
        .bind(db::now())
        .bind(&run_id)
        .execute(&app.db)
        .await
        .unwrap();
    drop(guard);
    rec.await.unwrap().unwrap();

    // What `start_bot` finds next is the only thing that matters: nothing in its way.
    assert!(
        db::active_run(&app.db, &bot).await.unwrap().is_none(),
        "reconcile adopted the agent that had just been stopped, so the restart will refuse with `active run already exists`"
    );
}

/// A stale list must not roll a run's status back (review 2026-09-12 a).
#[tokio::test]
async fn a_stale_agent_list_does_not_roll_a_runs_status_back() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let pane = client.tab_create(&ws.workspace_id, "/tmp/p", "alfa", json!({})).await.unwrap();
    let bot = a_bot(&env, "alfa").await;
    let agent = crate::config::agent_name("proj", &bot);
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
    )
    .bind(db::ulid())
    .bind(&bot)
    .bind(&ws.workspace_id)
    .bind(&pane.tab_id)
    .bind(&pane.pane_id)
    .bind(&agent)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    *env.herdr.agents.lock().unwrap() = vec![json!({
        "name": agent, "agent": "claude", "agent_status": "idle",
        "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"})];

    let guard = app.bot_lock(&bot).await.lock_owned().await;
    let app2 = app.clone();
    let rec = tokio::spawn(async move { super::reconcile_host(&app2, crate::config::LOCAL_HOST).await });
    wait_for_call(&env, "agent.list").await;

    // The agent starts working while the reconcile is parked on the lock; the event
    // handler records that in the DB.
    env.herdr.agents.lock().unwrap()[0]["agent_status"] = json!("working");
    sqlx::query("UPDATE runs SET agent_status='working' WHERE bot_id=?")
        .bind(&bot)
        .execute(&app.db)
        .await
        .unwrap();
    drop(guard);
    rec.await.unwrap().unwrap();

    let r = run_of(&app, &bot).await.unwrap();
    assert_eq!(r.agent_status, "working", "the reconcile asked herdr again instead of trusting its stale list");
}

/// The other half of the race: the restart won the lock; the stale list names the old pane.
#[tokio::test]
async fn a_stale_agent_list_does_not_move_a_fresh_run_back_to_the_old_pane() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let old = client.tab_create(&ws.workspace_id, "/tmp/p", "alfa", json!({})).await.unwrap();
    let bot = a_bot(&env, "alfa").await;
    let agent = crate::config::agent_name("proj", &bot);
    let old_run = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
    )
    .bind(&old_run)
    .bind(&bot)
    .bind(&ws.workspace_id)
    .bind(&old.tab_id)
    .bind(&old.pane_id)
    .bind(&agent)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    *env.herdr.agents.lock().unwrap() = vec![json!({
        "name": agent, "agent": "claude", "agent_status": "idle",
        "workspace_id": ws.workspace_id, "tab_id": old.tab_id, "pane_id": old.pane_id, "cwd": "/tmp/p"})];

    let guard = app.bot_lock(&bot).await.lock_owned().await;
    let app2 = app.clone();
    let rec = tokio::spawn(async move { super::reconcile_host(&app2, crate::config::LOCAL_HOST).await });
    wait_for_call(&env, "agent.list").await;

    client.pane_close(&old.pane_id).await.unwrap();
    sqlx::query("UPDATE runs SET state='stopped', ended_at=? WHERE id=?")
        .bind(db::now())
        .bind(&old_run)
        .execute(&app.db)
        .await
        .unwrap();
    let new = client.tab_create(&ws.workspace_id, "/tmp/p", "alfa", json!({})).await.unwrap();
    let new_run = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
    )
    .bind(&new_run)
    .bind(&bot)
    .bind(&ws.workspace_id)
    .bind(&new.tab_id)
    .bind(&new.pane_id)
    .bind(&agent)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    *env.herdr.agents.lock().unwrap() = vec![json!({
        "name": agent, "agent": "claude", "agent_status": "idle",
        "workspace_id": ws.workspace_id, "tab_id": new.tab_id, "pane_id": new.pane_id, "cwd": "/tmp/p"})];
    drop(guard);
    rec.await.unwrap().unwrap();

    let r = db::active_run(&app.db, &bot).await.unwrap().expect("the new run survives");
    assert_eq!(r.id, new_run);
    assert_eq!(r.pane_id.as_deref(), Some(new.pane_id.as_str()), "the new run was moved onto the closed pane");
}

/// 2026-09-11: after a same-named restart herdr briefly cannot confirm the agent; that must not
/// read as "agent gone" (every bot in the batch was exited and its new pane swept).
async fn a_fresh_run_survives_when_herdr_cannot_confirm_its_agent(herdr_still_lists_the_old_pane: bool) {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let old = client.tab_create(&ws.workspace_id, "/tmp/p", "alfa", json!({})).await.unwrap();
    let bot = a_bot(&env, "alfa").await;
    let agent = crate::config::agent_name("proj", &bot);
    let old_run = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
    )
    .bind(&old_run)
    .bind(&bot)
    .bind(&ws.workspace_id)
    .bind(&old.tab_id)
    .bind(&old.pane_id)
    .bind(&agent)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let old_entry = json!({
        "name": agent, "agent": "claude", "agent_status": "idle",
        "workspace_id": ws.workspace_id, "tab_id": old.tab_id, "pane_id": old.pane_id, "cwd": "/tmp/p"});
    *env.herdr.agents.lock().unwrap() = vec![old_entry.clone()];

    let guard = app.bot_lock(&bot).await.lock_owned().await;
    let app2 = app.clone();
    let rec = tokio::spawn(async move { super::reconcile_host(&app2, crate::config::LOCAL_HOST).await });
    wait_for_call(&env, "agent.list").await;

    client.pane_close(&old.pane_id).await.unwrap();
    sqlx::query("UPDATE runs SET state='stopped', ended_at=? WHERE id=?")
        .bind(db::now())
        .bind(&old_run)
        .execute(&app.db)
        .await
        .unwrap();
    let new = client.tab_create(&ws.workspace_id, "/tmp/p", "alfa", json!({})).await.unwrap();
    let new_run = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
    )
    .bind(&new_run)
    .bind(&bot)
    .bind(&ws.workspace_id)
    .bind(&new.tab_id)
    .bind(&new.pane_id)
    .bind(&agent)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    // The new occupant's name cleared; in one variant the old entry lingers.
    let nameless = json!({
        "name": null, "agent": "claude", "agent_status": "idle",
        "workspace_id": ws.workspace_id, "tab_id": new.tab_id, "pane_id": new.pane_id, "cwd": "/tmp/p"});
    *env.herdr.agents.lock().unwrap() = vec![nameless];
    // herdr 以名字查 agent 仍回舊的、已關掉的 pane（cb12b83d：重啟後名字還沒換過來）。
    if herdr_still_lists_the_old_pane {
        env.herdr.set_stale_agent(&agent, &old.pane_id);
    }
    drop(guard);
    rec.await.unwrap().unwrap();

    let r = db::active_run(&app.db, &bot).await.unwrap().expect("the fresh run was marked exited");
    assert_eq!(r.id, new_run);
    assert_eq!(r.state, "running");
    assert_eq!(r.pane_id.as_deref(), Some(new.pane_id.as_str()), "the fresh run was moved off its pane");
    assert!(
        client.pane_get(&new.pane_id).await.unwrap().is_some(),
        "the orphan sweep closed the pane the restart had just opened"
    );
    let rename = env.herdr.first_call("agent.rename").expect("the cleared name is put back");
    assert_eq!(rename["target"], new.pane_id);
    assert_eq!(rename["name"], agent);
}

/// The real shape of 2026-09-11 on herdr 0.8.2: agent in its pane with `name: null`.
#[tokio::test]
async fn a_run_whose_agent_lost_its_name_is_kept_and_renamed() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let pane = client.tab_create(&ws.workspace_id, "/tmp/p", "alfa", json!({})).await.unwrap();
    let bot = a_bot(&env, "alfa").await;
    let agent = crate::config::agent_name("proj", &bot);
    let run_id = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'starting','unknown',?,?,?,?,'test',?)",
    )
    .bind(&run_id)
    .bind(&bot)
    .bind(&ws.workspace_id)
    .bind(&pane.tab_id)
    .bind(&pane.pane_id)
    .bind(&agent)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    *env.herdr.agents.lock().unwrap() = vec![json!({
        "name": null, "agent": "claude", "agent_status": "idle",
        "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"})];

    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    let r = db::active_run(&app.db, &bot).await.unwrap().expect("the run was marked exited over a lost name");
    assert_eq!(r.id, run_id);
    assert_eq!(r.state, "running");
    assert_eq!(r.agent_status, "idle", "status is read off the pane");
    assert_eq!(r.pane_id.as_deref(), Some(pane.pane_id.as_str()));
    let rename = env.herdr.first_call("agent.rename").expect("the name is put back");
    assert_eq!(rename["target"], pane.pane_id);
    assert_eq!(rename["name"], agent);
    assert_eq!(env.herdr.agents.lock().unwrap()[0]["name"], agent, "herdr knows the agent by name again");
    assert!(client.pane_get(&pane.pane_id).await.unwrap().is_some(), "the pane was not swept as an orphan");
}

#[tokio::test]
async fn a_fresh_run_survives_when_agent_get_says_not_found() {
    a_fresh_run_survives_when_herdr_cannot_confirm_its_agent(false).await;
}

#[tokio::test]
async fn a_fresh_run_survives_when_agent_get_still_points_at_the_closed_pane() {
    a_fresh_run_survives_when_herdr_cannot_confirm_its_agent(true).await;
}

/// A child row with a run on `pane`, the way reconcile adopts one (#60 tests).
async fn a_child(env: &tt::Env, parent: &str, name: &str, agent: &str, ws: &str, tab: &str, pane: &str) -> (String, String) {
    let bot = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, auto_approve, env_json, managed_by, parent_bot_id, hook_token, created_at)
             VALUES (?,?,?,'claude','[]',0,0,1,'{}','child',?,'tok',?)",
    )
    .bind(&bot)
    .bind(&env.project_id)
    .bind(name)
    .bind(parent)
    .bind(db::now())
    .execute(&env.app.db)
    .await
    .unwrap();
    let run = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, adopted, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,1,?,'test',?)",
    )
    .bind(&run)
    .bind(&bot)
    .bind(ws)
    .bind(tab)
    .bind(pane)
    .bind(agent)
    .bind(db::now())
    .execute(&env.app.db)
    .await
    .unwrap();
    (bot, run)
}

/// **#469**：孤兒 pane 的清理只比對 pane id，而 herdr 重開後 id 會被重用（`runs.pane_id` 結束後不清、
/// `runs` 也沒有保留期，所以每一顆歷史 run 的 id 都是候選）。使用者自己開的 shell 只要拿到其中一個 id
/// 就會被無條件關掉——那條路不是 `panes` GC，`user_pane`／`scratch`／`recent_output` 三條守門一條都不適用。
///
/// 分辨的依據是 `panes.first_seen`：真孤兒在同一輪裡是「先關、後掃」，關的時候還沒有 `panes` 列；
/// 有列而且 `first_seen` 晚於那個 run 的 `ended_at`，就是熬過至少一輪的另一顆 pane。
/// 這裡兩顆 pane 走同一條路，只差有沒有那一列。
#[tokio::test]
async fn a_reused_pane_id_belonging_to_a_newer_pane_is_not_closed_as_an_orphan() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    // 兩顆都是「活著、沒有 agent」的 pane，而且都掛在某個已結束的 run 的 pane id 上。
    let orphan = client.tab_create(&ws.workspace_id, "/tmp/p", "orphan", json!({})).await.unwrap();
    let reused = client.tab_create(&ws.workspace_id, "/tmp/p", "reused", json!({})).await.unwrap();
    let ended_at = db::iso_in(-600);
    for (name, pane) in [("alfa", &orphan), ("bravo", &reused)] {
        let bot = a_bot(&env, name).await;
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, herdr_session, started_at, ended_at)
                 VALUES (?,?,'exited','unknown',?,?,?,'test',?,?)",
        )
        .bind(db::ulid())
        .bind(&bot)
        .bind(&ws.workspace_id)
        .bind(&pane.tab_id)
        .bind(&pane.pane_id)
        .bind(db::iso_in(-1200))
        .bind(&ended_at)
        .execute(&app.db)
        .await
        .unwrap();
    }
    // 只有「重用 id」那顆在 `panes` 裡，而且是那個 run 結束**之後**才第一次被看到的。
    let seen = db::iso_in(-60);
    sqlx::query(
        "INSERT INTO panes (pane_id, host, workspace_id, tab_id, cwd, kind, last_output_at, first_seen, last_seen, owned_by)
             VALUES (?,?,?,?,'/tmp/p','shell',?,?,?,'user')",
    )
    .bind(&reused.pane_id)
    .bind(crate::config::LOCAL_HOST)
    .bind(&ws.workspace_id)
    .bind(&reused.tab_id)
    .bind(&seen)
    .bind(&seen)
    .bind(&seen)
    .execute(&app.db)
    .await
    .unwrap();
    env.herdr.agents.lock().unwrap().clear();

    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    assert!(
        client.pane_get(&reused.pane_id).await.unwrap().is_some(),
        "first_seen 晚於那個 run 的 ended_at＝這是重用同一個 id 的新 pane，不能當孤兒關掉"
    );
    assert!(
        client.pane_get(&orphan.pane_id).await.unwrap().is_none(),
        "沒有 panes 列的才是這一輪剛留下的孤兒，照舊關掉"
    );
}

/// **#60.** `pane_closed` ends the run before reconcile; children stayed forever (19 on 2026-09-11).
#[tokio::test]
async fn a_child_whose_pane_closed_before_the_reconcile_is_retired() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let pane = client.tab_create(&ws.workspace_id, "/tmp/p", "kid", json!({})).await.unwrap();
    let parent = a_bot(&env, "alfa").await;
    let kid_agent = format!("{}-kid", crate::config::agent_name("proj", &parent));
    let (kid, run) = a_child(&env, &parent, "kid", &kid_agent, &ws.workspace_id, &pane.tab_id, &pane.pane_id).await;

    // `pane_closed` first…
    client.pane_close(&pane.pane_id).await.unwrap();
    env.herdr.agents.lock().unwrap().clear();
    crate::lifecycle::mark_run_exited(&app, &run, "pane exited").await;
    // …then the reconcile.
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    let b = db::bot(&app.db, &kid).await.unwrap().unwrap();
    assert!(b.deleted_at.is_some(), "a child whose pane was closed must leave the sidebar");
}

/// #554：這顆 child 的退役紀錄（`intents` 的 `retire_child`）：`(status, payload)`。
async fn retire_record(app: &Arc<App>, bot: &str) -> Option<(String, serde_json::Value)> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT status, payload_json FROM intents WHERE kind = 'retire_child' AND subject_id = ?")
            .bind(bot)
            .fetch_optional(&app.db)
            .await
            .unwrap();
    row.map(|(status, payload)| (status, serde_json::from_str(&payload).unwrap()))
}

/// **#554，父 bot 用 `herdr pane close` 收 child 的那條路**：herdr 發了 pane 關閉事件（run 以 `pane exited` 結束）、
/// 對帳時 herdr 也說 pane 不在 → 退役紀錄的 `cause` 是 `pane_closed`。換版腳本只認這種（與 promote）是刻意收掉的。
#[tokio::test]
async fn a_child_whose_pane_was_closed_leaves_a_pane_closed_retire_record() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let pane = client.tab_create(&ws.workspace_id, "/tmp/p", "kid", json!({})).await.unwrap();
    let parent = a_bot(&env, "alfa").await;
    let kid_agent = format!("{}-kid", crate::config::agent_name("proj", &parent));
    let (kid, run) = a_child(&env, &parent, "kid", &kid_agent, &ws.workspace_id, &pane.tab_id, &pane.pane_id).await;
    client.pane_close(&pane.pane_id).await.unwrap();
    env.herdr.agents.lock().unwrap().clear();
    crate::lifecycle::mark_run_exited(&app, &run, "pane exited").await;

    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    assert!(retired(&app, &kid).await);
    let (status, p) = retire_record(&app, &kid).await.expect("退役要留一筆 DB 查得到的紀錄");
    assert_eq!(status, "done");
    assert_eq!(p["why"], "reconcile_run_already_ended");
    assert_eq!(p["cause"], "pane_closed", "{p}");
    assert_eq!(p["pane"], "gone", "{p}");
    assert_eq!(p["last_run"]["id"], run.as_str());
    assert_eq!(p["last_run"]["exit_reason"], "pane exited", "{p}");
    assert_eq!(p["parent_bot_id"], parent.as_str());
    assert_eq!(p["requested_by"], "-", "背景對帳沒有 HTTP 呼叫端");
    assert_eq!(p["boot_id"], app.boot_id.as_str(), "是哪一顆 daemon 退役的");
}

/// **#554，換版會弄丟 child 的那條路**：daemon 重啟、herdr 沒動，pane 還在，只是 daemon 認不出裡面的 agent
/// （herdr 不列它、pane 上也問不到）。這種不能被當成刻意收掉：`cause` 是 `agent_missing`。
#[tokio::test]
async fn a_child_whose_agent_vanished_from_a_live_pane_is_recorded_as_agent_missing() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let pane = client.tab_create(&ws.workspace_id, "/tmp/p", "kid", json!({})).await.unwrap();
    let parent = a_bot(&env, "alfa").await;
    let kid_agent = format!("{}-kid", crate::config::agent_name("proj", &parent));
    let (kid, _run) = a_child(&env, &parent, "kid", &kid_agent, &ws.workspace_id, &pane.tab_id, &pane.pane_id).await;
    env.herdr.agents.lock().unwrap().clear();

    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    assert!(retired(&app, &kid).await, "前提：照 #60 退役");
    let (_, p) = retire_record(&app, &kid).await.expect("退役要留紀錄");
    assert_eq!(p["why"], "reconcile_agent_gone");
    assert_eq!(p["pane"], "present", "{p}");
    assert_eq!(p["cause"], "agent_missing", "{p}");
}

/// **#554**：pane 真的不在了、但 daemon 沒有親眼看到它被關（沒有 pane 關閉事件，對帳時才發現）——例如換版那幾秒裡
/// 被關掉，或新的 daemon 連到一個空的 herdr。「pane 不在」本身證明不了是誰關的：`cause` 是 `unconfirmed`，不算刻意。
#[tokio::test]
async fn a_child_whose_pane_vanished_unobserved_is_unconfirmed() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let pane = client.tab_create(&ws.workspace_id, "/tmp/p", "kid", json!({})).await.unwrap();
    let parent = a_bot(&env, "alfa").await;
    let kid_agent = format!("{}-kid", crate::config::agent_name("proj", &parent));
    let (kid, _run) = a_child(&env, &parent, "kid", &kid_agent, &ws.workspace_id, &pane.tab_id, &pane.pane_id).await;
    client.pane_close(&pane.pane_id).await.unwrap();
    env.herdr.agents.lock().unwrap().clear();

    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    assert!(retired(&app, &kid).await);
    let (_, p) = retire_record(&app, &kid).await.expect("退役要留紀錄");
    assert_eq!(p["why"], "reconcile_agent_gone");
    assert_eq!(p["pane"], "gone", "{p}");
    assert_eq!(p["last_run"]["exit_reason"], "agent not found during reconcile", "{p}");
    assert_eq!(p["cause"], "unconfirmed", "{p}");
}

#[tokio::test]
async fn a_parent_replacing_a_missing_child_is_recorded_as_deliberate() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let parent_pane = client.tab_create(&ws.workspace_id, "/tmp/p", "parent", json!({})).await.unwrap();
    let old_pane = client.tab_create(&ws.workspace_id, "/tmp/p", "old-child", json!({})).await.unwrap();
    let successor_pane = client.tab_create(&ws.workspace_id, "/tmp/p", "successor", json!({})).await.unwrap();
    let parent = a_bot(&env, "alfa").await;
    let parent_agent = crate::config::agent_name("proj", &parent);
    let parent_run = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','working',?,?,?,?,'test',?)",
    )
    .bind(&parent_run)
    .bind(&parent)
    .bind(&ws.workspace_id)
    .bind(&parent_pane.tab_id)
    .bind(&parent_pane.pane_id)
    .bind(&parent_agent)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let old_agent = format!("{parent_agent}-codex");
    let (old_child, _) = a_child(&env, &parent, "old-codex", &old_agent, &ws.workspace_id, &old_pane.tab_id, &old_pane.pane_id).await;
    let successor_agent = format!("{parent_agent}-opus");
    crate::spawn_hints::record(&app, &parent, &successor_pane.pane_id).await.unwrap();
    client.pane_close(&old_pane.pane_id).await.unwrap();
    *env.herdr.agents.lock().unwrap() = vec![
        json!({
            "name": parent_agent, "agent": "claude", "agent_status": "working",
            "workspace_id": ws.workspace_id, "tab_id": parent_pane.tab_id, "pane_id": parent_pane.pane_id,
            "cwd": "/tmp/p"
        }),
        json!({
            "name": successor_agent, "agent": "claude", "agent_status": "working",
            "workspace_id": ws.workspace_id, "tab_id": successor_pane.tab_id, "pane_id": successor_pane.pane_id,
            "cwd": "/tmp/p"
        }),
    ];

    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    assert!(retired(&app, &old_child).await);
    let (status, p) = retire_record(&app, &old_child).await.expect("replacement retirement is recorded");
    assert_eq!(status, "done");
    assert_eq!(p["why"], "parent_replaced_child", "{p}");
    assert_eq!(p["cause"], "parent_replaced_child", "{p}");
    assert_eq!(p["parent_bot_id"], parent.as_str(), "{p}");
    let adopted: Option<(String, String)> = sqlx::query_as(
        "SELECT name, parent_bot_id FROM bots WHERE managed_by='child' AND deleted_at IS NULL AND name='opus'",
    )
    .fetch_optional(&app.db)
    .await
    .unwrap();
    assert_eq!(adopted, Some(("opus".into(), parent)), "the successor is adopted under the still-live parent");
}

/// 計畫中的 herdr 重啟（§6.5.2）：維護中兩條退休路徑都不刪子 agent；run 照樣結束。
/// 反向：同樣的情境不在維護中就照舊退休（上面那條測試），逾時的窗口也不算維護中。
#[tokio::test]
async fn children_are_kept_while_herdr_maintenance_is_open() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let parent = a_bot(&env, "alfa").await;
    let open_until = |until: &str| {
        sqlx::query("INSERT OR REPLACE INTO herdr_maintenance (id, opened_at, until, opened_by, reason) VALUES (1,?,?,'patrol','test')")
            .bind(db::now())
            .bind(until.to_string())
    };
    open_until("2999-01-01T00:00:00.000Z").execute(&app.db).await.unwrap();

    // 路徑 1：run 還開著，agent 不見了。
    let p1 = client.tab_create(&ws.workspace_id, "/tmp/p", "k1", json!({})).await.unwrap();
    let a1 = format!("{}-k1", crate::config::agent_name("proj", &parent));
    let (k1, r1) = a_child(&env, &parent, "k1", &a1, &ws.workspace_id, &p1.tab_id, &p1.pane_id).await;
    // 路徑 2：pane_closed 先把 run 結束了。
    let p2 = client.tab_create(&ws.workspace_id, "/tmp/p", "k2", json!({})).await.unwrap();
    let a2 = format!("{}-k2", crate::config::agent_name("proj", &parent));
    let (k2, r2) = a_child(&env, &parent, "k2", &a2, &ws.workspace_id, &p2.tab_id, &p2.pane_id).await;
    client.pane_close(&p1.pane_id).await.unwrap();
    client.pane_close(&p2.pane_id).await.unwrap();
    env.herdr.agents.lock().unwrap().clear();
    crate::lifecycle::mark_run_exited(&app, &r2, "pane exited").await;
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    for (kid, run) in [(&k1, &r1), (&k2, &r2)] {
        assert!(db::bot(&app.db, kid).await.unwrap().unwrap().deleted_at.is_none(), "maintenance keeps the child");
        let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(run).fetch_one(&app.db).await.unwrap();
        assert!(!["starting", "running", "stopping"].contains(&state.as_str()), "the run still ends: {state}");
    }

    // 窗口過期：下一輪對帳就照原規則退休（過期當下由 herdr_maintenance 收尾）。
    open_until("2020-01-01T00:00:00.000Z").execute(&app.db).await.unwrap();
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
    for kid in [&k1, &k2] {
        assert!(db::bot(&app.db, kid).await.unwrap().unwrap().deleted_at.is_some(), "expired window: normal rule again");
    }
}

/// 讀不到／讀得到之間切換 `herdr_maintenance`（改表名＝SELECT 失敗，跟 SQLite busy／I/O 錯誤同一條路）。
async fn maintenance_readable(app: &Arc<App>, readable: bool) {
    let sql = if readable {
        "ALTER TABLE herdr_maintenance_unreadable RENAME TO herdr_maintenance"
    } else {
        "ALTER TABLE herdr_maintenance RENAME TO herdr_maintenance_unreadable"
    };
    sqlx::query(sql).execute(&app.db).await.unwrap();
}

async fn retired(app: &Arc<App>, bot: &str) -> bool {
    db::bot(&app.db, bot).await.unwrap().unwrap().deleted_at.is_some()
}

/// **#191.** 維護狀態讀不到不等於沒在維護：herdr 重啟那幾分鐘所有 pane 同時消失，這時把子 agent 軟刪，pane 回來也接不回。
/// 兩條退休路徑（run 還開著／run 已經結束）都要留著；DB 恢復後還在維護照樣留；確定窗口結束、pane 也沒回來，才照原規則
/// 退休——而且不必等下一個 herdr 事件，延後的那一輪自己會補跑。
#[tokio::test]
async fn an_unreadable_maintenance_state_never_retires_a_child() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let parent = a_bot(&env, "alfa").await;
    sqlx::query("INSERT INTO herdr_maintenance (id, opened_at, until, opened_by, reason) VALUES (1,?,'2999-01-01T00:00:00.000Z','patrol','test')")
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
    // 路徑 1：run 還開著，agent 不見了。路徑 2：pane_closed 先把 run 結束了。
    let p1 = client.tab_create(&ws.workspace_id, "/tmp/p", "k1", json!({})).await.unwrap();
    let a1 = format!("{}-k1", crate::config::agent_name("proj", &parent));
    let (k1, r1) = a_child(&env, &parent, "k1", &a1, &ws.workspace_id, &p1.tab_id, &p1.pane_id).await;
    let p2 = client.tab_create(&ws.workspace_id, "/tmp/p", "k2", json!({})).await.unwrap();
    let a2 = format!("{}-k2", crate::config::agent_name("proj", &parent));
    let (k2, r2) = a_child(&env, &parent, "k2", &a2, &ws.workspace_id, &p2.tab_id, &p2.pane_id).await;
    client.pane_close(&p1.pane_id).await.unwrap();
    client.pane_close(&p2.pane_id).await.unwrap();
    env.herdr.agents.lock().unwrap().clear();
    crate::lifecycle::mark_run_exited(&app, &r2, "pane exited").await;

    maintenance_readable(&app, false).await;
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
    for kid in [&k1, &k2] {
        assert!(!retired(&app, kid).await, "讀不到維護狀態：這一輪留著");
    }
    let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(&r1).fetch_one(&app.db).await.unwrap();
    assert_eq!(state, "exited", "run 照樣結束，留著的是子 bot");

    // DB 恢復，窗口還開著：照樣留。
    maintenance_readable(&app, true).await;
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
    for kid in [&k1, &k2] {
        assert!(!retired(&app, kid).await, "還在維護：留著");
    }

    // 兩顆都沒有 active run 了，再讀不到一次：一樣不退休。
    maintenance_readable(&app, false).await;
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
    for kid in [&k1, &k2] {
        assert!(!retired(&app, kid).await, "沒有 active run 也不因為讀不到而退休");
    }

    // 窗口確定結束、pane 也沒回來：延後的那一輪自己補跑，照原規則退休（這之後沒有任何人叫 reconcile）。
    sqlx::query("DELETE FROM herdr_maintenance_unreadable").execute(&app.db).await.unwrap();
    maintenance_readable(&app, true).await;
    let _ = crate::testing::eventually!(retired(&app, &k1).await && retired(&app, &k2).await);
    for kid in [&k1, &k2] {
        assert!(retired(&app, kid).await, "確定沒在維護：延後的那一輪照原規則退休");
    }
}

/// AGM 的 child 掛在哪一顆 parent 底下（`supervisors.bot_id`）。專案留空，免得同一個測試專案裡的一般 child 也被算進去。
async fn an_agm_parent(env: &tt::Env, name: &str) -> String {
    let id = a_bot(env, name).await;
    sqlx::query("INSERT INTO supervisors (id, bot_id, project_id, created_at, updated_at) VALUES ('AGM', ?, '', 't', 't')")
        .bind(&id)
        .execute(&env.app.db)
        .await
        .unwrap();
    id
}

async fn retire_refusals(app: &Arc<App>) -> Vec<(Option<String>, String)> {
    sqlx::query_as("SELECT bot_id, payload_json FROM supervisor_inbox WHERE kind = 'child_retire_refused'")
        .fetch_all(&app.db)
        .await
        .unwrap()
}

/// **#413，第一條路**（`(Some(run), None)`：run 還開著、herdr 不列這個 agent 了）。
/// #406 只擋了刪除 API；隱式退役直接 `UPDATE bots SET deleted_at`，AGM 的 build／triage 開的 child
/// 就這樣消失而且沒有任何紀錄。現在擋下來、推一則給巡檢；一般專案的 child 照舊退役。
#[tokio::test]
async fn an_agm_child_is_not_retired_when_its_agent_is_gone() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let agm = an_agm_parent(&env, "agm-build").await;
    let user = a_bot(&env, "alfa").await;
    let pa = client.tab_create(&ws.workspace_id, "/tmp/p", "k-agm", json!({})).await.unwrap();
    let pu = client.tab_create(&ws.workspace_id, "/tmp/p", "k-user", json!({})).await.unwrap();
    let (agm_kid, agm_run) =
        a_child(&env, &agm, "k-agm", &format!("{}-k", crate::config::agent_name("proj", &agm)), &ws.workspace_id, &pa.tab_id, &pa.pane_id).await;
    let (user_kid, _) =
        a_child(&env, &user, "k-user", &format!("{}-k", crate::config::agent_name("proj", &user)), &ws.workspace_id, &pu.tab_id, &pu.pane_id).await;
    // run 留著開著，只讓 herdr 不再列這兩個 agent。
    env.herdr.agents.lock().unwrap().clear();
    client.pane_close(&pa.pane_id).await.unwrap();
    client.pane_close(&pu.pane_id).await.unwrap();

    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    assert!(!retired(&app, &agm_kid).await, "AGM 的 child 沒有被隱式軟刪");
    assert!(retired(&app, &user_kid).await, "一般專案的 child 照舊退役");
    assert!(retire_record(&app, &agm_kid).await.is_none(), "擋下來的不留退役紀錄（#554）");
    assert!(retire_record(&app, &user_kid).await.is_some(), "退役的才有（#554）");
    let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(&agm_run).fetch_one(&app.db).await.unwrap();
    assert_eq!(state, "exited", "擋的只有軟刪：run 照樣收掉");
    let refusals = retire_refusals(&app).await;
    assert_eq!(refusals.len(), 1, "只有 AGM 那顆推了通知：{refusals:?}");
    assert_eq!(refusals[0].0.as_deref(), Some(agm_kid.as_str()));
    assert!(refusals[0].1.contains("reconcile_agent_gone"), "帶原因：{}", refusals[0].1);
    assert!(refusals[0].1.contains("AGM 開出去的子 agent"), "帶角色：{}", refusals[0].1);
    // 同一顆、同一輪不會越推越多：第二次沿用上一則那把鑰匙（#536；以前是牆上時鐘的小時格，兩次對帳
    // 跨過整點就變兩則，這條斷言因此會間歇性紅）。
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
    assert!(!retired(&app, &agm_kid).await);
    assert_eq!(retire_refusals(&app).await.len(), 1, "一小時只推一次，跨整點也一樣");
}

/// **#413，第二條路**（`(None, None)`：`pane_closed` 早就把 run 收掉了，herdr 也不列它）。
#[tokio::test]
async fn an_agm_child_is_not_retired_when_its_run_already_ended() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let agm = an_agm_parent(&env, "agm-build").await;
    let user = a_bot(&env, "alfa").await;
    let pa = client.tab_create(&ws.workspace_id, "/tmp/p", "k-agm", json!({})).await.unwrap();
    let pu = client.tab_create(&ws.workspace_id, "/tmp/p", "k-user", json!({})).await.unwrap();
    let (agm_kid, agm_run) =
        a_child(&env, &agm, "k-agm", &format!("{}-k", crate::config::agent_name("proj", &agm)), &ws.workspace_id, &pa.tab_id, &pa.pane_id).await;
    let (user_kid, user_run) =
        a_child(&env, &user, "k-user", &format!("{}-k", crate::config::agent_name("proj", &user)), &ws.workspace_id, &pu.tab_id, &pu.pane_id).await;
    client.pane_close(&pa.pane_id).await.unwrap();
    client.pane_close(&pu.pane_id).await.unwrap();
    env.herdr.agents.lock().unwrap().clear();
    crate::lifecycle::mark_run_exited(&app, &agm_run, "pane exited").await;
    crate::lifecycle::mark_run_exited(&app, &user_run, "pane exited").await;

    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    assert!(!retired(&app, &agm_kid).await, "AGM 的 child 沒有被隱式軟刪");
    assert!(retired(&app, &user_kid).await, "一般專案的 child 照舊退役");
    assert!(retire_record(&app, &agm_kid).await.is_none(), "擋下來的不留退役紀錄（#554）");
    assert!(retire_record(&app, &user_kid).await.is_some(), "退役的才有（#554）");
    let refusals = retire_refusals(&app).await;
    assert_eq!(refusals.len(), 1, "{refusals:?}");
    assert_eq!(refusals[0].0.as_deref(), Some(agm_kid.as_str()));
    assert!(refusals[0].1.contains("reconcile_run_already_ended"), "帶原因：{}", refusals[0].1);
}

/// #191 同一條：run 的結束寫不進去時，DB 裡它還在跑——這時把子 bot 軟刪，就是一顆刪掉的 bot 掛著活的 run。
#[tokio::test]
async fn a_child_whose_run_exit_was_not_recorded_is_not_retired() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let parent = a_bot(&env, "alfa").await;
    let pane = client.tab_create(&ws.workspace_id, "/tmp/p", "kid", json!({})).await.unwrap();
    let agent = format!("{}-kid", crate::config::agent_name("proj", &parent));
    let (kid, run) = a_child(&env, &parent, "kid", &agent, &ws.workspace_id, &pane.tab_id, &pane.pane_id).await;
    client.pane_close(&pane.pane_id).await.unwrap();
    env.herdr.agents.lock().unwrap().clear();
    sqlx::query(
        "CREATE TRIGGER exit_unwritable BEFORE UPDATE OF state ON runs WHEN NEW.state = 'exited'
             BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END",
    )
    .execute(&app.db)
    .await
    .unwrap();

    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
    let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(&run).fetch_one(&app.db).await.unwrap();
    assert_eq!(state, "running", "前提：結束沒寫進去");
    assert!(!retired(&app, &kid).await, "run 還活著就不退休");

    // 寫得進去之後，延後的那一輪自己把它收掉並退休。
    sqlx::query("DROP TRIGGER exit_unwritable").execute(&app.db).await.unwrap();
    let _ = crate::testing::eventually!(retired(&app, &kid).await);
    assert!(retired(&app, &kid).await, "結束寫進去之後照原規則退休");
}

/// An ended run is not enough: a still-listed child agent gets its run back.
#[tokio::test]
async fn a_child_whose_run_ended_but_whose_agent_is_still_listed_is_kept() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let pane = client.tab_create(&ws.workspace_id, "/tmp/p", "kid", json!({})).await.unwrap();
    let parent = a_bot(&env, "alfa").await;
    let kid_agent = format!("{}-kid", crate::config::agent_name("proj", &parent));
    let (kid, run) = a_child(&env, &parent, "kid", &kid_agent, &ws.workspace_id, &pane.tab_id, &pane.pane_id).await;
    *env.herdr.agents.lock().unwrap() = vec![json!({
        "name": kid_agent, "agent": "claude", "agent_status": "idle",
        "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"})];
    crate::lifecycle::mark_run_exited(&app, &run, "pane exited").await;

    bind_listed_instances(&env).await;
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    let b = db::bot(&app.db, &kid).await.unwrap().unwrap();
    assert!(b.deleted_at.is_none(), "the agent is still there, so the child is not retired");
    let r = run_of(&app, &kid).await.unwrap();
    assert_eq!(r.state, "running", "and it gets a run again");
}

#[tokio::test]
async fn a_spawned_child_is_adopted_with_the_model_its_argv_names() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let kid_pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();

    let parent = a_bot(&env, "alfa").await;
    let parent_agent = crate::config::agent_name("proj", &parent);
    let kid_agent = format!("{parent_agent}-lastq");
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,'test',?)",
    )
    .bind(db::ulid())
    .bind(&parent)
    .bind(&ws.workspace_id)
    .bind(&root.pane_id)
    .bind(&parent_agent)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    *env.herdr.agents.lock().unwrap() = vec![
        json!({"name": parent_agent, "agent": "claude", "agent_status": "idle",
               "workspace_id": ws.workspace_id, "tab_id": root.tab_id, "pane_id": root.pane_id, "cwd": "/tmp/p"}),
        json!({"name": kid_agent, "agent": "claude", "agent_status": "idle",
               "workspace_id": ws.workspace_id, "tab_id": kid_pane.tab_id, "pane_id": kid_pane.pane_id, "cwd": "/tmp/p"}),
    ];
    // The parent's `haiku` must be ignored: a user bot's model is configuration.
    env.herdr.set_argv(&root.pane_id, &["claude", "--dangerously-skip-permissions", "--model", "haiku"]);
    env.herdr.set_argv(&kid_pane.pane_id, &["claude", "--dangerously-skip-permissions", "--model", "opus", "--effort", "high"]);

    crate::spawn_hints::claim(&app, &parent, &kid_pane.pane_id).await.unwrap();
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    let kid = sqlx::query_as::<_, db::Bot>("SELECT * FROM bots WHERE parent_bot_id = ?")
        .bind(&parent)
        .fetch_one(&app.db)
        .await
        .expect("the child was adopted");
    assert_eq!(kid.name, "lastq");
    assert_eq!(kid.model.as_deref(), Some("claude-opus-5-5"), "read off `--model` after exact retired-alias mapping");
    assert_eq!(kid.effort.as_deref(), Some("high"), "read off `--effort`");
    assert_eq!(kid.inject_hooks, 0, "still no hooks: 對話 comes from the terminal");

    let p = db::bot(&app.db, &parent).await.unwrap().unwrap();
    assert_eq!(p.model, None, "a user bot's model is configuration, never scraped from its process");

    // `/model` leaves argv untouched, so argv must not win a rematch.
    sqlx::query("UPDATE bots SET model='sonnet' WHERE id=?").bind(&kid.id).execute(&app.db).await.unwrap();
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
    assert_eq!(db::bot(&app.db, &kid.id).await.unwrap().unwrap().model.as_deref(), Some("sonnet"));
}

/// 2026-10-04 a1：agy 子 agent 的 `bots.model` 從 argv 補得到，但 `runs.runtime_model` 一直是 null，網頁畫成「CLI 預設」。
/// `--model` 就是它啟動時實際用的模型：run 也要記（只補空的，不覆蓋之後別的來源寫的）。
#[tokio::test]
async fn an_agy_child_run_records_the_model_its_argv_names_as_its_runtime_model() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let kid_pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
    let parent = a_bot(&env, "alfa").await;
    let parent_agent = crate::config::agent_name("proj", &parent);
    let kid_agent = format!("{parent_agent}-a1");
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,'test',?)",
    )
    .bind(db::ulid())
    .bind(&parent)
    .bind(&ws.workspace_id)
    .bind(&root.pane_id)
    .bind(&parent_agent)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    *env.herdr.agents.lock().unwrap() = vec![
        json!({"name": parent_agent, "agent": "claude", "agent_status": "idle", "workspace_id": ws.workspace_id, "tab_id": root.tab_id, "pane_id": root.pane_id, "cwd": "/tmp/p"}),
        json!({"name": kid_agent, "agent": "agy", "agent_status": "idle", "workspace_id": ws.workspace_id, "tab_id": kid_pane.tab_id, "pane_id": kid_pane.pane_id, "cwd": "/tmp/p"}),
    ];
    env.herdr.set_argv(&root.pane_id, &["claude", "--model", "haiku"]);
    env.herdr.set_argv(&kid_pane.pane_id, &["agy", "--dangerously-skip-permissions", "--model", "gemini-3.8-flash-medium"]);

    crate::spawn_hints::claim(&app, &parent, &kid_pane.pane_id).await.unwrap();
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    let kid = sqlx::query_as::<_, db::Bot>("SELECT * FROM bots WHERE parent_bot_id = ?").bind(&parent).fetch_one(&app.db).await.expect("adopted");
    assert_eq!((kid.kind.as_str(), kid.model.as_deref()), ("agy", Some("gemini-3.8-flash-medium")));
    let run = run_of(&app, &kid.id).await.unwrap();
    assert_eq!(run.runtime_model.as_deref(), Some("gemini-3.8-flash-medium"), "run 也記 argv 的模型");
    // 之後別的來源寫過的不蓋。
    sqlx::query("UPDATE runs SET runtime_model = 'gemini-3.1-pro-high' WHERE id = ?").bind(&run.id).execute(&app.db).await.unwrap();
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
    assert_eq!(run_of(&app, &kid.id).await.unwrap().runtime_model.as_deref(), Some("gemini-3.1-pro-high"));
}

/// 2026-09-14 使用者指正：codex 子 agent 從 claude 母 bot 抄了 `cc1`，quota 就長出 `codex:cc1`。
/// 身份有 kind，只有同 kind 的子 agent 才繼承（`identity_kind::child_identity`）。
#[tokio::test]
async fn a_codex_child_of_a_claude_parent_does_not_inherit_its_identity() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let codex_pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
    let claude_pane = client.pane_split(&root.pane_id, "down", "/tmp/p", json!({})).await.unwrap();

    let parent = a_bot(&env, "alfa").await;
    sqlx::query("UPDATE bots SET identity = 'cc1' WHERE id = ?").bind(&parent).execute(&app.db).await.unwrap();
    let parent_agent = crate::config::agent_name("proj", &parent);
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,'test',?)",
    )
    .bind(db::ulid())
    .bind(&parent)
    .bind(&ws.workspace_id)
    .bind(&root.pane_id)
    .bind(&parent_agent)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    *env.herdr.agents.lock().unwrap() = vec![
        json!({"name": parent_agent, "agent": "claude", "agent_status": "idle",
               "workspace_id": ws.workspace_id, "tab_id": root.tab_id, "pane_id": root.pane_id, "cwd": "/tmp/p"}),
        json!({"name": format!("{parent_agent}-rtsp"), "agent": "codex", "agent_status": "idle",
               "workspace_id": ws.workspace_id, "tab_id": codex_pane.tab_id, "pane_id": codex_pane.pane_id, "cwd": "/tmp/p"}),
        json!({"name": format!("{parent_agent}-review"), "agent": "claude", "agent_status": "idle",
               "workspace_id": ws.workspace_id, "tab_id": claude_pane.tab_id, "pane_id": claude_pane.pane_id, "cwd": "/tmp/p"}),
    ];

    crate::spawn_hints::claim(&app, &parent, &codex_pane.pane_id).await.unwrap();
    crate::spawn_hints::claim(&app, &parent, &claude_pane.pane_id).await.unwrap();
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    let kid = |name: &str| {
        let app = app.clone();
        let (parent, name) = (parent.clone(), name.to_string());
        async move {
            sqlx::query_as::<_, db::Bot>("SELECT * FROM bots WHERE parent_bot_id = ? AND name = ?")
                .bind(&parent)
                .bind(&name)
                .fetch_one(&app.db)
                .await
                .unwrap_or_else(|_| panic!("child {name} adopted"))
        }
    };
    let codex = kid("rtsp").await;
    assert_eq!(codex.kind, "codex");
    assert_eq!(codex.identity, None, "claude 的 cc1 不能抄給 codex 子 agent");
    let claude = kid("review").await;
    assert_eq!(claude.identity.as_deref(), Some("cc1"), "同 kind 的子 agent 照舊繼承");
}

/// Stand-in for `ps eww -p <pid>` that records every pid asked (one ssh per ask on remote).
struct FakeProcEnv {
    envs: std::collections::BTreeMap<i64, std::collections::BTreeMap<String, String>>,
    asked: std::sync::Mutex<Vec<i64>>,
}

impl crate::runners::pane_identity::ProcEnv for FakeProcEnv {
    fn env_of<'a>(
        &'a self,
        _app: &'a Arc<App>,
        _host: &'a str,
        pid: i64,
    ) -> futures::future::BoxFuture<'a, Option<std::collections::BTreeMap<String, String>>> {
        Box::pin(async move {
            self.asked.lock().unwrap().push(pid);
            self.envs.get(&pid).cloned()
        })
    }
}

fn claude_env(dir: &str) -> std::collections::BTreeMap<String, String> {
    std::collections::BTreeMap::from([("CLAUDE_CONFIG_DIR".to_string(), dir.to_string())])
}

fn claude_identity(name: &str, dir: &str) -> crate::config::IdentityCfg {
    crate::config::IdentityCfg {
        name: name.into(),
        kind: "claude".into(),
        host: None,
        env: claude_env(dir),
        args: vec![],
    }
}

fn codex_identity(name: &str, dir: &str) -> crate::config::IdentityCfg {
    crate::config::IdentityCfg {
        name: name.into(),
        kind: "codex".into(),
        host: None,
        env: std::collections::BTreeMap::from([("CODEX_HOME".into(), dir.into())]),
        args: vec![],
    }
}

async fn configure_cross_kind_identities(app: &Arc<App>) {
    app.cfg
        .update(|c| {
            c.identities = vec![
                claude_identity("cc1", "/tmp/.claude-cc1"),
                codex_identity("cx1", "/tmp/.codex-cx1"),
            ];
            Ok(())
        })
        .await
        .unwrap();
}

fn codex_argv() -> [&'static str; 7] {
    [
        "codex",
        "-m",
        "gpt-5.6-sol",
        "-c",
        "model_reasoning_effort=\"max\"",
        "-c",
        "service_tier=\"priority\"",
    ]
}

/// Herdr can list the pane before it has filled `agent`.  The process is still enough to
/// classify the child on the first pass, and its own CODEX_HOME must win over the Claude
/// parent's inherited identity.
#[tokio::test]
async fn a_codex_child_is_classified_from_argv_before_herdr_reports_its_kind() {
    let env = tt::env().await;
    let app = env.app.clone();
    configure_cross_kind_identities(&app).await;
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let child_pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
    let parent = a_bot(&env, "alfa").await;
    sqlx::query("UPDATE bots SET identity = 'cc1' WHERE id = ?").bind(&parent).execute(&app.db).await.unwrap();
    let parent_agent = crate::config::agent_name("proj", &parent);
    let child_agent = format!("{parent_agent}-codex");
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
    )
    .bind(db::ulid())
    .bind(&parent)
    .bind(&ws.workspace_id)
    .bind(&root.tab_id)
    .bind(&root.pane_id)
    .bind(&parent_agent)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    *env.herdr.agents.lock().unwrap() = vec![
        json!({"name": parent_agent, "agent": "claude", "agent_status": "idle",
               "workspace_id": ws.workspace_id, "tab_id": root.tab_id, "pane_id": root.pane_id, "cwd": "/tmp/p"}),
        json!({"name": child_agent, "agent": null, "agent_status": "working",
               "workspace_id": ws.workspace_id, "tab_id": child_pane.tab_id, "pane_id": child_pane.pane_id, "cwd": "/tmp/p"}),
    ];
    let argv = codex_argv();
    env.herdr.set_argv(&child_pane.pane_id, &argv);
    env.herdr.set_pid(&child_pane.pane_id, 7901);
    let fake = Arc::new(FakeProcEnv {
        envs: std::collections::BTreeMap::from([(
            7901,
            std::collections::BTreeMap::from([("CODEX_HOME".into(), "/tmp/.codex-cx1".into())]),
        )]),
        asked: std::sync::Mutex::new(Vec::new()),
    });
    app.proc_env.set(fake);

    crate::spawn_hints::claim(&app, &parent, &child_pane.pane_id).await.unwrap();
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    let child = sqlx::query_as::<_, db::Bot>("SELECT * FROM bots WHERE parent_bot_id = ?")
        .bind(&parent)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(child.kind, "codex");
    assert_eq!(child.identity.as_deref(), Some("cx1"));
    assert_eq!(child.model.as_deref(), Some("gpt-6.1-sol"));
    assert_eq!(child.effort.as_deref(), Some("max"));
    assert_eq!(child.fast, 1);
}

/// If both Herdr's kind and the process argv were unavailable at adoption time, the later
/// agent-list update still has to repair an already-claimed child and re-run CODEX_HOME.
#[tokio::test]
async fn a_late_herdr_kind_repairs_an_existing_child_and_its_identity() {
    let env = tt::env().await;
    let app = env.app.clone();
    configure_cross_kind_identities(&app).await;
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let child_pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
    let parent = a_bot(&env, "alfa").await;
    sqlx::query("UPDATE bots SET identity = 'cc1' WHERE id = ?").bind(&parent).execute(&app.db).await.unwrap();
    let parent_agent = crate::config::agent_name("proj", &parent);
    let child_agent = format!("{parent_agent}-codex");
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
    )
    .bind(db::ulid())
    .bind(&parent)
    .bind(&ws.workspace_id)
    .bind(&root.tab_id)
    .bind(&root.pane_id)
    .bind(&parent_agent)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    *env.herdr.agents.lock().unwrap() = vec![
        json!({"name": parent_agent, "agent": "claude", "agent_status": "idle",
               "workspace_id": ws.workspace_id, "tab_id": root.tab_id, "pane_id": root.pane_id, "cwd": "/tmp/p"}),
        json!({"name": child_agent, "agent": null, "agent_status": "working",
               "workspace_id": ws.workspace_id, "tab_id": child_pane.tab_id, "pane_id": child_pane.pane_id, "cwd": "/tmp/p"}),
    ];
    crate::spawn_hints::claim(&app, &parent, &child_pane.pane_id).await.unwrap();
    bind_listed_instances(&env).await;
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
    let first = sqlx::query_as::<_, db::Bot>("SELECT * FROM bots WHERE parent_bot_id = ?")
        .bind(&parent)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(first.kind, "claude", "前提：第一輪沒有 kind／argv，只能暫時退回 parent");
    assert_eq!(first.identity.as_deref(), Some("cc1"));
    // As if the first pass had read a Claude account off the pane: a kind change must drop it.
    sqlx::query("UPDATE runs SET runtime_identity = 'cc1' WHERE bot_id = ?").bind(&first.id).execute(&app.db).await.unwrap();

    let argv = codex_argv();
    env.herdr.set_argv(&child_pane.pane_id, &argv);
    env.herdr.set_pid(&child_pane.pane_id, 7902);
    env.herdr
        .agents
        .lock()
        .unwrap()
        .iter_mut()
        .find(|a| a.get("name").and_then(Value::as_str) == Some(child_agent.as_str()))
        .unwrap()["agent"] = json!("codex");
    let fake = Arc::new(FakeProcEnv {
        envs: std::collections::BTreeMap::from([(
            7902,
            std::collections::BTreeMap::from([("CODEX_HOME".into(), "/tmp/.codex-cx1".into())]),
        )]),
        asked: std::sync::Mutex::new(Vec::new()),
    });
    app.proc_env.set(fake);

    bind_listed_instances(&env).await;
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    let child = sqlx::query_as::<_, db::Bot>("SELECT * FROM bots WHERE parent_bot_id = ?")
        .bind(&parent)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(child.kind, "codex");
    assert_eq!(child.identity.as_deref(), Some("cx1"));
    assert_eq!(run_of(&app, &child.id).await.unwrap().runtime_identity.as_deref(), Some("cx1"), "the run follows the new kind's account");
    assert_eq!(child.model.as_deref(), Some("gpt-6.1-sol"));
    assert_eq!(child.effort.as_deref(), Some("max"));
    assert_eq!(child.fast, 1);
}

/// Changing a child's kind invalidates both its bot observations and its active run's
/// observations.  A failed second write must not leave the first write committed, or the
/// next reconcile will see the new kind and never retry the runtime reset.
#[tokio::test]
async fn a_kind_repair_does_not_leave_bot_and_run_observations_half_updated() {
    let env = tt::env().await;
    let app = env.app.clone();
    configure_cross_kind_identities(&app).await;
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let child_pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();

    let parent = a_bot(&env, "alfa").await;
    sqlx::query("UPDATE bots SET identity = 'cc1' WHERE id = ?").bind(&parent).execute(&app.db).await.unwrap();
    let parent_agent = crate::config::agent_name("proj", &parent);
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
    )
    .bind(db::ulid())
    .bind(&parent)
    .bind(&ws.workspace_id)
    .bind(&root.tab_id)
    .bind(&root.pane_id)
    .bind(&parent_agent)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();

    let child_agent = format!("{parent_agent}-codex");
    let (child, child_run) = a_child(
        &env,
        &parent,
        "codex",
        &child_agent,
        &ws.workspace_id,
        &child_pane.tab_id,
        &child_pane.pane_id,
    )
    .await;
    sqlx::query("UPDATE bots SET identity='cc1', model='old-model', effort='old-effort', fast=1 WHERE id=?")
        .bind(&child)
        .execute(&app.db)
        .await
        .unwrap();
    sqlx::query("UPDATE runs SET runtime_model='old-model', runtime_effort='old-effort', runtime_fast=1 WHERE id=?")
        .bind(&child_run)
        .execute(&app.db)
        .await
        .unwrap();
    sqlx::query(&format!(
        "CREATE TRIGGER refuse_runtime_reset BEFORE UPDATE OF runtime_model ON runs
             WHEN OLD.id = '{child_run}' AND NEW.runtime_model IS NULL
             BEGIN SELECT RAISE(ABORT, 'runtime reset unavailable'); END"
    ))
    .execute(&app.db)
    .await
    .unwrap();

    *env.herdr.agents.lock().unwrap() = vec![
        json!({"name": parent_agent, "agent": "claude", "agent_status": "idle",
               "workspace_id": ws.workspace_id, "tab_id": root.tab_id, "pane_id": root.pane_id, "cwd": "/tmp/p"}),
        json!({"name": child_agent, "agent": "codex", "agent_status": "working",
               "workspace_id": ws.workspace_id, "tab_id": child_pane.tab_id, "pane_id": child_pane.pane_id, "cwd": "/tmp/p"}),
    ];

    bind_listed_instances(&env).await;
    assert!(super::reconcile_host(&app, crate::config::LOCAL_HOST).await.is_err(), "runtime reset failure must be retryable");
    let bot = sqlx::query_as::<_, db::Bot>("SELECT * FROM bots WHERE id=?").bind(&child).fetch_one(&app.db).await.unwrap();
    assert_eq!(bot.kind, "claude", "failed runtime reset must roll back the kind change");
    assert_eq!(bot.identity.as_deref(), Some("cc1"));
    assert_eq!(bot.model.as_deref(), Some("old-model"));
    assert_eq!(bot.effort.as_deref(), Some("old-effort"));
    assert_eq!(bot.fast, 1);
    let run = run_of(&app, &child).await.unwrap();
    assert_eq!(run.runtime_model.as_deref(), Some("old-model"));
    assert_eq!(run.runtime_effort.as_deref(), Some("old-effort"));
    assert_eq!(run.runtime_fast, Some(1));
}

/// A child's **account** is not its parent's (SPEC §16.6): read it off its own pane's process.
#[tokio::test]
async fn a_spawned_childs_identity_is_the_account_its_own_pane_runs_on() {
    let env = tt::env().await;
    let app = env.app.clone();
    let home = dirs::home_dir().unwrap().to_string_lossy().to_string();
    // Two accounts, spelled the two ways an identity may spell one.
    app.cfg
        .update(|c| {
            c.identities =
                vec![claude_identity("cc1", "$HOME/.claude-ccompany"), claude_identity("cc2", "~/.claude-cc2")];
            Ok(())
        })
        .await
        .unwrap();

    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let head = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
    let lost = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
    let same = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();

    let parent = a_bot(&env, "alfa").await;
    sqlx::query("UPDATE bots SET identity = 'cc1' WHERE id = ?")
        .bind(&parent)
        .execute(&app.db)
        .await
        .unwrap();
    let parent_agent = crate::config::agent_name("proj", &parent);
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,'test',?)",
    )
    .bind(db::ulid())
    .bind(&parent)
    .bind(&ws.workspace_id)
    .bind(&root.pane_id)
    .bind(&parent_agent)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    *env.herdr.agents.lock().unwrap() = vec![
        json!({"name": parent_agent, "agent": "claude", "agent_status": "idle",
               "workspace_id": ws.workspace_id, "tab_id": root.tab_id, "pane_id": root.pane_id, "cwd": "/tmp/p"}),
        json!({"name": format!("{parent_agent}-head"), "agent": "claude", "agent_status": "idle",
               "workspace_id": ws.workspace_id, "tab_id": head.tab_id, "pane_id": head.pane_id, "cwd": "/tmp/p"}),
        json!({"name": format!("{parent_agent}-lost"), "agent": "claude", "agent_status": "idle",
               "workspace_id": ws.workspace_id, "tab_id": lost.tab_id, "pane_id": lost.pane_id, "cwd": "/tmp/p"}),
        json!({"name": format!("{parent_agent}-same"), "agent": "claude", "agent_status": "idle",
               "workspace_id": ws.workspace_id, "tab_id": same.tab_id, "pane_id": same.pane_id, "cwd": "/tmp/p"}),
    ];
    for pane in [&root.pane_id, &head.pane_id, &lost.pane_id, &same.pane_id] {
        env.herdr.set_argv(pane, &["claude", "--dangerously-skip-permissions", "--model", "opus"]);
    }
    env.herdr.set_pid(&head.pane_id, 4924);
    env.herdr.set_pid(&lost.pane_id, 4925);
    env.herdr.set_pid(&same.pane_id, 4926);
    // The parent's pane says `cc2` too, and must never be read: user config.
    let fake = Arc::new(FakeProcEnv {
        envs: std::collections::BTreeMap::from([
            (1, claude_env(&format!("{home}/.claude-cc2"))),
            (4924, claude_env(&format!("{home}/.claude-cc2/"))),
            (4925, claude_env("/tmp/an-account-nobody-configured")),
            (4926, claude_env(&format!("{home}/.claude-ccompany"))),
        ]),
        asked: std::sync::Mutex::new(Vec::new()),
    });
    app.proc_env.set(fake.clone());

    let listed = env.herdr.agents.lock().unwrap().clone();
    for agent in listed {
        let pane = agent["pane_id"].as_str().unwrap();
        if pane != root.pane_id { crate::spawn_hints::claim(&app, &parent, pane).await.unwrap(); }
    }
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    let kid = |name: &str| {
        let db = app.db.clone();
        let name = name.to_string();
        async move {
            sqlx::query_as::<_, db::Bot>("SELECT * FROM bots WHERE name = ?")
                .bind(&name)
                .fetch_one(&db)
                .await
                .expect("the child was adopted")
        }
    };
    let head_bot = kid("head").await;
    assert_eq!(head_bot.managed_by, "child");
    assert_eq!(head_bot.identity.as_deref(), Some("cc2"), "read off its own pane's CLAUDE_CONFIG_DIR");
    assert_eq!(head_bot.model.as_deref(), Some("claude-opus-5-5"), "the same process_info still fills the model through the shared mapping");

    // An unclaimed directory is no answer: the inherited value stays.
    assert_eq!(kid("lost").await.identity.as_deref(), Some("cc1"));

    // The run records what was actually read off the pane — also when it equals the inherited
    // value — and stays NULL (UI: 未知) when nothing was: the inherited `cc1` is only a guess.
    let run_identity = |name: &'static str| {
        let app = app.clone();
        let kid = kid(name);
        async move { run_of(&app, &kid.await.id).await.expect("adopted run").runtime_identity }
    };
    assert_eq!(run_identity("head").await.as_deref(), Some("cc2"));
    assert_eq!(kid("same").await.identity.as_deref(), Some("cc1"));
    assert_eq!(run_identity("same").await.as_deref(), Some("cc1"), "detected, not merely inherited");
    assert_eq!(run_identity("lost").await, None, "an unclaimed account is still unknown");

    let p = db::bot(&app.db, &parent).await.unwrap().unwrap();
    assert_eq!(p.identity.as_deref(), Some("cc1"), "a user bot's account is configuration, never scraped");

    // Once per child, never the parent: a per-pass `ps` is an ssh storm on remote hosts.
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
    assert_eq!(*fake.asked.lock().unwrap(), vec![4924, 4925, 4926]);
    assert_eq!(kid("head").await.identity.as_deref(), Some("cc2"));
}

/// An explicit claim adopts an unprefixed helper; its tab/name does not choose its owner.
#[tokio::test]
async fn an_explicitly_claimed_stranger_is_adopted_as_its_child() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let kid_pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
    assert_eq!(kid_pane.tab_id, root.tab_id, "precondition: one bot, one tab");

    let parent = a_bot(&env, "alfa").await;
    let parent_agent = crate::config::agent_name("proj", &parent);
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
    )
    .bind(db::ulid())
    .bind(&parent)
    .bind(&ws.workspace_id)
    .bind(&root.pane_id)
    .bind(&root.tab_id)
    .bind(&parent_agent)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    *env.herdr.agents.lock().unwrap() = vec![
        json!({"name": parent_agent, "agent": "claude", "agent_status": "idle",
               "workspace_id": ws.workspace_id, "tab_id": root.tab_id, "pane_id": root.pane_id, "cwd": "/tmp/p"}),
        json!({"name": "helper", "agent": "codex", "agent_status": "working",
               "workspace_id": ws.workspace_id, "tab_id": kid_pane.tab_id, "pane_id": kid_pane.pane_id, "cwd": "/tmp/p"}),
    ];

    crate::spawn_hints::claim(&app, &parent, &kid_pane.pane_id).await.unwrap();
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    let kid = sqlx::query_as::<_, db::Bot>("SELECT * FROM bots WHERE parent_bot_id = ?")
        .bind(&parent)
        .fetch_one(&app.db)
        .await
        .expect("the stranger in the tab was adopted as a child");
    assert_eq!(kid.name, "helper", "no prefix to strip: herdr's own agent name");
    assert_eq!(kid.kind, "codex", "a child need not be the parent's CLI");
    assert_eq!(kid.managed_by, "child");
    let r = run_of(&app, &kid.id).await.unwrap();
    assert_eq!(r.adopted, 1);
    assert_eq!(r.pane_id.as_deref(), Some(kid_pane.pane_id.as_str()));

    // Idempotent: a second pass reuses the bot rather than making a twin.
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bots WHERE parent_bot_id = ? AND deleted_at IS NULL")
        .bind(&parent)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn a_grandchild_in_the_same_tab_lands_under_the_child() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let kid_pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
    let grand_pane = client.pane_split(&kid_pane.pane_id, "down", "/tmp/p", json!({})).await.unwrap();

    let parent = a_bot(&env, "alfa").await;
    let parent_agent = crate::config::agent_name("proj", &parent);
    let kid_agent = format!("{parent_agent}-lastq");
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
    )
    .bind(db::ulid())
    .bind(&parent)
    .bind(&ws.workspace_id)
    .bind(&root.pane_id)
    .bind(&root.tab_id)
    .bind(&parent_agent)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let agent_json = |name: &str, pane: &str, tab: &str| {
        json!({"name": name, "agent": "claude", "agent_status": "idle",
               "workspace_id": ws.workspace_id, "tab_id": tab, "pane_id": pane, "cwd": "/tmp/p"})
    };
    *env.herdr.agents.lock().unwrap() = vec![
        agent_json(&parent_agent, &root.pane_id, &root.tab_id),
        agent_json(&kid_agent, &kid_pane.pane_id, &kid_pane.tab_id),
    ];
    crate::spawn_hints::claim(&app, &parent, &kid_pane.pane_id).await.unwrap();
    bind_listed_instances(&env).await;
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
    let kid = sqlx::query_as::<_, db::Bot>("SELECT * FROM bots WHERE parent_bot_id = ?")
        .bind(&parent)
        .fetch_one(&app.db)
        .await
        .unwrap();

    env.herdr.agents.lock().unwrap().push(agent_json(
        &format!("{kid_agent}-deep"),
        &grand_pane.pane_id,
        &grand_pane.tab_id,
    ));
    crate::spawn_hints::claim(&app, &kid.id, &grand_pane.pane_id).await.unwrap();
    bind_listed_instances(&env).await;
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    let grand = sqlx::query_as::<_, db::Bot>("SELECT * FROM bots WHERE name = 'deep'")
        .fetch_one(&app.db)
        .await
        .expect("the grandchild was adopted");
    assert_eq!(grand.parent_bot_id.as_deref(), Some(kid.id.as_str()), "under the child, not the top bot");
}

/// 2026-10-01 cf-優化：parent 在 verify 的 tab 裡開了 `<parent>-memleak`（沒有 spawn hint）。同 tab 的 verify 名字對不上，
/// 明確 claim 指向 parent；即使 pane 在 verify 的 tab，仍掛在 parent，短名只作顯示。
#[tokio::test]
async fn a_sibling_opened_in_a_childs_tab_follows_its_claim() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let (ws2, kid_root) = client.workspace_create("/tmp/p", "kid", json!({})).await.unwrap();
    let sib_pane = client.pane_split(&kid_root.pane_id, "down", "/tmp/p", json!({})).await.unwrap();

    let parent = a_bot(&env, "alfa").await;
    let parent_agent = crate::config::agent_name("proj", &parent);
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
    )
    .bind(db::ulid())
    .bind(&parent)
    .bind(&ws.workspace_id)
    .bind(&root.pane_id)
    .bind(&root.tab_id)
    .bind(&parent_agent)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let agent_json = |name: &str, ws: &str, pane: &str, tab: &str| {
        json!({"name": name, "agent": "claude", "agent_status": "idle",
               "workspace_id": ws, "tab_id": tab, "pane_id": pane, "cwd": "/tmp/p"})
    };
    *env.herdr.agents.lock().unwrap() = vec![
        agent_json(&parent_agent, &ws.workspace_id, &root.pane_id, &root.tab_id),
        agent_json(&format!("{parent_agent}-verify"), &ws2.workspace_id, &kid_root.pane_id, &kid_root.tab_id),
    ];
    crate::spawn_hints::claim(&app, &parent, &kid_root.pane_id).await.unwrap();
    crate::spawn_hints::claim(&app, &parent, &sib_pane.pane_id).await.unwrap();
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
    env.herdr.agents.lock().unwrap().push(agent_json(&format!("{parent_agent}-memleak"), &ws2.workspace_id, &sib_pane.pane_id, &sib_pane.tab_id));
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    let sib = sqlx::query_as::<_, db::Bot>("SELECT * FROM bots WHERE name = 'memleak'")
        .fetch_one(&app.db)
        .await
        .expect("adopted under its short name");
    assert_eq!(sib.parent_bot_id.as_deref(), Some(parent.as_str()), "the claim says the top bot opened it, not verify");
}

#[tokio::test]
async fn a_child_name_with_a_redundant_project_agent_prefix_is_stored_short() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let child_pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
    let project_label: String = sqlx::query_scalar("SELECT label FROM projects WHERE id=?")
        .bind(&env.project_id)
        .fetch_one(&app.db)
        .await
        .unwrap();

    let parent = a_bot(&env, "ops-web").await;
    let redundant_prefix_bot = a_bot(&env, "former-parent").await;
    let parent_agent = crate::config::agent_name(&project_label, &parent);
    let redundant_prefix = crate::config::agent_name(&project_label, &redundant_prefix_bot);
    let herdr_child_name = format!("{parent_agent}-{redundant_prefix}-sheet");
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
    )
    .bind(db::ulid())
    .bind(&parent)
    .bind(&ws.workspace_id)
    .bind(&root.pane_id)
    .bind(&root.tab_id)
    .bind(&parent_agent)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    *env.herdr.agents.lock().unwrap() = vec![
        json!({"name": parent_agent.clone(), "agent": "claude", "agent_status": "idle",
               "workspace_id": ws.workspace_id, "tab_id": root.tab_id, "pane_id": root.pane_id, "cwd": "/tmp/p"}),
        json!({"name": herdr_child_name.clone(), "agent": "claude", "agent_status": "idle",
               "workspace_id": ws.workspace_id, "tab_id": child_pane.tab_id, "pane_id": child_pane.pane_id, "cwd": "/tmp/p"}),
    ];

    crate::spawn_hints::claim(&app, &parent, &child_pane.pane_id).await.unwrap();
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    let child = sqlx::query_as::<_, db::Bot>("SELECT * FROM bots WHERE parent_bot_id=? AND managed_by='child' AND deleted_at IS NULL")
        .bind(&parent)
        .fetch_one(&app.db)
        .await
        .expect("child row adopted");
    assert_eq!(child.name, "sheet", "the embedded full project-agent prefix is redundant display text");
    let run = run_of(&app, &child.id).await.unwrap();
    assert_eq!(run.agent_name.as_deref(), Some(herdr_child_name.as_str()), "herdr's parent-prefixed agent name stays unchanged");
}

/// issue #82：native `SubagentStart`／`SubagentStop`（頂層 bot 自己行程內的 Task 工具呼叫，跟
/// §6.5a 血緣認領的子 pane 完全是兩回事）不能改變、也不會改變誰認領誰。父 bot 收到一則
/// `SubagentStart`（`runs.subagent_json` 因此被寫入）之後，同一個 tab 底下的孫代仍然照血緣掛在
/// 子代下面——跟沒有這則 hook 時一模一樣：hookless 的 child／grandchild pane 完全不受影響。
#[tokio::test]
async fn a_native_subagent_hook_on_the_parent_does_not_disturb_pane_based_adoption() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let kid_pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
    let grand_pane = client.pane_split(&kid_pane.pane_id, "down", "/tmp/p", json!({})).await.unwrap();

    let parent = a_bot(&env, "alfa").await;
    let parent_agent = crate::config::agent_name("proj", &parent);
    let kid_agent = format!("{parent_agent}-lastq");
    let parent_run = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
    )
    .bind(&parent_run)
    .bind(&parent)
    .bind(&ws.workspace_id)
    .bind(&root.pane_id)
    .bind(&root.tab_id)
    .bind(&parent_agent)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();

    // 父 bot 自己觸發一個 in-process Task 工具子代理：純可見性，不建立任何 pane。
    crate::hookrecv::process(
        &app,
        &crate::hookrecv::HookBody {
            bot_id: parent.clone(),
            provider: "claude".into(),
            payload: json!({"hook_event_name": "SubagentStart", "agent_id": "a1", "agent_type": "general-purpose"}),
            received_at: None,
            truncated: false,
            run_id: None,
        },
    )
    .await
    .unwrap();
    let snap: Option<String> =
        sqlx::query_scalar("SELECT subagent_json FROM runs WHERE id = ?").bind(&parent_run).fetch_one(&app.db).await.unwrap();
    assert!(snap.is_some(), "hook 有正常寫進這顆 run");

    let agent_json = |name: &str, pane: &str, tab: &str| {
        json!({"name": name, "agent": "claude", "agent_status": "idle",
               "workspace_id": ws.workspace_id, "tab_id": tab, "pane_id": pane, "cwd": "/tmp/p"})
    };
    *env.herdr.agents.lock().unwrap() = vec![
        agent_json(&parent_agent, &root.pane_id, &root.tab_id),
        agent_json(&kid_agent, &kid_pane.pane_id, &kid_pane.tab_id),
    ];
    crate::spawn_hints::claim(&app, &parent, &kid_pane.pane_id).await.unwrap();
    bind_listed_instances(&env).await;
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
    let kid = sqlx::query_as::<_, db::Bot>("SELECT * FROM bots WHERE parent_bot_id = ?")
        .bind(&parent)
        .fetch_one(&app.db)
        .await
        .unwrap();

    env.herdr.agents.lock().unwrap().push(agent_json(
        &format!("{kid_agent}-deep"),
        &grand_pane.pane_id,
        &grand_pane.tab_id,
    ));
    crate::spawn_hints::claim(&app, &kid.id, &grand_pane.pane_id).await.unwrap();
    bind_listed_instances(&env).await;
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    let grand = sqlx::query_as::<_, db::Bot>("SELECT * FROM bots WHERE name = 'deep'")
        .fetch_one(&app.db)
        .await
        .expect("the grandchild was adopted exactly like without the hook");
    assert_eq!(grand.parent_bot_id.as_deref(), Some(kid.id.as_str()), "血緣認領完全不看 subagent_json");

    // hookless：child／grandchild 自己一路都沒收過任何 hook，`subagent_json` 仍是 NULL。
    for id in [&kid.id, &grand.id] {
        let s: Option<String> = sqlx::query_scalar("SELECT subagent_json FROM runs WHERE bot_id = ?").bind(id).fetch_one(&app.db).await.unwrap();
        assert_eq!(s, None, "child 沒有 hook，不該憑空冒出快照");
    }
}

/// 2026-09-17 使用者實戰重現（issue #94）：父 agent 開一個**新** tab（不是自己那個），連續在裡面
/// 開三顆子代理。每一顆開出來時，父 agent 自己的 `PostToolUse` 都送回這個 pane 是它剛開的——這正是
/// `spawn_hints` 要餵給 `adopt_child` 的線索。三顆都應該直接掛在父 bot 底下，不會一顆掛一顆串成鏈，
/// 也不會多長出重複 bot；重複跑一次 reconcile（這次沒有新 hint 可用）也不該有任何變化。
#[tokio::test]
async fn spawn_hints_keep_a_new_tab_full_of_children_from_chaining_or_duplicating() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();

    let parent = a_bot(&env, "alfa").await;
    let parent_agent = crate::config::agent_name("proj", &parent);
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
    )
    .bind(db::ulid())
    .bind(&parent)
    .bind(&ws.workspace_id)
    .bind(&root.pane_id)
    .bind(&root.tab_id)
    .bind(&parent_agent)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();

    // 一個全新的 tab（不是父自己的），裡面連續開三顆——今晚實戰的形狀。
    let p1 = client.tab_create(&ws.workspace_id, "/tmp/p", "kids", json!({})).await.unwrap();
    let p2 = client.pane_split(&p1.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
    let p3 = client.pane_split(&p2.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
    assert_eq!((p2.tab_id.as_str(), p3.tab_id.as_str()), (p1.tab_id.as_str(), p1.tab_id.as_str()), "三個 pane 同一個新 tab");

    let agent_json = |name: &str, pane: &crate::herdr::PaneInfo| {
        json!({"name": name, "agent": "claude", "agent_status": "idle",
               "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"})
    };
    let (name1, name2, name3) = (format!("{parent_agent}-k1"), format!("{parent_agent}-k2"), format!("{parent_agent}-k3"));

    // 三次分開的 reconcile pass，模擬三次 `agent start` 之間真的會有的時間差——正是今晚串成鏈的時序。
    // 父 agent 自己全程還活著（它自己的 pane 一路都在清單裡）。
    crate::spawn_hints::record(&app, &parent, &p1.pane_id).await.unwrap();
    *env.herdr.agents.lock().unwrap() = vec![agent_json(&parent_agent, &root), agent_json(&name1, &p1)];
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    crate::spawn_hints::record(&app, &parent, &p2.pane_id).await.unwrap();
    env.herdr.agents.lock().unwrap().push(agent_json(&name2, &p2));
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    crate::spawn_hints::record(&app, &parent, &p3.pane_id).await.unwrap();
    env.herdr.agents.lock().unwrap().push(agent_json(&name3, &p3));
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    let children: Vec<db::Bot> =
        sqlx::query_as("SELECT * FROM bots WHERE parent_bot_id IS NOT NULL ORDER BY name").fetch_all(&app.db).await.unwrap();
    assert_eq!(children.len(), 3, "剛好三顆，沒有多長出重複 bot：{children:?}");
    for c in &children {
        assert_eq!(c.parent_bot_id.as_deref(), Some(parent.as_str()), "{} 要掛在真正的父 bot 底下，不是前一顆子代理", c.name);
    }

    // 用過的 hint 已經被消耗掉；再跑一次（這次沒有新 hint）靠既有的血緣配對，不該長出任何東西。
    assert!(crate::spawn_hints::for_host(&app, crate::config::LOCAL_HOST).await.unwrap().is_empty(), "hint 用完就該被消耗掉");
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bots WHERE parent_bot_id IS NOT NULL").fetch_one(&app.db).await.unwrap();
    assert_eq!(n, 3, "重跑一次不該重複建立");
}

/// 2026-10-19 mac_builder：子 agent 出現時那輪對帳還沒有線索（遠端 hook 走 spool 晚到），之後沒有別的事件叫對帳。
/// 線索的 hook 一到就要自己排一輪對帳把它認領起來，不能等到下一個不相干的 pane 事件。
#[tokio::test]
async fn a_spawn_hint_that_arrives_after_the_pass_still_gets_the_child_adopted() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let kid_pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();

    let parent = a_bot(&env, "alfa").await;
    let parent_agent = crate::config::agent_name("proj", &parent);
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
    )
    .bind(db::ulid())
    .bind(&parent)
    .bind(&ws.workspace_id)
    .bind(&root.pane_id)
    .bind(&root.tab_id)
    .bind(&parent_agent)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let agent_json = |name: &str, pane: &crate::herdr::PaneInfo| {
        json!({"name": name, "agent": "grok", "agent_status": "working",
               "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"})
    };
    *env.herdr.agents.lock().unwrap() = vec![agent_json(&parent_agent, &root), agent_json(&format!("{parent_agent}-draft"), &kid_pane)];
    bind_listed_instances(&env).await;

    // 子 agent 出現的那一輪：還沒有線索，不認領。
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bots WHERE parent_bot_id = ?").bind(&parent).fetch_one(&app.db).await.unwrap();
    assert_eq!(n, 0, "沒有線索的那輪不該認領");

    // 父 bot 的 PostToolUse 晚到；之後沒有任何人再叫 reconcile。
    crate::hookrecv::process(
        &app,
        &crate::hookrecv::HookBody {
            bot_id: parent.clone(),
            provider: "claude".into(),
            payload: json!({
                "hook_event_name": "PostToolUse",
                "tool_name": "Bash",
                "tool_input": {"command": format!("herdr agent start draft --kind grok --pane {}", kid_pane.pane_id)},
                "tool_response": {"stdout": format!(r#"{{"id":"cli:agent:start","result":{{"agent":{{"pane_id":"{}"}}}}}}"#, kid_pane.pane_id), "stderr": ""},
            }),
            received_at: None,
            truncated: false,
            run_id: None,
        },
    )
    .await
    .unwrap();

    let mut adopted = 0i64;
    for _ in 0..100 {
        adopted = sqlx::query_scalar("SELECT COUNT(*) FROM bots WHERE parent_bot_id = ?").bind(&parent).fetch_one(&app.db).await.unwrap();
        if adopted > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(adopted, 1, "線索到了就該排一輪對帳把子 agent 認領起來");
}

#[test]
fn nr1892_unverified_run_warning_is_deduplicated() {
    let run = db::ulid();
    let warnings = (0..2).filter(|_| super::warn_unverified_run(&run)).count();
    assert_eq!(warnings, 1, "repeated reconciliation must not flood WARN");
    assert!(super::warn_unverified_run(&db::ulid()), "different runs keep their diagnosis");
}

#[tokio::test]
async fn nr1892_terminal_migration_is_idempotent_and_keeps_legacy_identity_unknown() {
    let env = tt::env().await;
    let bot = a_bot(&env, "legacy").await;
    let run_id = tt::fake_run(&env.app, &bot).await;
    sqlx::query("ALTER TABLE runs DROP COLUMN terminal_id").execute(&env.app.db).await.unwrap();
    sqlx::query(&format!("PRAGMA user_version = {}", db::SCHEMA_VERSION - 1)).execute(&env.app.db).await.unwrap();
    for _ in 0..2 {
        crate::runners::am_base_tests::migrate(&env.app.db).await.unwrap();
        let run = db::run(&env.app.db, &run_id).await.unwrap().unwrap();
        assert_eq!(run.terminal_id, None);
        assert_eq!(run.bot_id, bot);
    }
    sqlx::query("UPDATE runs SET terminal_id='saved-terminal' WHERE id=?").bind(&run_id).execute(&env.app.db).await.unwrap();
    crate::runners::am_base_tests::migrate(&env.app.db).await.unwrap();
    assert_eq!(db::run(&env.app.db,&run_id).await.unwrap().unwrap().terminal_id.as_deref(), Some("saved-terminal"));
}

// Fake herdr replies deliberately omit agent_session, as they can at initial adoption.
fn nr1892_agent(pane: &crate::herdr::PaneInfo, name: &str, terminal: &str) -> Value {
    json!({"name":name, "agent":"claude", "agent_status":"idle", "pane_id":pane.pane_id,
        "workspace_id":pane.workspace_id, "tab_id":pane.tab_id, "terminal_id":terminal})
}

async fn nr1892_remote(app: &Arc<App>, env: &tt::Env, host: &str) {
    if host == "local" { return; }
    let cfg: crate::config::HostCfg = serde_json::from_value(json!({
        "name":host, "ssh":"unused.invalid", "herdr_session":"test"
    })).unwrap();
    crate::hosts::set_ssh_fake(host, |_| anyhow::bail!("no real SSH in #1892 tests"));
    app.hosts.insert_remote_with_client_for_test(cfg,
        crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"))).await;
}

#[tokio::test]
async fn macos_local_nr1892_terminal_restores_child_watch_and_completion_after_restart() {
    for host in ["local".to_owned(), format!("nr1892-{}", db::ulid())] {
        let env = tt::env().await;
        nr1892_remote(&env.app, &env, &host).await;
        sqlx::query("UPDATE projects SET host=? WHERE id=?").bind(&host).bind(&env.project_id)
            .execute(&env.app.db).await.unwrap();
        env.herdr.allow_subscribe.store(true, std::sync::atomic::Ordering::SeqCst);
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
        let parent_id = a_bot(&env, "parent").await;
        let parent = db::bot(&env.app.db, &parent_id).await.unwrap().unwrap();
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,pane_id,herdr_session,started_at) VALUES (?,?,'running','idle',?,'test',?)")
            .bind(db::ulid()).bind(&parent_id).bind(&root.pane_id).bind(db::now()).execute(&env.app.db).await.unwrap();
        *env.herdr.agents.lock().unwrap() = vec![nr1892_agent(&root,"parent","parent-terminal"), nr1892_agent(&pane,"kid","child-terminal")];
        let agent = client.agent_get(&pane.pane_id).await.unwrap().unwrap();
        let kid = super::adopt_child(&env.app,&host,&client,"test",&agent,"kid",&parent,"kid","kid","claude").await.unwrap();
        let run = db::active_run(&env.app.db, &kid).await.unwrap().unwrap();
        assert_eq!(run.terminal_id.as_deref(), Some("child-terminal"));
        assert_eq!(run.native_session_id, None);
        // Let the original adoption seed read an empty screen before the new daemon exists.
        assert!(tt::eventually!(env.herdr.calls.lock().unwrap().iter().any(|(method, args)|
            method == "pane.read" && args["pane_id"] == pane.pane_id && args["source"] == "recent_unwrapped" && args["lines"] == 200)));
        // The capture holds the bot lock through the read/store; wait until it has finished.
        drop(env.app.bot_lock(&kid).await.lock_owned().await);
        crate::events::unwatch_pane_on_session(&env.app, &host, "test", &pane.pane_id).await;
        let subscriptions_before = env.herdr.calls_to("events.subscribe").iter()
            .filter(|args| args["subscriptions"][0]["pane_id"] == pane.pane_id).count();
        let restarted = tt::restart_app(&env).await;
        nr1892_remote(&restarted, &env, &host).await;
        let key = (host.clone(), "test".to_string(), pane.pane_id.clone());
        assert!(!restarted.pane_watchers.lock().await.contains_key(&key));
        env.herdr.set_screen(&pane.pane_id, "❯ 請檢查庫存\n\n⏺ 庫存檢查完成，測試通過。\n\n❯\n");
        super::reconcile_host(&restarted, &host).await.unwrap();
        assert!(restarted.pane_watchers.lock().await.contains_key(&key), "{host}: restart must restore the watcher");
        assert!(tt::eventually!(env.herdr.calls_to("events.subscribe").iter()
            .filter(|args| args["subscriptions"][0]["pane_id"] == pane.pane_id).count() > subscriptions_before));
        let conv = db::conversation_id(&restarted.db, &kid).await.unwrap();
        let turn_id = tokio::time::timeout(std::time::Duration::from_secs(12), async {
            loop {
                // Capture inserts the completed turn before its messages; status alone
                // is not yet a completion that notify_turn can deliver.
                if let Some(id) = sqlx::query_scalar::<_, String>(
                    "SELECT t.id FROM turns t WHERE t.conversation_id=? AND t.status='completed_fallback'
                           AND t.completed_at IS NOT NULL
                           AND EXISTS (SELECT 1 FROM messages m WHERE m.turn_id=t.id AND m.role='assistant' AND trim(m.content)<>'')")
                    .bind(&conv).fetch_optional(&restarted.db).await.unwrap() { break id; }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }).await.expect("restarted reconcile must capture the child's completed reply");
        // Wait for the same capture's cursor/emit work to finish, not an arbitrary delay.
        let capture_lock = restarted.bot_lock(&kid).await;
        let _capture_guard = capture_lock.lock().await;
        let captured_reply: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='assistant'")
            .bind(&turn_id).fetch_one(&restarted.db).await.unwrap();
        assert!(captured_reply.contains("庫存檢查完成，測試通過。"));
        assert_eq!(db::run(&restarted.db, &run.id).await.unwrap().unwrap().agent_status, "idle");
        drop(_capture_guard);
        // Parent is mid-turn: the production path must durably queue the restored child's notice.
        let parent_run = db::active_run(&restarted.db, &parent_id).await.unwrap().unwrap();
        let parent_conv = db::conversation_id(&restarted.db, &parent_id).await.unwrap();
        sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
            .bind(db::ulid()).bind(&parent_conv).bind(&parent_run.id).bind(db::now()).execute(&restarted.db).await.unwrap();
        // Terminal-fallback completions wait out a short settle window (child_done FALLBACK_SETTLE_SECS)
        // before they are reported; age the capture past it instead of sleeping.
        sqlx::query("UPDATE turns SET completed_at=? WHERE id=?")
            .bind((chrono::Utc::now() - chrono::Duration::seconds(30)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
            .bind(&turn_id).execute(&restarted.db).await.unwrap();
        // Immediate notification is disabled under cfg(test); invoke its production handler.
        crate::runners::child_done::notify_turn(&restarted, &turn_id).await.unwrap();
        let crid = format!("child-done:{kid}:{turn_id}");
        let notices: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns t JOIN conversations c ON c.id=t.conversation_id WHERE c.bot_id=? AND t.client_request_id=?")
            .bind(&parent_id).bind(&crid).fetch_one(&restarted.db).await.unwrap();
        assert_eq!(notices, 1, "{host}: the parent must get a durable completion notice");
        // The CLI identity can arrive later; terminal evidence permits recording it.
        env.herdr.agents.lock().unwrap()[1]["agent_session"] = json!({"kind":"id","value":"late-native"});
        super::reconcile_host(&restarted, &host).await.unwrap();
        assert_eq!(db::run(&restarted.db, &run.id).await.unwrap().unwrap().native_session_id.as_deref(), Some("late-native"));
        let children = all_children(&restarted).await;
        assert_eq!(children.len(), 1, "no duplicate adoption");
        for p in [&root.pane_id, &pane.pane_id] {
            crate::events::unwatch_pane_on_session(&restarted, &host, "test", p).await;
        }
        let _ = ws;
    }
}

#[tokio::test]
async fn nr1892_terminal_matching_keeps_native_conflicts_and_legacy_nulls_closed() {
    let env = tt::env().await;
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, pane) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let parent = a_bot(&env,"parent").await;
    let (kid, id) = a_child(&env,&parent,"kid","old",&ws.workspace_id,&pane.tab_id,&pane.pane_id).await;
    let mut run = db::run(&env.app.db, &id).await.unwrap().unwrap();
    for (native, seen_native, terminal, seen_terminal, expected) in [
        (None, None, None, None, false),
        (None, None, None, Some("term"), false),
        (None, Some("native"), None, Some("term"), false),
        (None, None, Some(""), Some(""), false),
        (None, None, Some(" "), Some(" "), false),
        (None, None, Some("term"), None, false),
        (None, None, Some("old"), Some("new"), false),
        (None, None, Some("term"), Some("term"), true),
        (None, Some("native"), Some("term"), Some("term"), true),
        (Some("a"), Some("b"), Some("term"), Some("term"), false),
        (Some("a"), Some("a"), Some("old"), Some("new"), true),
        (Some("a"), None, None, None, true), // existing grok fallback remains unchanged
    ] {
        run.native_session_id = native.map(str::to_owned);
        run.terminal_id = terminal.map(str::to_owned);
        let mut value = nr1892_agent(&pane,"renamed",seen_terminal.unwrap_or(""));
        value["agent_session"] = seen_native.map(|id| json!({"kind":"id","value":id})).unwrap_or(Value::Null);
        let mut agent: crate::herdr::AgentInfo = serde_json::from_value(value).unwrap();
        assert_eq!(super::matches_run(&run,&agent,true), expected, "{native:?}/{seen_native:?}/{terminal:?}/{seen_terminal:?}");
        if native.is_none() { agent.pane_id = "another-pane".into(); assert!(!super::matches_run(&run,&agent,true)); }
    }
    // Legacy NULL cannot be backfilled from the current occupant, even with a parent claim.
    crate::spawn_hints::claim(&env.app,&parent,&pane.pane_id).await.unwrap();
    *env.herdr.agents.lock().unwrap() = vec![nr1892_agent(&pane,"old","new-terminal")];
    super::reconcile_host(&env.app,"local").await.unwrap();
    let kept = db::active_run(&env.app.db,&kid).await.unwrap().unwrap();
    assert_eq!(kept.id,id);
    assert_eq!(kept.terminal_id,None);
    assert!(env.app.pane_watchers.lock().await.is_empty());
    // Matching terminal IDs on the wrong host/session must not attach a child either.
    sqlx::query("DELETE FROM spawn_claims WHERE pane_id=?").bind(&pane.pane_id).execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE runs SET terminal_id='new-terminal' WHERE id=?").bind(&id).execute(&env.app.db).await.unwrap();
    let other = format!("nr1892-other-{}", db::ulid());
    nr1892_remote(&env.app, &env, &other).await;
    super::reconcile_host(&env.app,&other).await.unwrap();
    assert!(env.app.pane_watchers.lock().await.is_empty());
    sqlx::query("UPDATE runs SET herdr_session='another-session' WHERE id=?").bind(&id).execute(&env.app.db).await.unwrap();
    super::reconcile_host(&env.app,"local").await.unwrap();
    assert!(env.app.pane_watchers.lock().await.is_empty());
}

/// 2026-10-19 mac_builder：daemon 重啟後，herdr 沒綁 CLI session 的子 agent（grok）要靠 pane_id 接回來、繼續追蹤狀態；
/// 以前只認 native session，接不回，子 agent 的進度從此看不到。
#[tokio::test]
async fn a_child_without_a_herdr_session_is_kept_by_its_pane_after_a_restart() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let kid_pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();

    let parent = a_bot(&env, "alfa").await;
    let parent_agent = crate::config::agent_name("proj", &parent);
    let kid = a_bot(&env, "grok").await;
    sqlx::query("UPDATE bots SET kind='grok', managed_by='child', parent_bot_id=? WHERE id=?").bind(&parent).bind(&kid).execute(&app.db).await.unwrap();
    let kid_agent = format!("{parent_agent}-grok");
    let kid_run = db::ulid();
    for (run, bot, pane, name, native) in [
        (db::ulid(), &parent, &root, &parent_agent, None),
        (kid_run.clone(), &kid, &kid_pane, &kid_agent, Some("01a1530b-226c-7883-a859-b274392a0dda")),
    ] {
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, agent_name, herdr_session, native_session_id, started_at)
                 VALUES (?,?,'running','working',?,?,?,?,'test',?,?)",
        )
        .bind(&run)
        .bind(bot)
        .bind(&ws.workspace_id)
        .bind(&pane.pane_id)
        .bind(&pane.tab_id)
        .bind(name)
        .bind(native)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
    }
    // herdr 對 grok 不回 agent_session。
    let agent_json = |name: &str, kind: &str, pane: &crate::herdr::PaneInfo| {
        json!({"name": name, "agent": kind, "agent_status": "idle",
               "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"})
    };
    *env.herdr.agents.lock().unwrap() = vec![agent_json(&parent_agent, "claude", &root), agent_json(&kid_agent, "grok", &kid_pane)];

    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    let (state, status): (String, Option<String>) =
        sqlx::query_as("SELECT state, agent_status FROM runs WHERE id=?").bind(&kid_run).fetch_one(&app.db).await.unwrap();
    assert_eq!(state, "running");
    assert_eq!(status.as_deref(), Some("idle"), "接回來的子 agent 要拿到 herdr 現在的狀態，不是停在舊的 working");
    let kids: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bots WHERE parent_bot_id=? AND deleted_at IS NULL").bind(&parent).fetch_one(&app.db).await.unwrap();
    assert_eq!(kids, 1, "不會另外長出一顆重複的子 agent");
}

/// 2026-10-19 #1867：本機 shim 開完 grok 才 POST `/relay/spawn/claim`，比 `pane.agent_detected` 觸發的那輪對帳晚；
/// 那輪沒登記不認領，之後沒有別的事件叫對帳（要等 pane 關掉）。登記一記下就要自己排一輪對帳把子 agent 認領起來。
#[tokio::test]
async fn a_shim_spawn_claim_that_arrives_after_the_pass_still_gets_the_child_adopted() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let kid_pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();

    let parent = a_bot(&env, "alfa").await;
    let parent_agent = crate::config::agent_name("proj", &parent);
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
    )
    .bind(db::ulid())
    .bind(&parent)
    .bind(&ws.workspace_id)
    .bind(&root.pane_id)
    .bind(&root.tab_id)
    .bind(&parent_agent)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let agent_json = |name: &str, pane: &crate::herdr::PaneInfo| {
        json!({"name": name, "agent": "grok", "agent_status": "working",
               "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"})
    };
    *env.herdr.agents.lock().unwrap() = vec![agent_json(&parent_agent, &root), agent_json(&format!("{parent_agent}-draft"), &kid_pane)];
    bind_listed_instances(&env).await;

    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bots WHERE parent_bot_id = ?").bind(&parent).fetch_one(&app.db).await.unwrap();
    assert_eq!(n, 0, "沒有登記的那輪不該認領");

    // shim 的 claim 晚到；之後沒有任何人再叫 reconcile。
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("X-AM-Bot-Token", "tok".parse().unwrap());
    let (status, _) = crate::runners::credential_spawn::claim(
        axum::extract::State(app.clone()),
        headers,
        axum::extract::Form(crate::runners::credential_spawn::SpawnClaim { bot_id: parent.clone(), parent_run_id: String::new(), pane_id: kid_pane.pane_id.clone() }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);

    let mut adopted = 0i64;
    for _ in 0..100 {
        adopted = sqlx::query_scalar("SELECT COUNT(*) FROM bots WHERE parent_bot_id = ?").bind(&parent).fetch_one(&app.db).await.unwrap();
        if adopted > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(adopted, 1, "登記到了就該排一輪對帳把子 agent 認領起來");
}

/// **#94 重開**：hint 讀不到不等於沒有 hint。父 agent 在新 tab 裡連開三顆、每一顆的 hint 都已經記在表裡，只是第二、三顆
/// 出現的那一輪 SELECT 失敗——退回同 tab 推斷就會把 k2 掛到 k1 底下（串鏈、還因為短名字對錯 parent 取不到而長出重複 bot）。
/// 讀不到的那一輪一顆都不認領；DB 恢復之後，延後的那一輪自己照 hint 認領（這之後沒有任何人叫 reconcile）。
#[tokio::test]
async fn an_unreadable_spawn_hint_defers_adoption_instead_of_guessing_by_tab() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();

    let parent = a_bot(&env, "alfa").await;
    let parent_agent = crate::config::agent_name("proj", &parent);
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
    )
    .bind(db::ulid())
    .bind(&parent)
    .bind(&ws.workspace_id)
    .bind(&root.pane_id)
    .bind(&root.tab_id)
    .bind(&parent_agent)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let p1 = client.tab_create(&ws.workspace_id, "/tmp/p", "kids", json!({})).await.unwrap();
    let p2 = client.pane_split(&p1.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
    let p3 = client.pane_split(&p2.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
    let agent_json = |name: &str, pane: &crate::herdr::PaneInfo| {
        json!({"name": name, "agent": "claude", "agent_status": "idle",
               "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"})
    };
    let (name1, name2, name3) = (format!("{parent_agent}-k1"), format!("{parent_agent}-k2"), format!("{parent_agent}-k3"));
    for p in [&p1, &p2, &p3] {
        crate::spawn_hints::record(&app, &parent, &p.pane_id).await.unwrap();
    }
    let children = |app: Arc<App>| async move {
        sqlx::query_as::<_, db::Bot>("SELECT * FROM bots WHERE parent_bot_id IS NOT NULL ORDER BY name").fetch_all(&app.db).await.unwrap()
    };

    // k1 那一輪讀得到：照 hint 掛在父底下。
    *env.herdr.agents.lock().unwrap() = vec![agent_json(&parent_agent, &root), agent_json(&name1, &p1)];
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
    assert_eq!(children(app.clone()).await.len(), 1);

    // k2、k3 出現的那兩輪 hint 讀不到：k1 已經是那個 tab 的候選 parent，退回推斷就會串鏈。
    sqlx::query("ALTER TABLE spawn_hints RENAME TO spawn_hints_unreadable").execute(&app.db).await.unwrap();
    env.herdr.agents.lock().unwrap().push(agent_json(&name2, &p2));
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
    env.herdr.agents.lock().unwrap().push(agent_json(&name3, &p3));
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
    let kids = children(app.clone()).await;
    assert_eq!(kids.len(), 1, "hint 讀不到的那幾輪一顆都不認領，不猜：{kids:?}");

    // DB 恢復：延後的那一輪自己補跑，照 hint 認領，沒有鏈、沒有重複。
    sqlx::query("ALTER TABLE spawn_hints_unreadable RENAME TO spawn_hints").execute(&app.db).await.unwrap();
    let mut kids;
    let _ = crate::testing::eventually!({
        kids = children(app.clone()).await;
        kids.len() == 3
    });
    assert_eq!(kids.len(), 3, "剛好三顆：{kids:?}");
    for c in &kids {
        assert_eq!(c.parent_bot_id.as_deref(), Some(parent.as_str()), "{} 要掛在真正的父 bot 底下", c.name);
    }
    assert_eq!(kids.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["k1", "k2", "k3"], "短名字都取得到：parent 沒選錯");
}

/// 兩輪對帳同時跑（事件驅動的一輪＋延後補跑的一輪，負載一高就會重疊）：後到的那一輪 `claimed` 讀在前一輪認領之前、
/// hint 卻讀在前一輪 `consume` 之後，就退回同 tab 推斷，把已經認領的子 agent 又掛到別顆底下、長出重複 bot。
/// 同一台主機的對帳要一輪一輪來：不論幾輪同時進來，都只認領一次、掛對 parent。
#[tokio::test]
async fn overlapping_reconcile_passes_never_adopt_the_same_child_twice() {
    for round in 0..8 {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let parent = a_bot(&env, "alfa").await;
        let parent_agent = crate::config::agent_name("proj", &parent);
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, agent_name, herdr_session, started_at)
                 VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
        )
        .bind(db::ulid())
        .bind(&parent)
        .bind(&ws.workspace_id)
        .bind(&root.pane_id)
        .bind(&root.tab_id)
        .bind(&parent_agent)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let p1 = client.tab_create(&ws.workspace_id, "/tmp/p", "kids", json!({})).await.unwrap();
        let p2 = client.pane_split(&p1.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
        let p3 = client.pane_split(&p2.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
        let agent_json = |name: &str, pane: &crate::herdr::PaneInfo| {
            json!({"name": name, "agent": "claude", "agent_status": "idle",
                   "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"})
        };
        let mut agents = vec![agent_json(&parent_agent, &root)];
        for (i, p) in [&p1, &p2, &p3].into_iter().enumerate() {
            crate::spawn_hints::record(&app, &parent, &p.pane_id).await.unwrap();
            agents.push(agent_json(&format!("{parent_agent}-k{}", i + 1), p));
        }
        *env.herdr.agents.lock().unwrap() = agents;

        let host = crate::config::LOCAL_HOST;
        let (a, b, c, d) = tokio::join!(
            super::reconcile_host(&app, host),
            super::reconcile_host(&app, host),
            super::reconcile_host(&app, host),
            super::reconcile_host(&app, host)
        );
        for r in [a, b, c, d] {
            r.unwrap();
        }
        let kids: Vec<db::Bot> =
            sqlx::query_as("SELECT * FROM bots WHERE parent_bot_id IS NOT NULL ORDER BY name").fetch_all(&app.db).await.unwrap();
        assert_eq!(kids.iter().map(|k| k.name.as_str()).collect::<Vec<_>>(), ["k1", "k2", "k3"], "第 {round} 輪：不重複認領：{kids:?}");
        assert!(kids.iter().all(|k| k.parent_bot_id.as_deref() == Some(parent.as_str())), "第 {round} 輪：都掛在真正的父 bot 底下：{kids:?}");
    }
}

/// 沒有 ownership 證據時，即使名字前綴與同 tab 都吻合也保持未知（#1791）。
#[tokio::test]
async fn without_spawn_hints_a_new_tab_full_of_children_stays_unknown() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();

    let parent = a_bot(&env, "alfa").await;
    let parent_agent = crate::config::agent_name("proj", &parent);
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
    )
    .bind(db::ulid())
    .bind(&parent)
    .bind(&ws.workspace_id)
    .bind(&root.pane_id)
    .bind(&root.tab_id)
    .bind(&parent_agent)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();

    let p1 = client.tab_create(&ws.workspace_id, "/tmp/p", "kids", json!({})).await.unwrap();
    let p2 = client.pane_split(&p1.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
    let p3 = client.pane_split(&p2.pane_id, "right", "/tmp/p", json!({})).await.unwrap();

    let agent_json = |name: &str, pane: &crate::herdr::PaneInfo| {
        json!({"name": name, "agent": "claude", "agent_status": "idle",
               "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"})
    };
    let (name1, name2, name3) = (format!("{parent_agent}-k1"), format!("{parent_agent}-k2"), format!("{parent_agent}-k3"));

    // 父 agent 自己全程還活著（它自己的 pane 一路都在清單裡）。
    *env.herdr.agents.lock().unwrap() = vec![agent_json(&parent_agent, &root), agent_json(&name1, &p1)];
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
    env.herdr.agents.lock().unwrap().push(agent_json(&name2, &p2));
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
    env.herdr.agents.lock().unwrap().push(agent_json(&name3, &p3));
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    assert!(all_children(&app).await.is_empty(), "names and tabs cannot establish ownership");
}

/// issue #1472 的場景：父 bot 在 root pane，另有一顆同 tab 的 pane（`herdr pane split` 出來的、`herdr pane run` 跑 claude）。
/// 回 (client, 父 bot id, 父的 herdr agent 名, root pane, 分出來的 pane)。
async fn parent_with_split_pane(
    env: &tt::Env,
) -> (crate::herdr::HerdrClient, String, String, crate::herdr::PaneInfo, crate::herdr::PaneInfo) {
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let parent = a_bot(env, "alfa").await;
    let parent_agent = crate::config::agent_name("proj", &parent);
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
    )
    .bind(db::ulid())
    .bind(&parent)
    .bind(&ws.workspace_id)
    .bind(&root.pane_id)
    .bind(&root.tab_id)
    .bind(&parent_agent)
    .bind(db::now())
    .execute(&env.app.db)
    .await
    .unwrap();
    let split = client.pane_split(&root.pane_id, "down", "/tmp/p", json!({})).await.unwrap();
    assert_eq!(split.tab_id, root.tab_id, "分出來的 pane 在父的同一個 tab");
    (client, parent, parent_agent, root, split)
}

/// herdr 認得是 claude、但沒有名稱的 agent（`pane run "claude …"` 開的）。
fn unnamed_agent_json(pane: &crate::herdr::PaneInfo) -> serde_json::Value {
    json!({"agent": "claude", "agent_status": "idle", "workspace_id": pane.workspace_id, "tab_id": pane.tab_id,
           "pane_id": pane.pane_id, "cwd": "/tmp/p"})
}

fn named_agent_json(name: &str, pane: &crate::herdr::PaneInfo) -> serde_json::Value {
    let mut v = unnamed_agent_json(pane);
    v["name"] = json!(name);
    v
}

async fn all_children(app: &Arc<App>) -> Vec<db::Bot> {
    sqlx::query_as::<_, db::Bot>("SELECT * FROM bots WHERE parent_bot_id IS NOT NULL ORDER BY name").fetch_all(&app.db).await.unwrap()
}

/// issue #1472：有 spawn hint 的無名 agent——`agent.rename` 補上 `<parent>-<argv 的 --name 字尾>`，再照一般流程掛到父底下。
#[tokio::test]
async fn an_unnamed_agent_with_a_spawn_hint_is_renamed_from_its_argv_name_and_adopted() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (_client, parent, parent_agent, root, split) = parent_with_split_pane(&env).await;
    crate::spawn_hints::record(&app, &parent, &split.pane_id).await.unwrap();
    env.herdr.set_argv(&split.pane_id, &["claude", "--model", "claude-sonnet-5-5", "--name", &format!("{parent_agent}-om-fix"), "做事"]);
    *env.herdr.agents.lock().unwrap() = vec![named_agent_json(&parent_agent, &root), unnamed_agent_json(&split)];

    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    let want = format!("{parent_agent}-om-fix");
    let renames = env.herdr.calls_to("agent.rename");
    assert_eq!(renames.len(), 1, "只改這一顆的名字：{renames:?}");
    assert_eq!(renames[0]["target"], json!(split.pane_id));
    assert_eq!(renames[0]["name"], json!(want));
    let kids = all_children(&app).await;
    assert_eq!(kids.len(), 1, "{kids:?}");
    assert_eq!(kids[0].name, "om-fix");
    assert_eq!(kids[0].parent_bot_id.as_deref(), Some(parent.as_str()));
    let run = run_of(&app, &kids[0].id).await.unwrap();
    assert_eq!(run.agent_name.as_deref(), Some(want.as_str()), "runs.agent_name 是 herdr 上的新名字");
    assert_eq!(run.pane_id.as_deref(), Some(split.pane_id.as_str()));
    assert!(crate::spawn_hints::for_host(&app, crate::config::LOCAL_HOST).await.unwrap().is_empty(), "認領成功就消耗 hint");
}

/// argv 沒有 `--name`（或名字不是這顆 parent 的前綴、字尾不合法）：字尾由 pane id 產生，名字照樣是 `<parent>-<字尾>`。
#[tokio::test]
async fn an_unnamed_hinted_agent_without_a_usable_argv_name_gets_a_name_from_its_pane_id() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (_client, parent, parent_agent, root, split) = parent_with_split_pane(&env).await;
    crate::spawn_hints::record(&app, &parent, &split.pane_id).await.unwrap();
    // 別顆 bot 的名字（前綴對不上）、`--name` 後面沒東西都不能用。
    env.herdr.set_argv(&split.pane_id, &["claude", "--name", "someone-else-fix"]);
    *env.herdr.agents.lock().unwrap() = vec![named_agent_json(&parent_agent, &root), unnamed_agent_json(&split)];

    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    let want = format!("{parent_agent}-{}", super::unnamed_child_suffix(&split.pane_id, &parent_agent).unwrap());
    let kids = all_children(&app).await;
    assert_eq!(kids.len(), 1, "{kids:?}");
    assert_eq!(kids[0].parent_bot_id.as_deref(), Some(parent.as_str()));
    assert_eq!(run_of(&app, &kids[0].id).await.unwrap().agent_name.as_deref(), Some(want.as_str()));
    assert_eq!(env.herdr.calls_to("agent.rename")[0]["name"], json!(want));
}

/// 沒有 hint、argv 也不像哪顆 parent 的子 agent：同 tab 裡可能是使用者自己開的 claude，不收、不改名。
#[tokio::test]
async fn an_unnamed_agent_without_a_hint_or_a_parent_named_argv_is_left_alone() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (_client, _parent, parent_agent, root, split) = parent_with_split_pane(&env).await;
    env.herdr.set_argv(&split.pane_id, &["claude", "--name", "my-own-session"]);
    *env.herdr.agents.lock().unwrap() = vec![named_agent_json(&parent_agent, &root), unnamed_agent_json(&split)];

    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    assert!(all_children(&app).await.is_empty());
    assert!(env.herdr.calls_to("agent.rename").is_empty(), "不能替使用者的 agent 改名");
    assert!(env.herdr.agents.lock().unwrap().iter().any(|a| a.get("name").is_none_or(|n| n.is_null())), "它還是無名");
}

/// 沒有 hint，但 argv 的 `--name` 正好以活著的 parent 名稱加 `-` 開頭、而且同 tab：等同 hint。換了 tab 就不算。
#[tokio::test]
async fn an_unnamed_agent_with_only_an_argv_prefix_is_left_unknown() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (client, _parent, parent_agent, root, split) = parent_with_split_pane(&env).await;
    let elsewhere = client.tab_create(&root.workspace_id, "/tmp/p", "other", json!({})).await.unwrap();
    let want = format!("{parent_agent}-om-fix");
    env.herdr.set_argv(&split.pane_id, &["claude", "--name", &want]);
    env.herdr.set_argv(&elsewhere.pane_id, &["claude", "--name", &format!("{parent_agent}-far")]);
    *env.herdr.agents.lock().unwrap() =
        vec![named_agent_json(&parent_agent, &root), unnamed_agent_json(&split), unnamed_agent_json(&elsewhere)];

    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    assert!(all_children(&app).await.is_empty(), "argv prefix is not ownership evidence");
    assert!(env.herdr.calls_to("agent.rename").is_empty());
}

/// rename 失敗（舊版 herdr、撞名）：這一輪不收這顆、其他 agent 照常收編、整台主機的 reconcile 不中斷；hint 留著，下一輪再試。
#[tokio::test]
async fn a_failed_rename_skips_only_that_agent_and_is_retried_next_pass() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (client, parent, parent_agent, root, split) = parent_with_split_pane(&env).await;
    let good = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
    crate::spawn_hints::record(&app, &parent, &split.pane_id).await.unwrap();
    crate::spawn_hints::record(&app, &parent, &good.pane_id).await.unwrap();
    env.herdr.set_argv(&split.pane_id, &["claude", "--name", &format!("{parent_agent}-om-fix")]);
    *env.herdr.agents.lock().unwrap() = vec![
        named_agent_json(&parent_agent, &root),
        unnamed_agent_json(&split),
        named_agent_json(&format!("{parent_agent}-good"), &good),
    ];
    env.herdr.fail_next("agent.rename", tt::Fault::RefuseWith("name_taken"));

    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    let kids = all_children(&app).await;
    assert_eq!(kids.iter().map(|k| k.name.as_str()).collect::<Vec<_>>(), ["good"], "撞名那顆不收、其他照常：{kids:?}");
    assert_eq!(env.herdr.calls_to("agent.rename").len(), 1);

    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    let kids = all_children(&app).await;
    assert_eq!(kids.iter().map(|k| k.name.as_str()).collect::<Vec<_>>(), ["good", "om-fix"], "下一輪補收：{kids:?}");
    assert_eq!(env.herdr.calls_to("agent.rename").len(), 2);
}

/// 「hint 讀不到＝這一輪一顆都不認領」也涵蓋無名 agent：不 rename、不收；表恢復後延後的那一輪自己補上。
#[tokio::test]
async fn an_unreadable_spawn_hint_also_defers_unnamed_agents() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (_client, parent, parent_agent, root, split) = parent_with_split_pane(&env).await;
    crate::spawn_hints::record(&app, &parent, &split.pane_id).await.unwrap();
    env.herdr.set_argv(&split.pane_id, &["claude", "--name", &format!("{parent_agent}-om-fix")]);
    *env.herdr.agents.lock().unwrap() = vec![named_agent_json(&parent_agent, &root), unnamed_agent_json(&split)];

    sqlx::query("ALTER TABLE spawn_hints RENAME TO spawn_hints_unreadable").execute(&app.db).await.unwrap();
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
    assert!(all_children(&app).await.is_empty(), "hint 讀不到的那一輪不收無名 agent");
    assert!(env.herdr.calls_to("agent.rename").is_empty(), "也不改名");

    sqlx::query("ALTER TABLE spawn_hints_unreadable RENAME TO spawn_hints").execute(&app.db).await.unwrap();
    let mut kids;
    let _ = crate::testing::eventually!({
        kids = all_children(&app).await;
        kids.len() == 1
    });
    assert_eq!(kids.len(), 1, "延後的那一輪自己補上：{kids:?}");
    assert_eq!(kids[0].parent_bot_id.as_deref(), Some(parent.as_str()));
}

/// 已經有 active run 的 pane（bot 自己的 agent 被 herdr 清掉名字）不是新的子 agent：交給原本的「把名字放回去」流程。
#[tokio::test]
async fn an_unnamed_agent_in_a_panes_with_an_active_run_is_not_adopted_as_a_child() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (_client, parent, parent_agent, root, split) = parent_with_split_pane(&env).await;
    let sibling = a_bot(&env, "bravo").await;
    let sibling_agent = crate::config::agent_name("proj", &sibling);
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
    )
    .bind(db::ulid())
    .bind(&sibling)
    .bind(&split.workspace_id)
    .bind(&split.pane_id)
    .bind(&split.tab_id)
    .bind(&sibling_agent)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    crate::spawn_hints::record(&app, &parent, &split.pane_id).await.unwrap();
    *env.herdr.agents.lock().unwrap() = vec![named_agent_json(&parent_agent, &root), unnamed_agent_json(&split)];

    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    assert!(all_children(&app).await.is_empty(), "有人的 run 住在這顆 pane：不能再收成子 agent");
    // 原本的流程把名字放回去（用的是那顆 bot 自己的名字，不是 `<parent>-…`）。
    let renames = env.herdr.calls_to("agent.rename");
    assert_eq!(renames.len(), 1, "{renames:?}");
    assert_eq!(renames[0]["name"], json!(sibling_agent));
}

#[test]
fn argv_name_reads_long_short_and_equals_forms() {
    let argv = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(super::argv_agent_name(&argv(&["claude", "--name", "p-x", "go"])), Some("p-x"));
    assert_eq!(super::argv_agent_name(&argv(&["claude", "-n", "p-x"])), Some("p-x"));
    assert_eq!(super::argv_agent_name(&argv(&["claude", "--name=p-x"])), Some("p-x"));
    assert_eq!(super::argv_agent_name(&argv(&["claude", "--model", "m"])), None);
    assert_eq!(super::argv_agent_name(&argv(&["claude", "--name"])), None);
    assert_eq!(super::argv_agent_name(&argv(&["--name", "x"])), None, "argv[0] 是執行檔，不是旗標");
}

#[test]
fn unnamed_child_suffix_prefers_a_legal_argv_suffix_and_falls_back_to_the_pane_id() {
    use super::unnamed_child_suffix as s;
    // pane id 轉成 herdr 名字的字元集：小寫、非英數換成 `-`。
    assert_eq!(s("w9FY:p6A", "hub-kytpg9").as_deref(), Some("w9fy-p6a"));
    assert_eq!(super::unnamed_child_suffix_from("w9FY:p6A", "hub-kytpg9", Some("hub-kytpg9-om-fix")).as_deref(), Some("om-fix"));
    // 前綴對不上、字尾含不合法字元、整個放不進 32 字：改用 pane id。
    for argv_name in ["other-om-fix", "hub-kytpg9-Om Fix", "hub-kytpg9-", "hub-kytpg9-aaaaaaaaaaaaaaaaaaaaaaaa"] {
        assert_eq!(
            super::unnamed_child_suffix_from("w9FY:p6A", "hub-kytpg9", Some(argv_name)).as_deref(),
            Some("w9fy-p6a"),
            "{argv_name}"
        );
    }
    // pane id 太長（放不進 32 字）：留右邊、去掉開頭的 `-`。
    assert_eq!(s("ws-100000001:p100000003", "hub-kytpg9").as_deref(), Some("100000001-p100000003"));
    // 母名已經把 32 字用完、一個字都放不下的時候：不改名。
    assert_eq!(s("w9FY:p6A", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), None);
}

/// A run whose agent herdr no longer lists still exits, tab or not.
/// issue #1790（2026-10-16 mac_builder）：fork 出來的 `ops-交貨` 開的子 agent 被名字前綴掛到原 bot `ops` 底下。shim 登記了
/// 「這顆 pane 是 B 開的」（claim）選 parent；child 之後改名時以 native session 確認同一個 instance，
/// 還是同一顆 child、同一個 parent（不被收掉、也不被名字前綴重新認給 A）。
#[tokio::test]
async fn a_claimed_child_keeps_its_parent_against_the_name_prefix_and_a_later_rename() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let tab_b = client.tab_create(&ws.workspace_id, "/tmp/p", "b", json!({})).await.unwrap();
    let kids = client.tab_create(&ws.workspace_id, "/tmp/p", "kids", json!({})).await.unwrap();

    let a = a_bot(&env, "alfa").await;
    let b = a_bot(&env, "bravo").await;
    let (a_agent, b_agent) = (crate::config::agent_name("proj", &a), crate::config::agent_name("proj", &b));
    for (bot, agent, pane) in [(&a, &a_agent, &root), (&b, &b_agent, &tab_b)] {
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, agent_name, herdr_session, started_at)
                 VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
        )
        .bind(db::ulid())
        .bind(bot)
        .bind(&ws.workspace_id)
        .bind(&pane.pane_id)
        .bind(&pane.tab_id)
        .bind(agent)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
    }
    let agent_json = |name: &str, pane: &crate::herdr::PaneInfo| {
        json!({"name": name, "agent": "claude", "agent_status": "idle",
               "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"})
    };
    let kid_name = format!("{a_agent}-kid");
    *env.herdr.agents.lock().unwrap() = vec![agent_json(&a_agent, &root), agent_json(&b_agent, &tab_b), agent_json(&kid_name, &kids)];
    // B 開的：即使名字前綴說是 A，仍只接受 claim 的 bot ID。
    crate::spawn_hints::claim(&app, &b, &kids.pane_id).await.unwrap();

    bind_listed_instances(&env).await;
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
    let kids_of = |parent: String| {
        let app = app.clone();
        async move {
            sqlx::query_as::<_, db::Bot>("SELECT * FROM bots WHERE parent_bot_id = ? AND deleted_at IS NULL").bind(parent).fetch_all(&app.db).await.unwrap()
        }
    };
    let adopted = kids_of(b.clone()).await;
    assert_eq!(adopted.len(), 1, "掛在登記的 B 底下：{adopted:?}");
    assert!(kids_of(a.clone()).await.is_empty(), "不是名字前綴說的 A");
    let kid = adopted[0].clone();
    let claims = crate::spawn_hints::claims_for_host(&app, crate::config::LOCAL_HOST).await.unwrap();
    assert_eq!(claims.get(&kids.pane_id), Some(&b), "認領之後 claim 還在");

    // 之後被改名成別的前綴（還是 A 的）：同一顆 child、同一個 parent，run 記新名字。
    let renamed = format!("{a_agent}-renamed");
    *env.herdr.agents.lock().unwrap() = vec![agent_json(&a_agent, &root), agent_json(&b_agent, &tab_b), agent_json(&renamed, &kids)];
    bind_listed_instances(&env).await;
    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    let all = all_children(&app).await;
    assert_eq!(all.len(), 1, "沒有收掉、也沒有多長一顆：{all:?}");
    assert_eq!((all[0].id.as_str(), all[0].parent_bot_id.as_deref()), (kid.id.as_str(), Some(b.as_str())), "同一顆 child、同一個 parent");
    let run = db::active_run(&app.db, &kid.id).await.unwrap().expect("run 沒有被當成 agent 不見而收掉");
    assert_eq!(run.agent_name.as_deref(), Some(renamed.as_str()), "run 記新名字");
    assert_eq!(run.pane_id.as_deref(), Some(kids.pane_id.as_str()));
}

/// 未確認 child instance 的 occupant 改名：保留未知，不用名字判斷它消失（#1791）。
#[tokio::test]
async fn an_unclaimed_rename_keeps_the_child_instance_unknown() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let parent = a_bot(&env, "alfa").await;
    let parent_agent = crate::config::agent_name("proj", &parent);
    let kid_pane = client.tab_create(&ws.workspace_id, "/tmp/p", "kids", json!({})).await.unwrap();
    let (_kid, kid_run) = a_child(&env, &parent, "kid", &format!("{parent_agent}-kid"), &ws.workspace_id, &kid_pane.tab_id, &kid_pane.pane_id).await;
    let agent_json = |name: &str, pane: &crate::herdr::PaneInfo| {
        json!({"name": name, "agent": "claude", "agent_status": "idle",
               "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"})
    };
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
    )
    .bind(db::ulid())
    .bind(&parent)
    .bind(&ws.workspace_id)
    .bind(&root.pane_id)
    .bind(&root.tab_id)
    .bind(&parent_agent)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    *env.herdr.agents.lock().unwrap() =
        vec![agent_json(&parent_agent, &root), agent_json(&format!("{parent_agent}-other"), &kid_pane)];

    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id = ?").bind(&kid_run).fetch_one(&app.db).await.unwrap();
    assert_eq!(state, "running", "缺少 child instance 證據，不推定改名或消失");
    let run = db::active_run(&app.db, &_kid).await.unwrap().unwrap();
    assert_eq!(run.id, kid_run);
    assert_eq!(run.agent_name.as_deref(), Some(format!("{parent_agent}-kid").as_str()));
    assert_eq!(all_children(&app).await.len(), 1);
}

#[tokio::test]
async fn a_run_whose_agent_is_gone_still_exits() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();

    let bot = a_bot(&env, "alfa").await;
    let ghost = a_bot(&env, "bravo").await;
    let ghost_pane = client.tab_create(&ws.workspace_id, "/tmp/p", "gone", json!({})).await.unwrap();
    let live_agent = crate::config::agent_name("proj", &bot);
    for (b, pane) in [(&bot, &root.pane_id), (&ghost, &ghost_pane.pane_id)] {
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
                 VALUES (?,?,'running','idle',?,?,?,'test',?)",
        )
        .bind(db::ulid())
        .bind(b)
        .bind(&ws.workspace_id)
        .bind(pane)
        .bind(crate::config::agent_name("proj", b))
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
    }
    // herdr lists only one of the two.
    *env.herdr.agents.lock().unwrap() = vec![json!({
        "name": live_agent, "agent": "claude", "agent_status": "idle",
        "workspace_id": ws.workspace_id, "tab_id": root.tab_id, "pane_id": root.pane_id,
        "cwd": "/tmp/p"})];

    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    assert_eq!(run_of(&app, &bot).await.unwrap().state, "running");
    assert_eq!(run_of(&app, &ghost).await.unwrap().state, "exited");
}
