use super::*;

#[tokio::test]
async fn the_snapshot_does_no_per_bot_lookups() {
    let env = crate::testing::env().await;
    for i in 0..15 {
        let b = crate::testing::claude_bot(&env.app, &env.project_id, &format!("many{i}")).await;
        crate::testing::fake_run(&env.app, &b.id).await;
    }
    db::PER_BOT_LOOKUPS.with(|c| c.set(0));
    let state = state_json(&env.app).await.unwrap();
    assert_eq!(state["projects"][0]["bots"].as_array().unwrap().len(), 15);
    assert_eq!(db::PER_BOT_LOOKUPS.with(|c| c.get()), 0, "每顆 bot 再查一次（active_run／queued_turn／bot／bot_host）= N+1，要改成一次讀完");
}

#[tokio::test]
async fn nr1892_state_json_projects_persisted_run_identity() {
    let e = crate::testing::env().await;
    for (i, (adopted, native, terminal, reason)) in [
        (1, None, None, Some("missing_instance_identity")),
        (1, Some(" "), Some("\t"), Some("missing_instance_identity")),
        (1, Some("native"), None, None),
        (1, None, Some("terminal"), None),
        (0, None, None, None),
    ].into_iter().enumerate() {
        let bot = crate::testing::claude_bot(&e.app, &e.project_id, &format!("identity-{i}")).await;
        let run_id = crate::testing::fake_run(&e.app, &bot.id).await;
        sqlx::query("UPDATE runs SET adopted=?, native_session_id=?, terminal_id=?, turn_error='original error' WHERE id=?")
            .bind(adopted).bind(native).bind(terminal).bind(&run_id).execute(&e.app.db).await.unwrap();
        let state = state_json(&e.app).await.unwrap();
        let row = state["projects"][0]["bots"].as_array().unwrap().iter().find(|b| b["id"] == bot.id).unwrap();
        assert_eq!(row["run"]["adopted"], json!(adopted));
        assert_eq!(row["run"]["native_session_id"], json!(native));
        assert_eq!(row["run"]["terminal_id"], json!(terminal));
        assert_eq!(row["run"]["identity_recovery_reason"], json!(reason));
        assert_eq!(row["run"]["state"], "running");
        assert_eq!(row["run"]["agent_status"], "idle");
        assert_eq!(row["run"]["turn_error"], "original error");
    }
}

#[tokio::test]
async fn each_bots_run_lamp_and_queued_turn_are_still_its_own() {
    let env = crate::testing::env().await;
    let running = crate::testing::claude_bot(&env.app, &env.project_id, "running").await;
    let run = crate::testing::fake_run(&env.app, &running.id).await;
    let queued = crate::testing::claude_bot(&env.app, &env.project_id, "queued").await;
    let _idle = crate::testing::claude_bot(&env.app, &env.project_id, "idle").await;
    let conv = db::ulid();
    sqlx::query("INSERT INTO conversations (id, bot_id, created_at) VALUES (?,?,?)").bind(&conv).bind(&queued.id).bind(db::now()).execute(&env.app.db).await.unwrap();
    let turn = db::ulid();
    sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,'web','queued','pending','等著',?)")
        .bind(&turn)
        .bind(&conv)
        .bind(db::now())
        .execute(&env.app.db)
        .await
        .unwrap();
    let state = state_json(&env.app).await.unwrap();
    let bots: Vec<&Value> = state["projects"].as_array().unwrap().iter().flat_map(|p| p["bots"].as_array().unwrap()).collect();
    let by = |name: &str| bots.iter().find(|b| b["name"] == name).copied().unwrap();
    assert_eq!(by("running")["run"]["id"], json!(run));
    assert_eq!(by("running")["lamp"], "idle");
    assert!(by("running")["queued_turn"].is_null());
    assert_eq!(by("queued")["queued_turn"]["id"], json!(turn));
    assert!(by("queued")["run"].is_null());
    assert_eq!(by("queued")["lamp"], "offline");
    assert!(by("idle")["run"].is_null() && by("idle")["queued_turn"].is_null());
    env.app.connected.store(false, Ordering::SeqCst);
    let state = state_json(&env.app).await.unwrap();
    let lamp = |name: &str| state["projects"][0]["bots"].as_array().unwrap().iter().find(|b| b["name"] == name).unwrap()["lamp"].clone();
    assert_eq!(lamp("running"), "disconnected", "herdr 斷線時燈是 disconnected");
    assert_eq!(lamp("idle"), "disconnected");
}

/// #1663：`/state` 取 config 的次數不隨主機數成長。舊寫法 `hosts_list` 每台主機的 `is_shared` 各 clone 一次整份設定，
/// web 在 herdr 斷線時每 3 秒打一次 `/state`，就是 N 次 clone。
#[tokio::test]
async fn config_reads_per_state_do_not_grow_with_hosts() {
    let env = crate::testing::env().await;
    for i in 0..5 {
        let b = crate::testing::claude_bot(&env.app, &env.project_id, &format!("cfg{i}")).await;
        crate::testing::fake_run(&env.app, &b.id).await;
    }
    let reads = |app: Arc<App>| async move {
        am_config::GET_CALLS.with(|c| c.set(0));
        state_json(&app).await.unwrap();
        am_config::GET_CALLS.with(|c| c.get())
    };
    let local_only = reads(env.app.clone()).await;
    for i in 0..10 {
        env.app
            .hosts
            .insert_remote_for_test(crate::config::HostCfg { daemon_url: None,
                shared_session: false,
                name: format!("cfg-host-{i}"),
                ssh: "unused".into(),
                ssh_port: 22,
                ssh_opts: vec![],
                herdr_session: "am-test".into(),
                remote_path: String::new(),
            })
            .await;
    }
    let with_hosts = reads(env.app.clone()).await;
    assert_eq!(with_hosts, local_only, "/state 取 config 的次數不能隨主機數成長（只有本機 {local_only} 次，加 10 台後 {with_hosts} 次）");
}

#[tokio::test]
async fn state_projects_a_short_name_for_legacy_children_without_rewriting_the_database() {
    let env = crate::testing::env().await;
    sqlx::query("UPDATE projects SET label='智選hub' WHERE id=?").bind(&env.project_id).execute(&env.app.db).await.unwrap();
    let parent = crate::testing::claude_bot(&env.app, &env.project_id, "parent").await;
    let child = crate::testing::claude_bot(&env.app, &env.project_id, "legacy").await;
    sqlx::query("UPDATE bots SET name='hub-dgs9j9-sheet', managed_by='child', parent_bot_id=?, inject_hooks=0 WHERE id=?")
        .bind(&parent.id)
        .bind(&child.id)
        .execute(&env.app.db)
        .await
        .unwrap();

    let state = state_json(&env.app).await.unwrap();
    let projected = state["projects"][0]["bots"].as_array().unwrap().iter().find(|b| b["id"] == child.id).unwrap();
    assert_eq!(projected["name"], "sheet");
    let stored: String = sqlx::query_scalar("SELECT name FROM bots WHERE id=?").bind(&child.id).fetch_one(&env.app.db).await.unwrap();
    assert_eq!(stored, "hub-dgs9j9-sheet", "display normalization must not rename a live row or risk a project name collision");
}
