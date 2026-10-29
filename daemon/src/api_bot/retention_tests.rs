use super::retention;

#[tokio::test]
async fn r2_retention_removes_expired_proven_session() {
    let env = crate::testing::env().await;
    let now = crate::db::now();
    sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES('r2-bot',?,'r2-bot','claude-api','fake',?)").bind(&env.project_id).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES('r2-run','r2-bot','exited',?)").bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO conversations(id,bot_id,created_at) VALUES('r2-conv','r2-bot',?)").bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES('r2-turn','r2-conv','r2-run','web','completed','ok',?)").bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO api_bot_sessions(id,bot_id,owner_run_id,owner_turn_id,generation,revision,checkpoint_json,created_at,updated_at,expires_at) VALUES('r2-session','r2-bot','r2-run','r2-turn',1,1,'{}',?,?,?)").bind(&now).bind(&now).bind(crate::db::iso_in(-60)).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,final_text,state,created_at,processed_at) VALUES('r2-session','r2-bot','r2-run','r2-turn',1,1,'completed','fixture','applied',?,?)").bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO messages(id,conversation_id,turn_id,role,content,source,created_at) VALUES('r2-message','r2-conv','r2-turn','assistant','fixture','transcript',?)").bind(&now).execute(&env.app.db).await.unwrap();
    retention::scan_once(&env.app).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM api_bot_sessions WHERE id='r2-session'").fetch_one(&env.app.db).await.unwrap();
    assert_eq!(count, 0, "a proven expired session must be removed");
}

pub(super) async fn fixture() -> crate::testing::Env {
    let env = crate::testing::env().await;
    let now = crate::db::now();
    sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES('r2-bot',?,'r2-bot','claude-api','fake',?)").bind(&env.project_id).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES('r2-run','r2-bot','exited',?)").bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO conversations(id,bot_id,created_at) VALUES('r2-conv','r2-bot',?)").bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES('r2-turn','r2-conv','r2-run','web','completed','ok',?)").bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO api_bot_sessions(id,bot_id,owner_run_id,owner_turn_id,generation,revision,checkpoint_json,created_at,updated_at,expires_at) VALUES('r2-session','r2-bot','r2-run','r2-turn',1,5000,'{}',?,?,?)").bind(&now).bind(&now).bind(crate::db::iso_in(-60)).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,final_text,state,created_at,processed_at) VALUES('r2-session','r2-bot','r2-run','r2-turn',1,5000,'completed','fixture','applied',?,?)").bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO messages(id,conversation_id,turn_id,role,content,source,created_at) VALUES('r2-message','r2-conv','r2-turn','assistant','fixture','transcript',?)").bind(&now).execute(&env.app.db).await.unwrap();
    env
}

async fn sessions(env: &crate::testing::Env) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM api_bot_sessions WHERE id='r2-session'").fetch_one(&env.app.db).await.unwrap()
}

#[tokio::test]
async fn r2_inconsistent_and_unresolved_evidence_never_prunes() {
    for mutation in [
        "UPDATE api_bot_join_receipts SET state='pending',processed_at=NULL",
        "UPDATE api_bot_join_receipts SET state='fenced'",
        "DELETE FROM api_bot_join_receipts",
        "UPDATE api_bot_join_receipts SET revision=4999",
        "UPDATE api_bot_join_receipts SET generation=2",
        "UPDATE messages SET content='wrong'",
        "UPDATE messages SET source='hook'",
        "UPDATE messages SET incomplete=1",
        "UPDATE messages SET rewound_at='2020-01-01T00:00:00Z'",
        "UPDATE api_bot_join_receipts SET outcome='failed',final_text=NULL",
        "UPDATE turns SET delivery='unknown'",
        "UPDATE runs SET state='running'",
    ] {
        let env = fixture().await;
        sqlx::query(mutation).execute(&env.app.db).await.unwrap();
        retention::scan_once(&env.app).await.unwrap();
        assert_eq!(sessions(&env).await, 1, "unresolved fixture must remain");
    }
}

