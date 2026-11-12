//! One bot, one tab (and the retrofit). The mock herdr keeps real tab/pane bookkeeping but,
//! unlike herdr 0.8.2, does not reap empty tabs — so these see whether the daemon tidies up itself.
use super::*;
use crate::capabilities::Db;
use crate::testing as tt;

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
    .execute(env.app.db())
    .await
    .unwrap();
    id
}

/// 協調者（`bots.cwd` 是自己的目錄）比巡檢先開 workspace：root pane 開在專案目錄，不能拿來跑它——
/// 要在自己的目錄開 tab，再把 root 關掉（review 2026-09-16 c5 M1）。沒有自己目錄的 bot 照舊直接用 root。
#[tokio::test]
async fn a_bot_with_its_own_directory_does_not_run_in_a_fresh_workspace_root_opened_elsewhere() {
    let env = tt::env().await;
    let own = env.dir.join("AGM-responder");
    std::fs::create_dir_all(&own).unwrap();
    let own = own.to_string_lossy().to_string();
    let resp = a_bot(&env, "responder").await;
    sqlx::query("UPDATE bots SET cwd=? WHERE id=?").bind(&own).bind(&resp).execute(env.app.db()).await.unwrap();

    start_bot(&env.app, &resp).await.unwrap();
    let root = env.herdr.first_call("workspace.create").expect("the workspace did not exist yet");
    assert_ne!(root["cwd"].as_str(), Some(own.as_str()), "workspace 仍以專案目錄建立");
    let run = db::active_run(env.app.db(), &resp).await.unwrap().unwrap();
    let tab_create = env.herdr.first_call("tab.create").expect("a tab in the bot's own directory");
    assert_eq!(tab_create["cwd"].as_str(), Some(own.as_str()));
    let ws = run.workspace_id.clone().unwrap();
    let tabs = env.herdr.tabs_in(&ws);
    assert_eq!(tabs.len(), 1, "root 的 tab 關掉了，只剩協調者自己的：{tabs:?}");
    assert!(tabs[0].panes.contains(run.pane_id.as_ref().unwrap()));
    assert!(env.herdr.methods().contains(&"pane.close".into()));

    // 沒有自己目錄的 bot：新 workspace 的 root 就是它的 pane，不多開 tab。
    let env2 = tt::env().await;
    let plain = a_bot(&env2, "plain").await;
    start_bot(&env2.app, &plain).await.unwrap();
    assert!(env2.herdr.first_call("tab.create").is_none());
    assert!(!env2.herdr.methods().contains(&"pane.close".into()));
}

/// A DB failure after `workspace.create` must still remove the root pane and its tab (trigger-injected).
#[tokio::test]
async fn a_run_mapping_failure_closes_the_new_pane_and_tab() {
    let env = tt::env().await;
    let bot_id = a_bot(&env, "alfa").await;
    sqlx::query(
        "CREATE TRIGGER fail_run_mapping BEFORE UPDATE OF workspace_id, pane_id, tab_id ON runs
         BEGIN SELECT RAISE(ABORT, 'forced run mapping failure'); END",
    )
    .execute(env.app.db())
    .await
    .unwrap();

    assert!(start_bot(&env.app, &bot_id).await.is_err());

    let workspace_id = db::project(env.app.db(), &env.project_id)
        .await
        .unwrap()
        .unwrap()
        .workspace_id
        .expect("workspace.create ran before the mapping failure");
    assert!(env.herdr.tabs_in(&workspace_id).is_empty(), "the failed start left a tab behind");
    let methods = env.herdr.methods();
    assert!(methods.contains(&"pane.close".into()), "cleanup did not close the pane: {methods:?}");
    assert!(methods.contains(&"tab.close".into()), "cleanup did not close the empty tab: {methods:?}");

    let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE bot_id = ? ORDER BY started_at DESC LIMIT 1")
        .bind(&bot_id)
        .fetch_one(env.app.db())
        .await
        .unwrap();
    assert_eq!(state, "exited");
}

async fn run_row(app: &Arc<App>, id: &str) -> db::Run {
    sqlx::query_as::<_, db::Run>("SELECT * FROM runs WHERE id = ?").bind(id).fetch_one(app.db()).await.unwrap()
}

/// A run that is `running` on `pane_id`, with whatever `tab_id` the caller says.
async fn running_on(app: &Arc<App>, bot_id: &str, ws: &str, pane: &str, tab: Option<&str>) -> String {
    let id = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, agent_name, herdr_session, started_at)
         VALUES (?,?,'running','idle',?,?,?,'agent','test',?)",
    )
    .bind(&id)
    .bind(bot_id)
    .bind(ws)
    .bind(pane)
    .bind(tab)
    .bind(db::now())
    .execute(app.db())
    .await
    .unwrap();
    id
}

