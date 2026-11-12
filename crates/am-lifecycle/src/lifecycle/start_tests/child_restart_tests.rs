//! `restart_child_in_pane` when the agent will not leave (review 2026-09-12 #1).
use super::*;
use crate::capabilities::Db;
use crate::testing as tt;

/// Agent ignores ctrl+c: after 20 polls the `stopping` run must return to `running` (else 409s
/// and a permanently yellow bot).
#[tokio::test]
async fn a_child_that_will_not_exit_gets_its_run_back() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let kid_pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();

    let parent = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
         VALUES (?,?,'alfa','claude','[]',0,1,'tok',?)",
    )
    .bind(&parent)
    .bind(&env.project_id)
    .bind(db::now())
    .execute(app.db())
    .await
    .unwrap();
    let kid = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, parent_bot_id, created_at)
         VALUES (?,?,'ui','claude','[]',0,0,'tok','child',?,?)",
    )
    .bind(&kid)
    .bind(&env.project_id)
    .bind(&parent)
    .bind(db::now())
    .execute(app.db())
    .await
    .unwrap();
    let run_id = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, adopted, started_at)
         VALUES (?,?,'running','idle',?,?,?,'proj-alfa-ui','test',1,?)",
    )
    .bind(&run_id)
    .bind(&kid)
    .bind(&ws.workspace_id)
    .bind(&kid_pane.tab_id)
    .bind(&kid_pane.pane_id)
    .bind(db::now())
    .execute(app.db())
    .await
    .unwrap();
    // Explicitly model an agent ignoring ctrl+c, independently of its display name.
    *env.herdr.agents.lock().unwrap() = vec![json!({
        "name": null, "agent": "claude", "agent_status": "idle", "ignore_ctrl_c": true,
        "workspace_id": ws.workspace_id, "tab_id": kid_pane.tab_id, "pane_id": kid_pane.pane_id,
        "cwd": "/tmp/p"})];

    let err = restart_child_in_pane(&app, &kid).await.expect_err("the agent never left");
    assert!(matches!(err, LcError::Upstream(_)), "{err:?}");

    let run = db::run(app.db(), &run_id).await.unwrap().unwrap();
    assert_eq!(run.state, "running", "the agent is still in its pane, so the run is still live");
    assert!(run.ended_at.is_none());
    assert_eq!(db::active_run(app.db(), &kid).await.unwrap().map(|r| r.id), Some(run_id));
    // Nothing touched the pane.
    assert!(env.herdr.tab(&kid_pane.tab_id).unwrap().panes.contains(&kid_pane.pane_id));
    assert!(!env.herdr.methods().iter().any(|m| m == "pane.close"));
}

/// 子 agent 原地重啟的「舊 run 停了、新 run 還沒寫進去」那一段也是重啟中（#106 換到這條路）：
/// 不拿 bot 鎖的 orphan sweeper 剛好在這時候跑，不可以把 AGM 排著的派工當孤兒撤掉。
#[tokio::test]
async fn a_queued_dispatch_survives_a_sweep_in_the_middle_of_a_child_restart() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let kid_pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
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
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, adopted, started_at)
         VALUES (?,?,'running','idle',?,?,?,'proj-alfa-ui','test',1,?)",
    )
    .bind(db::ulid())
    .bind(&kid)
    .bind(&ws.workspace_id)
    .bind(&kid_pane.tab_id)
    .bind(&kid_pane.pane_id)
    .bind(db::now())
    .execute(app.db())
    .await
    .unwrap();
    // 有名字的 agent：mock 收到 ctrl+c 就讓它離開，`agent.start` 再把它放回來。
    *env.herdr.agents.lock().unwrap() = vec![json!({
        "name": "proj-alfa-ui", "agent": "claude", "agent_status": "idle",
        "workspace_id": ws.workspace_id, "tab_id": kid_pane.tab_id, "pane_id": kid_pane.pane_id,
        "cwd": "/tmp/p"})];
    let conv = db::conversation_id(app.db(), &kid).await.unwrap();
    let queued = db::ulid();
    sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,'web','queued','pending','派工',?)")
        .bind(&queued)
        .bind(&conv)
        .bind(db::now())
        .execute(app.db())
        .await
        .unwrap();

    let swept = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (app2, swept2) = (app.clone(), swept.clone());
    super::super::race_point::arm("child_restart_between_runs", &kid, move || async move {
        revoke_all_orphaned_queued_turns(&app2).await;
        swept2.store(true, std::sync::atomic::Ordering::SeqCst);
    });

    restart_child_in_pane(&app, &kid).await.expect("子 agent 在原本的 pane 裡回來了");
    assert!(swept.load(std::sync::atomic::Ordering::SeqCst), "sweeper 真的落在停與起之間");
    let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(&queued).fetch_one(app.db()).await.unwrap();
    assert_eq!(status, "queued", "重啟中的子 agent 不是孤兒：派工留給新的 run 送");
}