#[tokio::test]
async fn r2_scoped_ledger_preserves_other_attempts_and_hold() {
    let env = fixture().await;
    let now = crate::db::now();
    for (id, session, state, estimate, hold) in [
        ("r2-reserved", Some("r2-session"), "reserved", None, 123),
        ("r2-recorded", Some("r2-session"), "recorded", Some(44_i64), 124),
        ("r2-failed", Some("r2-session"), "failed", None, 125),
        ("r2-unknown", Some("r2-session"), "unknown", None, 126),
        ("r2-null", None, "reserved", None, 127),
    ] {
        sqlx::query("INSERT INTO api_bot_attempts(id,session_id,bot_id,generation,ordinal,request_hash,requested_speed,state,reserved_microusd,estimated_microusd,pricing_version,created_at) VALUES(?,?,'r2-bot',1,0,'fixture','standard',?,?,?,'fixture',?)").bind(id).bind(session).bind(state).bind(hold).bind(estimate).bind(&now).execute(&env.app.db).await.unwrap();
    }
    retention::scan_once(&env.app).await.unwrap();
    assert_eq!(sessions(&env).await, 0);
    let rows: Vec<(String,String,Option<i64>,i64)> = sqlx::query_as("SELECT id,state,estimated_microusd,reserved_microusd FROM api_bot_attempts ORDER BY id").fetch_all(&env.app.db).await.unwrap();
    assert_eq!(rows, vec![("r2-failed".into(),"failed".into(),None,125),("r2-null".into(),"reserved".into(),None,127),("r2-recorded".into(),"recorded".into(),Some(44),124),("r2-reserved".into(),"unknown".into(),None,123),("r2-unknown".into(),"unknown".into(),None,126)]);
    let assistant: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE id='r2-message'").fetch_one(&env.app.db).await.unwrap();
    assert_eq!(assistant, 1);
}

#[tokio::test]
async fn r2_no_api_does_not_read_private_tables() {
    let env = crate::testing::env().await;
    for table in ["api_bot_join_receipts", "api_bot_attempts", "api_bot_sessions"] {
        sqlx::query(&format!("DROP TABLE {table}")).execute(&env.app.db).await.unwrap();
    }
    retention::scan_once(&env.app).await.unwrap();
}

#[tokio::test]
async fn r2_commit_error_is_not_reported_as_success() {
    let env = fixture().await;
    sqlx::query("CREATE TABLE r2_parent(id INTEGER PRIMARY KEY)").execute(&env.app.db).await.unwrap();
    sqlx::query("CREATE TABLE r2_child(parent INTEGER REFERENCES r2_parent(id) DEFERRABLE INITIALLY DEFERRED)").execute(&env.app.db).await.unwrap();
    sqlx::query("CREATE TRIGGER r2_commit_fault AFTER DELETE ON api_bot_sessions BEGIN INSERT INTO r2_child(parent) VALUES(7); END").execute(&env.app.db).await.unwrap();
    assert!(retention::scan_once(&env.app).await.is_err());
}

#[tokio::test]
async fn r2_more_than_4096_receipts_is_retained() {
    let env = fixture().await;
    let now = crate::db::now();
    sqlx::query("WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM n WHERE x<4095) INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) SELECT printf('h%04d',x),'r2-conv','r2-run','web','completed','ok',? FROM n").bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,final_text,state,created_at,processed_at) SELECT 'r2-session','r2-bot','r2-run',t.id,1,CAST(substr(t.id,2) AS INTEGER),'completed','fixture','applied',?,? FROM turns t WHERE t.id LIKE 'h%'").bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO messages(id,conversation_id,turn_id,role,content,source,created_at) SELECT 'm-'||t.id,'r2-conv',t.id,'assistant','fixture','transcript',? FROM turns t WHERE t.id LIKE 'h%'").bind(&now).execute(&env.app.db).await.unwrap();
    retention::scan_once(&env.app).await.unwrap();
    assert_eq!(sessions(&env).await, 1);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM api_bot_join_receipts WHERE session_id='r2-session'").fetch_one(&env.app.db).await.unwrap();
    assert_eq!(n, 4097);
}

#[tokio::test]
async fn r2_applied_history_is_validated_and_ordered() {
    for historical_revision in [2_i64, 5000, 5001] {
        let env = fixture().await;
        let now = crate::db::now();
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES('r2-old-turn','r2-conv','r2-run','web','failed','ok',?)").bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,state,created_at,processed_at) VALUES('r2-session','r2-bot','r2-run','r2-old-turn',1,?,'failed','applied',?,?)").bind(historical_revision).bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
        retention::scan_once(&env.app).await.unwrap();
        assert_eq!(sessions(&env).await, if historical_revision == 2 { 0 } else { 1 });
    }
}