/// Every start pre-trusts its cwd, or claude's "trust this project?" prompt (cursor on *No*) fails it.
#[tokio::test]
async fn a_fresh_working_directory_is_trusted_before_the_agent_starts() {
    let dir = crate::testing::track(std::env::temp_dir().join(format!("am-trust-start-{}", crate::db::ulid())));
    // 工作目錄是經過符號連結進去的（macOS 的 `/tmp` 就是）；自己造一個，不靠這台機器的 tmp 剛好是不是連結
    // ——Linux 的 `/tmp` 不是，原本的 `!starts_with("/tmp/")` 在那邊必紅（#139，遠端編譯主機）。
    let repo = dir.join("real/repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
    let store = dir.join(".claude.json");
    std::fs::write(&store, "{\"numStartups\":7}").unwrap();

    let cwd = dir.join("link/repo").to_string_lossy().to_string();
    let wrote = crate::trust::mark_trusted("claude", &store, &[crate::trust::canonical(&cwd)]).unwrap();
    assert!(wrote, "a directory the CLI has not seen is recorded");

    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&store).unwrap()).unwrap();
    assert_eq!(v["numStartups"], 7, "the CLI's own state survives the merge");
    let key = crate::trust::canonical(&cwd);
    assert_eq!(v["projects"][&key]["hasTrustDialogAccepted"], true);
    // Resolved path: the CLI compares its `getcwd()`, which never contains the symlink.
    assert!(!key.contains("/link/"), "the recorded path is canonical, got {key}");
    assert_eq!(key, std::fs::canonicalize(&repo).unwrap().to_string_lossy(), "resolved to the real directory");

    assert!(!crate::trust::mark_trusted("claude", &store, &[key]).unwrap(), "already trusted: left alone");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Starting a bot creates a tab (never `pane.split`): tabs don't divide a fixed width.
#[tokio::test]
async fn a_starting_bot_gets_a_tab_of_its_own() {
    let env = tt::env().await;
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();

    let e = json!({"AM_BOT_ID": "b1", "AM_HOOK_TOKEN": "tok"});
    let first = acquire_run_pane(&client, &ws.workspace_id, "/tmp/p", "alfa", &e, None).await.unwrap();
    let second = acquire_run_pane(&client, &ws.workspace_id, "/tmp/worktree", "bravo", &e, None).await.unwrap();

    assert!(!env.herdr.methods().iter().any(|m| m == "pane.split"), "a bot is never split into a shared tab");
    assert_ne!(first.tab_id, second.tab_id, "two bots, two tabs — they do not share a width");
    assert_ne!(first.tab_id, root.tab_id, "and neither lands in the workspace's own tab");
    for t in [&first.tab_id, &second.tab_id] {
        assert_eq!(env.herdr.tab(t).unwrap().panes.len(), 1, "each tab holds exactly the one bot's pane");
    }

    // Same cwd/env contract as `pane.split`, nickname on the tab bar, never steals focus.
    let p = env.herdr.first_call("tab.create").expect("tab.create was called");
    assert_eq!(p["workspace_id"], json!(ws.workspace_id));
    assert_eq!(p["cwd"], json!("/tmp/p"));
    assert_eq!(p["label"], json!("alfa"));
    assert_eq!(p["focus"], json!(false), "starting a bot must not yank the user's focus");
    assert_eq!(p["env"]["AM_BOT_ID"], json!("b1"));
    assert_eq!(p["env"]["AM_HOOK_TOKEN"], json!("tok"));
}

/// A fresh workspace is already one tab / one pane: the first bot uses the root pane.
#[tokio::test]
async fn the_first_bot_in_a_fresh_workspace_reuses_its_root_pane() {
    let env = tt::env().await;
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();

    let got =
        acquire_run_pane(&client, &ws.workspace_id, "/tmp/p", "alfa", &json!({}), Some((root.clone(), "/tmp/p"))).await.unwrap();

    assert_eq!(got.pane_id, root.pane_id);
    assert_eq!(got.tab_id, root.tab_id);
    assert!(env.herdr.first_call("tab.create").is_none(), "no second tab for a workspace that is one already");
    assert_eq!(env.herdr.tabs_in(&ws.workspace_id).len(), 1);
}

/// Stopping a bot takes its tab too, so no row of empty tabs builds up.
#[tokio::test]
async fn stopping_a_bot_closes_the_tab_it_owned() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let pane = acquire_run_pane(&client, &ws.workspace_id, "/tmp/p", "alfa", &json!({}), None).await.unwrap();

    let bot = a_bot(&env, "alfa").await;
    running_on(&app, &bot, &ws.workspace_id, &pane.pane_id, Some(&pane.tab_id)).await;

    assert!(stop_bot(&app, &bot).await.unwrap());

    assert!(env.herdr.tab(&pane.tab_id).is_none(), "the bot's own tab went with it");
    assert_eq!(env.herdr.tabs_in(&ws.workspace_id).len(), 1, "only the workspace's own tab is left");
}