#[tokio::test]
async fn a_codex_child_restart_keeps_its_process_and_screen_guards() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let kid_pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
    let parent = tt::claude_bot(&app, &env.project_id, "alfa").await;
    let kid = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, model, args_json, autostart, inject_hooks, hook_token, auto_approve, managed_by, parent_bot_id, created_at)
         VALUES (?,?,'ui','codex','gpt-5.6-sol','[]',0,0,'tok',1,'child',?,?)",
    )
    .bind(&kid)
    .bind(&env.project_id)
    .bind(&parent.id)
    .bind(db::now())
    .execute(app.db())
    .await
    .unwrap();
    let agent = "proj-alfa-ui";
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, adopted, started_at)
         VALUES (?,?,'running','idle',?,?,?,?,'test',1,?)",
    )
    .bind(db::ulid())
    .bind(&kid)
    .bind(&ws.workspace_id)
    .bind(&kid_pane.tab_id)
    .bind(&kid_pane.pane_id)
    .bind(agent)
    .bind(db::now())
    .execute(app.db())
    .await
    .unwrap();
    *env.herdr.agents.lock().unwrap() = vec![json!({
        "name": agent, "agent": "codex", "agent_status": "idle",
        "workspace_id": ws.workspace_id, "tab_id": kid_pane.tab_id, "pane_id": kid_pane.pane_id,
        "cwd": "/tmp/p"})];

    restart_child_in_pane(&app, &kid).await.unwrap();

    let calls = env.herdr.calls_to("agent.start");
    let args = calls.last().unwrap()["args"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|arg| arg.as_str())
        .collect::<Vec<_>>();
    assert!(args.contains(&"--no-daemon"), "Codex must not reuse a shared app server: {args:?}");
    assert!(args.contains(&"--no-alt-screen"), "Codex must keep its screen readable: {args:?}");
    assert!(args.windows(2).any(|w| w == ["-c", "tui.show_tooltips=false"]), "Codex turn tips must stay off: {args:?}");
    assert!(args.windows(2).any(|w| w == ["-c", "allow_login_shell=false"]), "Codex tool shells must not be login shells: {args:?}");
}

/// 稽核：herdr 0.9.2+ 的閒著 codex 永遠回 `unknown`、不會再有狀態事件。原地重啟新 run 以 `unknown` 起頭，
/// 等到 ready 的結果（折成 idle）卻被丟掉，DB 就一直停在 `unknown`：一鍵重啟、閒置回收都把它當「狀態不明」跳過。
#[tokio::test]
async fn a_codex_child_restarted_in_its_pane_is_recorded_idle_once_ready() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let kid_pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
    let parent = tt::claude_bot(&app, &env.project_id, "alfa").await;
    let kid = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, auto_approve, managed_by, parent_bot_id, created_at)
         VALUES (?,?,'ui','codex','[]',0,0,'tok',1,'child',?,?)",
    )
    .bind(&kid)
    .bind(&env.project_id)
    .bind(&parent.id)
    .bind(db::now())
    .execute(app.db())
    .await
    .unwrap();
    let agent = "proj-alfa-ui";
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, adopted, started_at)
         VALUES (?,?,'running','idle',?,?,?,?,'test',1,?)",
    )
    .bind(db::ulid())
    .bind(&kid)
    .bind(&ws.workspace_id)
    .bind(&kid_pane.tab_id)
    .bind(&kid_pane.pane_id)
    .bind(agent)
    .bind(db::now())
    .execute(app.db())
    .await
    .unwrap();
    *env.herdr.agents.lock().unwrap() = vec![json!({
        "name": agent, "agent": "codex", "agent_status": "unknown", "launch_pending": false,
        "workspace_id": ws.workspace_id, "tab_id": kid_pane.tab_id, "pane_id": kid_pane.pane_id,
        "cwd": "/tmp/p"})];

    let run_id = restart_child_in_pane(&app, &kid).await.unwrap();

    let status: String = sqlx::query_scalar("SELECT agent_status FROM runs WHERE id=?").bind(&run_id).fetch_one(app.db()).await.unwrap();
    assert_eq!(status, "idle", "ready 之後要記下折過的狀態，不能留在 unknown");
}