#[tokio::test]
async fn r2_keyset_crosses_pages_and_does_not_touch_cli() {
    let env = crate::testing::env().await;
    let now = crate::db::now();
    sqlx::query("WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM n WHERE x<64) INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) SELECT printf('b%03d',x),?,printf('bot-%03d',x),'claude-api','fake',? FROM n").bind(&env.project_id).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) SELECT 'r-'||id,id,'exited',? FROM bots WHERE id LIKE 'b___'").bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO conversations(id,bot_id,created_at) SELECT 'c-'||id,id,? FROM bots WHERE id LIKE 'b___'").bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) SELECT 't-'||id,'c-'||id,'r-'||id,'web','failed','ok',? FROM bots WHERE id LIKE 'b___'").bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO api_bot_sessions(id,bot_id,owner_run_id,owner_turn_id,generation,revision,checkpoint_json,created_at,updated_at,expires_at) SELECT 's-'||id,id,'r-'||id,'t-'||id,1,1,'{}',?,?,? FROM bots WHERE id LIKE 'b___'").bind(&now).bind(&now).bind(crate::db::iso_in(-60)).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,state,created_at,processed_at) SELECT 's-'||id,id,'r-'||id,'t-'||id,1,1,'failed','applied',?,? FROM bots WHERE id LIKE 'b___'").bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES('r2-cli',?,'r2-cli','claude','fake',?)").bind(&env.project_id).bind(&now).execute(&env.app.db).await.unwrap();
    retention::scan_once(&env.app).await.unwrap();
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM api_bot_sessions WHERE id LIKE 's-b%'").fetch_one(&env.app.db).await.unwrap();
    assert_eq!(remaining, 0);
    let cli: String = sqlx::query_scalar("SELECT kind FROM bots WHERE id='r2-cli'").fetch_one(&env.app.db).await.unwrap();
    assert_eq!(cli, "claude");
    retention::scan_once(&env.app).await.unwrap();
}

#[tokio::test]
async fn r2_retention_waits_for_lifecycle_bot_lock() {
    let env = fixture().await;
    let lock = crate::capabilities::BotLocks::bot_lock(env.app.as_ref(), "r2-bot").await;
    let guard = lock.lock_owned().await;
    let mut scan = Box::pin(retention::scan_once(&env.app));
    assert!(futures::poll!(scan.as_mut()).is_pending());
    assert_eq!(sessions(&env).await, 1);
    drop(guard);
    tokio::time::timeout(std::time::Duration::from_secs(10), scan).await.unwrap().unwrap();
    assert_eq!(sessions(&env).await, 0);
}

#[test]
fn r2_scanner_fits_one_mib_stack() {
    std::thread::Builder::new().stack_size(1024 * 1024).spawn(|| {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let env = fixture().await;
            retention::scan_once(&env.app).await.unwrap();
            assert_eq!(sessions(&env).await, 0);
        });
    }).unwrap().join().unwrap();
}

async fn future_child_fixture() -> crate::testing::Env {
    let env = fixture().await;
    sqlx::query("UPDATE bots SET managed_by='child' WHERE id='r2-bot'").execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE api_bot_sessions SET expires_at=? WHERE id='r2-session'").bind(crate::db::iso_in(3600)).execute(&env.app.db).await.unwrap();
    env
}

async fn drain_retention(env: &crate::testing::Env) {
    env.app.background_tasks.close();
    tokio::time::timeout(std::time::Duration::from_secs(10), env.app.background_tasks.wait()).await.unwrap();
}

#[tokio::test]
async fn r2_soft_delete_child_enqueues_postcommit_scan() {
    let env = future_child_fixture().await;
    assert_eq!(sessions(&env).await, 1);
    crate::api::soft_delete_child(&env.app, "r2-bot").await.unwrap();
    let deleted: Option<String> = sqlx::query_scalar("SELECT deleted_at FROM bots WHERE id='r2-bot'").fetch_one(&env.app.db).await.unwrap();
    assert!(deleted.is_some());
    drain_retention(&env).await;
    assert_eq!(sessions(&env).await, 0);
}