/// A pane sharing its tab only loses the pane; closing the tab would kill a neighbour's agent.
#[tokio::test]
async fn stopping_a_bot_in_a_shared_tab_leaves_the_tab_alone() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    // The old world: two bots split into the workspace's single tab.
    let neighbour = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
    let mine = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
    assert_eq!(mine.tab_id, neighbour.tab_id);

    let bot = a_bot(&env, "alfa").await;
    // Reconcile fills `tab_id` for old runs too, so "has a tab id" can't decide closing.
    running_on(&app, &bot, &ws.workspace_id, &mine.pane_id, Some(&mine.tab_id)).await;

    assert!(stop_bot(&app, &bot).await.unwrap());

    let tab = env.herdr.tab(&mine.tab_id).expect("the shared tab survives");
    assert!(!tab.panes.contains(&mine.pane_id), "our pane is gone");
    assert!(tab.panes.contains(&neighbour.pane_id), "the neighbour's agent is untouched");
}

/// Retrofit: a bot in a shared tab gets its own via a move; `pane_id` must survive.
#[tokio::test]
async fn moving_a_running_bot_gives_it_a_tab_without_changing_its_pane() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let neighbour = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
    let mine = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();

    let bot = a_bot(&env, "alfa").await;
    // NULL tab_id: a run from before the column existed.
    let run = running_on(&app, &bot, &ws.workspace_id, &mine.pane_id, None).await;

    move_pane_to_own_tab(&app, &bot).await.unwrap();

    let r = run_row(&app, &run).await;
    assert_eq!(r.pane_id.as_deref(), Some(mine.pane_id.as_str()), "a move, not a restart: same pane");
    assert_eq!(r.state, "running");
    let tab = r.tab_id.expect("the new tab was recorded on the run");
    assert_ne!(tab, mine.tab_id);
    assert_eq!(env.herdr.tab(&tab).unwrap().panes, vec![mine.pane_id.clone()], "it has the tab to itself");
    assert_eq!(env.herdr.first_call("pane.move").unwrap()["destination"]["label"], json!("alfa"));
    assert_eq!(env.herdr.first_call("pane.move").unwrap()["focus"], json!(false));
    // The tab it left still holds the neighbour, so it is not closed.
    assert!(env.herdr.tab(&neighbour.tab_id).unwrap().panes.contains(&neighbour.pane_id));
}

/// The shared tidy-up decides from herdr's pane count, not our records. "Not found" is normal in
/// production (herdr 0.8.2 reaps tabs); only the non-reaping mock reaches `tab.close`.
#[tokio::test]
async fn the_tidy_up_closes_an_empty_tab_and_only_an_empty_one() {
    let env = tt::env().await;
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let alone = acquire_run_pane(&client, &ws.workspace_id, "/tmp/p", "alfa", &json!({}), None).await.unwrap();
    let busy = acquire_run_pane(&client, &ws.workspace_id, "/tmp/p", "bravo", &json!({}), None).await.unwrap();

    // Occupied: left alone.
    close_tab_if_empty(&client, &ws.workspace_id, &busy.tab_id).await;
    assert!(env.herdr.tab(&busy.tab_id).is_some());

    // Emptied: closed.
    client.pane_close(&alone.pane_id).await.unwrap();
    close_tab_if_empty(&client, &ws.workspace_id, &alone.tab_id).await;
    assert!(env.herdr.tab(&alone.tab_id).is_none());

    // Already gone: not an error, and nothing else is touched.
    close_tab_if_empty(&client, &ws.workspace_id, &alone.tab_id).await;
    assert!(env.herdr.tab(&busy.tab_id).is_some());
}

/// Idempotence: on herdr a second move rebuilds the tab and renumbers the tab bar.
#[tokio::test]
async fn moving_a_bot_that_already_owns_its_tab_changes_nothing() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let mine = acquire_run_pane(&client, &ws.workspace_id, "/tmp/p", "alfa", &json!({}), None).await.unwrap();

    let bot = a_bot(&env, "alfa").await;
    let run = running_on(&app, &bot, &ws.workspace_id, &mine.pane_id, None).await;

    move_pane_to_own_tab(&app, &bot).await.unwrap();

    assert!(env.herdr.first_call("pane.move").is_none(), "nothing to move");
    let r = run_row(&app, &run).await;
    assert_eq!(r.tab_id.as_deref(), Some(mine.tab_id.as_str()), "the tab it already had is recorded");
    assert_eq!(env.herdr.tab(&mine.tab_id).unwrap().panes, vec![mine.pane_id]);
}

/// No active run, or a run with no pane behind it, is a 404 — not a 502 and not a panic.
#[tokio::test]
async fn moving_a_bot_that_is_not_running_is_a_not_found() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot = a_bot(&env, "alfa").await;

    assert!(matches!(move_pane_to_own_tab(&app, &bot).await, Err(LcError::NotFound(_))));

    let id = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, herdr_session, started_at)
         VALUES (?,?,'running','idle','test',?)",
    )
    .bind(&id)
    .bind(&bot)
    .bind(db::now())
    .execute(app.db())
    .await
    .unwrap();
    assert!(matches!(move_pane_to_own_tab(&app, &bot).await, Err(LcError::NotFound(_))));
}
