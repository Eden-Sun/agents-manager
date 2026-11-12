use super::*;
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn macos_local_child_spawn_environment_authenticates_as_child_through_router() {
    let env = crate::testing::env().await;
    let app = env.app.clone();
    let parent = crate::testing::claude_bot(&app, &env.project_id, "parent").await;
    let other = crate::testing::claude_bot(&app, &env.project_id, "other").await;
    let parent_run = crate::testing::fake_run(&app, &parent.id).await;
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (workspace, root) = client.workspace_create("/tmp", "p", json!({})).await.unwrap();
    let pane = client.pane_split(&root.pane_id, "right", "/tmp", json!({})).await.unwrap();
    sqlx::query("UPDATE runs SET pane_id=?,agent_name='parent',agent_status='working',workspace_id=?,tab_id=? WHERE id=?")
        .bind(&root.pane_id).bind(&workspace.workspace_id).bind(&root.tab_id).bind(&parent_run).execute(&app.db).await.unwrap();
    env.herdr.set_agent("parent", &root.pane_id, false);
    env.herdr.agents.lock().unwrap()[0]["agent_status"] = json!("working");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = crate::api::router(app.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await });
    let shim = env.dir.join("shim");
    std::fs::write(&shim, crate::herdr_shim::SHIM_SH).unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o700)).unwrap();
    let fake = env.dir.join("fake-herdr");
    std::fs::write(&fake, "#!/bin/sh\ncase \"$1 $2\" in\n'pane get') echo '{}';;\n'pane send-text') printf '%s' \"$4\" > \"$TEST_CAPTURE\";;\n'agent get') printf '{\"result\":{\"agent\":{\"pane_id\":\"%s\"}}}\\n' \"$TEST_PANE\";;\n'agent start') :;;\nesac\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    let capture = env.dir.join("capture");
    let test_path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into());
    let output = tokio::process::Command::new(&shim).env_clear().env("PATH", &test_path)
        .env("HOME", &env.dir).env("TMPDIR", &env.dir).env("AM_REAL_HERDR", &fake)
        .env("AM_BOT_ID", &parent.id).env("AM_BOT_TOKEN", &parent.hook_token)
        .env("AM_AGENT_NAME", "parent").env("AM_KIND", "codex").env("AM_DAEMON_URL", format!("http://{addr}"))
        .env("HERDR_PANE_ID", &root.pane_id).env("TEST_CAPTURE", &capture).env("TEST_PANE", &pane.pane_id)
        .args(["agent", "start", "kid", "--kind", "codex", "--pane", &pane.pane_id]).output().await.unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let source = std::fs::read_to_string(&capture).unwrap();
    let source_path = source.trim().strip_prefix(". '").unwrap().split("' && rm -f '").next().unwrap();
    let source_path = std::path::PathBuf::from(source_path);
    let child_command = |target: &str| {
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.env_clear().env("PATH", "/usr/bin:/bin").env("HOME", &env.dir).env("SOURCE", &source_path).env("SHIM", &shim).env("TARGET", target)
            .arg("-c").arg(". \"$SOURCE\"; exec \"$SHIM\" agent prompt \"$TARGET\" hello");
        cmd
    };
    let before = child_command(&format!("bot:{}", parent.id)).output().await.unwrap();
    assert!(!before.status.success() && String::from_utf8_lossy(&before.stderr).contains("child_not_registered"));
    // A different declared name in the same pane must never acquire a credential.
    let mismatched = db::ulid();
    sqlx::query("INSERT INTO child_credentials(spawn_id,token_hash,parent_bot_id,child_name,host,herdr_session,pane_id,state,created_at,expires_at) VALUES(?,?,?,'wrong-name','local','test',?,'pending',?,?)")
        .bind(&mismatched).bind(crate::child_credentials::hash_token("other-secret"))
        .bind(&parent.id).bind(&pane.pane_id).bind(db::now()).bind(db::iso_in(900)).execute(&app.db).await.unwrap();
    env.herdr.set_agent("parent-kid", &pane.pane_id, true);
    let agent: crate::herdr::AgentInfo = serde_json::from_value(json!({"name":"parent-kid","agent":"codex","agent_status":"idle", "workspace_id":workspace.workspace_id,"tab_id":pane.tab_id,"pane_id":pane.pane_id,"cwd":"/tmp","agent_session":{"kind":"id","value":"sess-1"}})).unwrap();
    let child_id = super::adopt_child(&app, "local", &client, "test", &agent, "parent-kid", &parent, "kid", "kid", "codex").await.unwrap();
    let state: String = sqlx::query_scalar("SELECT state FROM child_credentials WHERE spawn_id=?").bind(&mismatched).fetch_one(&app.db).await.unwrap();
    assert_eq!(state, "revoked");
    let alert: i64 = sqlx::query_scalar("SELECT count(*) FROM supervisor_inbox WHERE event_key=? AND kind='ops_alert'").bind(format!("child-credential-mismatch:{mismatched}")).fetch_one(&app.db).await.unwrap();
    assert_eq!(alert, 1);
    let sent = child_command(&format!("bot:{}", parent.id)).output().await.unwrap();
    assert!(sent.status.success(), "{}", String::from_utf8_lossy(&sent.stderr));
    let from: Option<String> = sqlx::query_scalar("SELECT relay_from FROM messages WHERE relay_from=? ORDER BY rowid DESC LIMIT 1").bind(&child_id).fetch_optional(&app.db).await.unwrap().flatten();
    assert_eq!(from.as_deref(), Some(child_id.as_str()));
    let denied = child_command(&format!("bot:{}", other.id)).output().await.unwrap();
    assert!(!denied.status.success() && String::from_utf8_lossy(&denied.stderr).contains("HTTP 403"));
    let injected = std::fs::read_to_string(&source_path).unwrap();
    let secret = |name: &str| injected.lines().find_map(|line| line.strip_prefix(&format!("export {name}='"))?.strip_suffix("'")).unwrap().to_string();
    let spawn_result = reqwest::Client::new().post(format!("http://{addr}/relay/spawn/begin"))
        .header("X-AM-Child-Spawn-Id", secret("AM_CHILD_SPAWN_ID"))
        .header("X-AM-Child-Token", secret("AM_CHILD_TOKEN"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("bot_id={}", parent.id)).send().await.unwrap();
    assert_eq!(spawn_result.status(), reqwest::StatusCode::FORBIDDEN);
    let spawn_from_shim = tokio::process::Command::new("/bin/sh").env_clear().env("PATH", "/usr/bin:/bin")
        .env("HOME", &env.dir).env("SOURCE", &source_path).env("SHIM", &shim)
        .arg("-c").arg(". \"$SOURCE\"; exec \"$SHIM\" agent start forbidden --kind codex --pane w1:p-new")
        .output().await.unwrap();
    assert!(!spawn_from_shim.status.success());
    let run = db::active_run(&app.db, &child_id).await.unwrap().unwrap();
    sqlx::query("UPDATE runs SET state='stopped' WHERE id=?").bind(run.id).execute(&app.db).await.unwrap();
    let after = child_command(&format!("bot:{}", parent.id)).output().await.unwrap();
    assert!(!after.status.success());
    server.abort();
}