#[tokio::test]
async fn r2_child_delete_route_enqueues_postcommit_scan() {
    use axum::{extract::{Path, Query, State}, http::StatusCode};
    let env = future_child_fixture().await;
    let response = crate::api::delete_bot_http(State(env.app.clone()), Path("r2-bot".to_owned()), Query(crate::api::DeleteQuery::default())).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let deleted: Option<String> = sqlx::query_scalar("SELECT deleted_at FROM bots WHERE id='r2-bot'").fetch_one(&env.app.db).await.unwrap();
    assert!(deleted.is_some());
    drain_retention(&env).await;
    assert_eq!(sessions(&env).await, 0);
}

#[tokio::test]
async fn r2_ingress_retire_enqueues_only_for_retired() {
    use crate::events::ports::IngressCommands;
    use crate::child_retire::{Mode, Outcome};
    let env = future_child_fixture().await;
    assert_eq!(env.app.retire_child("r2-bot", "fixture", Mode::Explicit).await.unwrap(), Outcome::Retired);
    drain_retention(&env).await;
    assert_eq!(sessions(&env).await, 0);
    let env = fixture().await;
    assert_eq!(env.app.retire_child("missing-r2", "fixture", Mode::Explicit).await.unwrap(), Outcome::AlreadyGone);
    drain_retention(&env).await;
    assert_eq!(sessions(&env).await, 1, "non-Retired must not enqueue a global scan");
}

#[tokio::test]
async fn r2_decided_delete_intent_recovery_enqueues_scan() {
    for kind in ["delete_bot", "delete_project"] {
        let env = future_child_fixture().await;
        sqlx::query("UPDATE bots SET deleted_at=? WHERE id='r2-bot'").bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        if kind == "delete_project" {
            sqlx::query("UPDATE projects SET deleted_at=? WHERE id=?").bind(crate::db::now()).bind(&env.project_id).execute(&env.app.db).await.unwrap();
        }
        let subject = if kind == "delete_bot" { "r2-bot" } else { &env.project_id };
        let snapshot = if kind == "delete_bot" { serde_json::json!({"bots": []}) } else { serde_json::json!({"bots": [{"id":"r2-bot", "managed_by":"child"}]}) };
        let crate::intents::Inserted::New(intent) = crate::intents::insert(&env.app.db, kind, subject, crate::config::LOCAL_HOST, &snapshot, 3600).await.unwrap() else { panic!("new intent expected"); };
        let outcome = crate::delete_intents::drive_once(&env.app, &intent.id).await;
        assert!(matches!(outcome, crate::runners::restart_intents::Outcome::Finished));
        assert_eq!(crate::intents::get(&env.app.db, &intent.id).await.unwrap().unwrap().status, "done");
        drain_retention(&env).await;
        assert_eq!(sessions(&env).await, 0, "{kind}");
    }
}

#[tokio::test]
async fn r2_uncommitted_child_delete_intent_does_not_enqueue_scan() {
    let env = fixture().await;
    sqlx::query("UPDATE bots SET managed_by='child' WHERE id='r2-bot'").execute(&env.app.db).await.unwrap();
    let crate::intents::Inserted::New(intent) = crate::intents::insert(&env.app.db, "delete_bot", "r2-bot", crate::config::LOCAL_HOST, &serde_json::json!({"bots": []}), 3600).await.unwrap() else { panic!("new intent expected"); };
    assert!(matches!(crate::delete_intents::drive_once(&env.app, &intent.id).await, crate::runners::restart_intents::Outcome::Finished));
    assert_eq!(crate::intents::get(&env.app.db, &intent.id).await.unwrap().unwrap().status, "abandoned");
    drain_retention(&env).await;
    assert_eq!(sessions(&env).await, 1);
}

#[tokio::test]
async fn r2_user_bot_and_project_routes_enqueue_after_projection() {
    use axum::{extract::{Path, Query, State}, http::StatusCode};
    for target in ["bot", "project"] {
        let env = fixture().await;
        let sentinel_path = env.dir.join("r2-sentinel-project");
        std::fs::create_dir_all(&sentinel_path).unwrap();
        let sentinel_path = sentinel_path.to_string_lossy().to_string();
        sqlx::query("INSERT INTO projects(id,path,label,host,created_at) VALUES('r2-sentinel-project',?,'sentinel','local',?)").bind(&sentinel_path).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        sqlx::query("UPDATE bots SET managed_by='child',project_id='r2-sentinel-project' WHERE id='r2-bot'").execute(&env.app.db).await.unwrap();
        let cli = crate::testing::claude_bot(&env.app, &env.project_id, "r2-cli").await;
        let project_id = env.project_id.clone();
        let repo = env.repo.to_string_lossy().to_string();
        let cli_id = cli.id.clone();
        env.app.cfg.update(move |cfg| {
            let bot: crate::config::BotCfg = toml::from_str(&format!("id = '{cli_id}'\nname = 'r2-cli'\nkind = 'claude'\n")).unwrap();
            cfg.projects = vec![crate::config::ProjectCfg { handed_off_to: None, id: Some(project_id), path: repo, label: "proj".into(), host: crate::config::LOCAL_HOST.into(), bots: vec![bot] }, crate::config::ProjectCfg { handed_off_to: None, id: Some("r2-sentinel-project".into()), path: sentinel_path, label: "sentinel".into(), host: crate::config::LOCAL_HOST.into(), bots: vec![] }];
            Ok(())
        }).await.unwrap();
        let response = if target == "bot" {
            crate::api::delete_bot_http(State(env.app.clone()), Path(cli.id.clone()), Query(crate::api::DeleteQuery::default())).await.unwrap()
        } else {
            crate::api::delete_project_http(State(env.app.clone()), Path(env.project_id.clone()), Query(crate::api::DeleteQuery::default())).await.unwrap()
        };
        assert_eq!(response.status(), StatusCode::OK);
        let deleted: Option<String> = sqlx::query_scalar("SELECT deleted_at FROM bots WHERE id=?").bind(&cli.id).fetch_one(&env.app.db).await.unwrap();
        assert!(deleted.is_some());
        drain_retention(&env).await;
        assert_eq!(sessions(&env).await, 0, "{target}");
    }
}

#[tokio::test]
async fn r2_exactly_4096_receipts_can_be_proven() {
    let env = fixture().await;
    let now = crate::db::now();
    sqlx::query("WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM n WHERE x<4094) INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) SELECT printf('h%04d',x),'r2-conv','r2-run','web','completed','ok',? FROM n").bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,final_text,state,created_at,processed_at) SELECT 'r2-session','r2-bot','r2-run',t.id,1,CAST(substr(t.id,2) AS INTEGER),'completed','fixture','applied',?,? FROM turns t WHERE t.id LIKE 'h%'").bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO messages(id,conversation_id,turn_id,role,content,source,created_at) SELECT 'm-'||t.id,'r2-conv',t.id,'assistant','fixture','transcript',? FROM turns t WHERE t.id LIKE 'h%'").bind(&now).execute(&env.app.db).await.unwrap();
    retention::scan_once(&env.app).await.unwrap();
    assert_eq!(sessions(&env).await, 0);
}

#[tokio::test]
async fn r2_proof_read_error_preserves_session_and_receipt() {
    let env = fixture().await;
    sqlx::query("ALTER TABLE messages RENAME TO r2_missing_messages").execute(&env.app.db).await.unwrap();
    retention::scan_once(&env.app).await.unwrap();
    assert_eq!(sessions(&env).await, 1);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM api_bot_join_receipts WHERE session_id='r2-session'").fetch_one(&env.app.db).await.unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn r2_closed_startup_gate_never_prunes() {
    let env = fixture().await;
    let app = crate::testing::restart_app(&env).await;
    retention::scan_once(&app).await.unwrap();
    assert_eq!(sessions(&env).await, 1);
}

#[tokio::test]
async fn r2_changed_expiry_or_restore_invalidates_candidate_fence() {
    for changed in ["expiry", "restore"] {
        let env = fixture().await;
        let unique_run = crate::db::ulid();
        sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES(?,'r2-bot','exited',?)").bind(&unique_run).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        sqlx::query("UPDATE turns SET run_id=? WHERE id='r2-turn'").bind(&unique_run).execute(&env.app.db).await.unwrap();
        sqlx::query("UPDATE api_bot_sessions SET owner_run_id=? WHERE id='r2-session'").bind(&unique_run).execute(&env.app.db).await.unwrap();
        sqlx::query("UPDATE api_bot_join_receipts SET run_id=? WHERE session_id='r2-session'").bind(&unique_run).execute(&env.app.db).await.unwrap();
        if changed == "restore" {
            sqlx::query("UPDATE api_bot_sessions SET expires_at=? WHERE id='r2-session'").bind(crate::db::iso_in(3600)).execute(&env.app.db).await.unwrap();
            sqlx::query("UPDATE bots SET deleted_at=? WHERE id='r2-bot'").bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        }
        let entered = std::sync::Arc::new(tokio::sync::Notify::new());
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let enter_hook = entered.clone(); let release_hook = release.clone();
        crate::lifecycle::race_point::arm("api_retention_before_tx", &unique_run, move || async move { enter_hook.notify_one(); release_hook.notified().await; });
        let app = env.app.clone();
        let scan = tokio::spawn(async move { retention::scan_once(&app).await });
        tokio::time::timeout(std::time::Duration::from_secs(10), entered.notified()).await.unwrap();
        if changed == "expiry" {
            sqlx::query("UPDATE api_bot_sessions SET expires_at=? WHERE id='r2-session'").bind(crate::db::iso_in(3600)).execute(&env.app.db).await.unwrap();
        } else {
            sqlx::query("UPDATE bots SET deleted_at=NULL WHERE id='r2-bot'").execute(&env.app.db).await.unwrap();
        }
        release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(10), scan).await.unwrap().unwrap().unwrap();
        assert_eq!(sessions(&env).await, 1, "{changed}");
    }
}

#[tokio::test]
async fn r2_proof_rejects_duplicate_assistant_and_wrong_owner() {
    for mutation in ["duplicate", "owner"] {
        let env = fixture().await;
        if mutation == "duplicate" {
            sqlx::query("INSERT INTO messages(id,conversation_id,turn_id,role,content,source,created_at) VALUES('r2-extra','r2-conv','r2-turn','assistant','fixture','transcript',?)").bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        } else {
            sqlx::query("UPDATE api_bot_sessions SET owner_turn_id=NULL WHERE id='r2-session'").execute(&env.app.db).await.unwrap();
        }
        retention::scan_once(&env.app).await.unwrap();
        assert_eq!(sessions(&env).await, 1, "{mutation}");
        let receipt: i64 = sqlx::query_scalar("SELECT count(*) FROM api_bot_join_receipts WHERE session_id='r2-session'").fetch_one(&env.app.db).await.unwrap();
        assert_eq!(receipt, 1);
    }
}

#[tokio::test]
async fn r2_failed_first_scan_is_retried_after_storage_repair() {
    let env = fixture().await;
    sqlx::query("ALTER TABLE messages RENAME TO r2_missing_messages").execute(&env.app.db).await.unwrap();
    retention::schedule_retention_after_delete(&env.app);
    drain_retention(&env).await;
    assert_eq!(sessions(&env).await, 1);
    sqlx::query("ALTER TABLE r2_missing_messages RENAME TO messages").execute(&env.app.db).await.unwrap();
    retention::scan_once(&env.app).await.unwrap();
    assert_eq!(sessions(&env).await, 0, "a later scan retries durable eligibility");
}

#[tokio::test]
async fn r2_hourly_ticker_scans_only_after_first_hour() {
    let env = fixture().await;
    tokio::time::pause();
    retention::spawn_retention_ticker(env.app.clone());
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_secs(3599)).await;
    tokio::time::resume();
    assert_eq!(sessions(&env).await, 1);
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    tokio::time::resume();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while sessions(&env).await != 0 { tokio::task::yield_now().await; }
    }).await.unwrap();
    env.app.shutdown.cancel();
    drain_retention(&env).await;
}

#[tokio::test]
async fn r2_unresolved_receipt_retains_all_ledger_and_message_data() {
    for state in ["pending", "fenced"] {
        let env = fixture().await;
        let now = crate::db::now();
        sqlx::query("INSERT INTO api_bot_attempts(id,session_id,bot_id,generation,ordinal,request_hash,requested_speed,state,reserved_microusd,pricing_version,created_at) VALUES('r2-hold','r2-session','r2-bot',1,0,'fixture','standard','reserved',123,'fixture',?)").bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("UPDATE api_bot_join_receipts SET state=?,processed_at=CASE WHEN ?='pending' THEN NULL ELSE processed_at END").bind(state).bind(state).execute(&env.app.db).await.unwrap();
        retention::scan_once(&env.app).await.unwrap();
        assert_eq!(sessions(&env).await, 1);
        let receipt: String = sqlx::query_scalar("SELECT state FROM api_bot_join_receipts WHERE session_id='r2-session'").fetch_one(&env.app.db).await.unwrap();
        assert_eq!(receipt, state);
        let attempt: (String, Option<i64>, i64, Option<String>) = sqlx::query_as("SELECT state,estimated_microusd,reserved_microusd,session_id FROM api_bot_attempts WHERE id='r2-hold'").fetch_one(&env.app.db).await.unwrap();
        assert_eq!(attempt, ("reserved".into(), None, 123, Some("r2-session".into())));
        let message: String = sqlx::query_scalar("SELECT content FROM messages WHERE id='r2-message'").fetch_one(&env.app.db).await.unwrap();
        assert_eq!(message, "fixture");
    }
}

#[tokio::test]
async fn r2_project_route_cleans_child_adopted_after_snapshot() {
    use axum::{extract::{Path, Query, State}, http::StatusCode};
    let env = future_child_fixture().await;
    let other_path = env.dir.join("r2-other-project");
    std::fs::create_dir_all(&other_path).unwrap();
    let other_path = other_path.to_string_lossy().to_string();
    sqlx::query("INSERT INTO projects(id,path,label,host,created_at) VALUES('r2-other',?,'other','local',?)").bind(&other_path).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE bots SET project_id='r2-other' WHERE id='r2-bot'").execute(&env.app.db).await.unwrap();
    let cli = crate::testing::claude_bot(&env.app, &env.project_id, "r2-cli").await;
    let project_id = env.project_id.clone(); let repo = env.repo.to_string_lossy().to_string(); let cli_id = cli.id.clone();
    env.app.cfg.update(move |cfg| {
        let bot: crate::config::BotCfg = toml::from_str(&format!("id = '{cli_id}'\nname = 'r2-cli'\nkind = 'claude'\n")).unwrap();
        cfg.projects = vec![crate::config::ProjectCfg { handed_off_to: None, id: Some(project_id), path: repo, label: "proj".into(), host: crate::config::LOCAL_HOST.into(), bots: vec![bot] }, crate::config::ProjectCfg { handed_off_to: None, id: Some("r2-other".into()), path: other_path, label: "other".into(), host: crate::config::LOCAL_HOST.into(), bots: vec![] }];
        Ok(())
    }).await.unwrap();
    let pool = env.app.db.clone(); let project_id = env.project_id.clone();
    crate::lifecycle::race_point::arm("delete_after_intent", &env.project_id, move || async move {
        sqlx::query("UPDATE bots SET project_id=? WHERE id='r2-bot'").bind(project_id).execute(&pool).await.unwrap();
    });
    let response = crate::api::delete_project_http(State(env.app.clone()), Path(env.project_id.clone()), Query(crate::api::DeleteQuery::default())).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let deleted: Option<String> = sqlx::query_scalar("SELECT deleted_at FROM bots WHERE id='r2-bot'").fetch_one(&env.app.db).await.unwrap();
    assert!(deleted.is_some());
    drain_retention(&env).await;
    assert_eq!(sessions(&env).await, 0);
}

#[tokio::test]
async fn r2_failed_turn_with_assistant_is_not_deletable() {
    let env = fixture().await;
    sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES('r2-failed-turn','r2-conv','r2-run','web','failed','ok',?)").bind(crate::db::now()).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO messages(id,conversation_id,turn_id,role,content,source,created_at) VALUES('r2-failed-message','r2-conv','r2-failed-turn','assistant','fixture','transcript',?)").bind(crate::db::now()).execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE api_bot_sessions SET owner_turn_id='r2-failed-turn' WHERE id='r2-session'").execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE api_bot_join_receipts SET turn_id='r2-failed-turn',outcome='failed',final_text=NULL WHERE session_id='r2-session'").execute(&env.app.db).await.unwrap();
    retention::scan_once(&env.app).await.unwrap();
    assert_eq!(sessions(&env).await, 1);
}

#[tokio::test]
async fn r2_bot_kind_read_error_fails_closed() {
    let env = fixture().await;
    sqlx::query("ALTER TABLE bots RENAME TO r2_missing_bots").execute(&env.app.db).await.unwrap();
    assert!(retention::scan_once(&env.app).await.is_err());
    assert_eq!(sessions(&env).await, 1);
    let receipt: i64 = sqlx::query_scalar("SELECT count(*) FROM api_bot_join_receipts WHERE session_id='r2-session'").fetch_one(&env.app.db).await.unwrap();
    assert_eq!(receipt, 1);
}

#[tokio::test]
async fn r2_history_identity_and_mixed_pending_preserve_session() {
    for fault in ["bot", "run", "conversation", "pending"] {
        let env = fixture().await;
        let now = crate::db::now();
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES('r2-other',?,'other','claude-api','fake',?)").bind(&env.project_id).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES('r2-other-run','r2-bot','exited',?)").bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO conversations(id,bot_id,created_at) VALUES('r2-other-conv','r2-other',?)").bind(&now).execute(&env.app.db).await.unwrap();
        let conversation = if fault == "conversation" { "r2-other-conv" } else { "r2-conv" };
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES('r2-history',?,'r2-run','web','failed','ok',?)").bind(conversation).bind(&now).execute(&env.app.db).await.unwrap();
        let bot = if fault == "bot" { "r2-other" } else { "r2-bot" };
        let run = if fault == "run" { "r2-other-run" } else { "r2-run" };
        let state = if fault == "pending" { "pending" } else { "applied" };
        sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,state,created_at,processed_at) VALUES('r2-session',?,?,'r2-history',1,2,'failed',?,?,?)").bind(bot).bind(run).bind(state).bind(&now).bind(if state == "pending" { None } else { Some(now.as_str()) }).execute(&env.app.db).await.unwrap();
        retention::scan_once(&env.app).await.unwrap();
        assert_eq!(sessions(&env).await, 1, "{fault}");
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM api_bot_join_receipts WHERE session_id='r2-session'").fetch_one(&env.app.db).await.unwrap();
        assert_eq!(count, 2);
    }
}

#[tokio::test]
async fn r2_project_intent_recovery_soft_deletes_live_child() {
    let env = future_child_fixture().await;
    sqlx::query("UPDATE projects SET deleted_at=? WHERE id=?").bind(crate::db::now()).bind(&env.project_id).execute(&env.app.db).await.unwrap();
    let payload = serde_json::json!({"bots": [{"id":"r2-bot", "managed_by":"child"}]});
    let crate::intents::Inserted::New(intent) = crate::intents::insert(&env.app.db, "delete_project", &env.project_id, crate::config::LOCAL_HOST, &payload, 3600).await.unwrap() else { panic!("new intent expected"); };
    assert!(matches!(crate::delete_intents::drive_once(&env.app, &intent.id).await, crate::runners::restart_intents::Outcome::Finished));
    assert_eq!(crate::intents::get(&env.app.db, &intent.id).await.unwrap().unwrap().status, "done");
    let deleted: Option<String> = sqlx::query_scalar("SELECT deleted_at FROM bots WHERE id='r2-bot'").fetch_one(&env.app.db).await.unwrap();
    assert!(deleted.is_some());
    drain_retention(&env).await;
    assert_eq!(sessions(&env).await, 0);
}

#[tokio::test]
async fn r2_uncommitted_project_intent_does_not_enqueue_scan() {
    let env = fixture().await;
    let project_id = env.project_id.clone(); let repo = env.repo.to_string_lossy().to_string();
    env.app.cfg.update(move |cfg| {
        cfg.projects = vec![crate::config::ProjectCfg { handed_off_to: None, id: Some(project_id), path: repo, label: "proj".into(), host: crate::config::LOCAL_HOST.into(), bots: vec![] }];
        Ok(())
    }).await.unwrap();
    let payload = serde_json::json!({"bots": []});
    let crate::intents::Inserted::New(intent) = crate::intents::insert(&env.app.db, "delete_project", &env.project_id, crate::config::LOCAL_HOST, &payload, 3600).await.unwrap() else { panic!("new intent expected"); };
    assert!(matches!(crate::delete_intents::drive_once(&env.app, &intent.id).await, crate::runners::restart_intents::Outcome::Finished));
    assert_eq!(crate::intents::get(&env.app.db, &intent.id).await.unwrap().unwrap().status, "abandoned");
    drain_retention(&env).await;
    assert_eq!(sessions(&env).await, 1);
}
