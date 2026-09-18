//! Stopping, interrupting and aborting: the ways a run or a turn ends on purpose.

use super::*;

pub async fn stop_bot(app: &Arc<App>, bot_id: &str) -> LcResult<bool> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    stop_bot_locked(app, bot_id).await
}

/// Is this run sitting in the user's own herdr `default` session (SPEC §6.5.1)?
pub fn in_default_session(run: &db::Run) -> bool {
    run.herdr_session.as_deref() == Some("default")
}

/// SPEC §6.5.1: a bot from the user's `default` session is observed, never driven — start
/// would create a workspace in their session, restart would close their pane (review 2026-09-12 #4).
pub(crate) fn refuse_default_session(bot: &db::Bot) -> LcResult<()> {
    if bot.herdr_session.as_deref() == Some("default") {
        return Err(LcError::conflict(
            "default_session",
            json!({"bot_id": bot.id,
                   "message": "這顆是從你自己的 herdr default session 匯入的，daemon 只觀察、不替它開或關 pane：要重啟請在那個終端裡自己做。"}),
        ));
    }
    Ok(())
}

/// [`stop_bot`] with the lock already held, so a restart stops and starts under one guard.
pub async fn stop_bot_locked(app: &Arc<App>, bot_id: &str) -> LcResult<bool> {
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let Some(run) = db::active_run(&app.db, bot_id).await.map_err(up)? else { return Ok(false) };
    let host = db::bot_host(&app.db, bot_id).await.map_err(up)?;
    let client = client_for_run(app, &run).await?;

    let _ = sqlx::query("UPDATE runs SET state='stopping' WHERE id=?").bind(&run.id).execute(&app.db).await;
    app.emit_bot_status(bot_id).await;
    fail_in_flight(app, &run.id, "run stopped by user").await;

    let target = db::run_target(&run, &bot);
    for _ in 0..2 {
        let _ = client.agent_send_keys(&target, &["ctrl+c".to_string()]).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let mut gone = false;
    for _ in 0..20 {
        let agent = client.agent_get(&target).await;
        let pane = match run.pane_id.as_deref() {
            Some(p) => client.pane_get(p).await.ok().flatten(),
            None => None,
        };
        if matches!(agent, Ok(None)) || pane.is_none() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    // Close the Run's pane (and its tab if owned) so no bare shell lingers — except in the user's
    // `default` session (SPEC §6.5.1): only ctrl+c; closing took their terminal (review 2026-09-12 #4).
    if in_default_session(&run) {
        if !gone {
            tracing::warn!(bot = %bot.name, "agent did not exit within 10s; its pane is the user's own and is left open");
        }
    } else {
        if let Some(p) = run.pane_id.as_deref() {
            close_pane_and_tab(&client, run.workspace_id.as_deref(), run.tab_id.as_deref(), p).await;
        }
        if !gone {
            tracing::warn!(bot = %bot.name, "agent did not exit within 10s; pane closed forcibly");
        }
    }
    let _ = sqlx::query("UPDATE runs SET state='stopped', ended_at=? WHERE id=?")
        .bind(db::now())
        .bind(&run.id)
        .execute(&app.db)
        .await;
    // 停掉之後沒有人會送它排著的 queued：收掉，不留著佔名額、擋 restart safety（AGM 2026-09-16）。
    revoke_orphaned_queued_turns(app, bot_id, "bot 已被停止").await;
    super::start_send::withdraw_on_stop(app, bot_id).await;
    if let Some(p) = run.pane_id.as_deref() {
        if let Some(session) = app.session_for_run(&run).await {
            crate::events::unwatch_pane_on_session(app, &host, &session, p).await;
        }
    }
    app.emit_bot_status(bot_id).await;
    Ok(true)
}

/// stop (if running) + start. Used to make edited `model` / `args` / `identity` / `env` take effect.
pub async fn run_alive(app: &Arc<App>, run: &db::Run, bot: &db::Bot) -> bool {
    let Some(pane) = run.pane_id.as_deref() else { return false };
    let Ok(client) = client_for_run(app, run).await else { return true };
    match client.pane_get(pane).await {
        Ok(None) => return false,
        // The pane says an agent is in it: `agent.get` by name can briefly miss after a same-named
        // restart on a new pane (2026-09-11).
        Ok(Some(p)) if p.agent.is_some() => return true,
        _ => {}
    }
    !matches!(client.agent_get(&db::run_target(run, bot)).await, Ok(None))
}

/// #61: one-time purge of `bots/<id>/` for soft-deleted bots with no live run. Dirs no bot row
/// claims are left alone (rt-87's `bots-orphan-backup-2026-09-10/` is outside `bots/`).
pub async fn purge_deleted_bot_dirs(app: &Arc<App>) -> usize {
    let root = app.data_dir.join("bots");
    let Ok(entries) = std::fs::read_dir(&root) else { return 0 };
    let mut removed = 0;
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let Some(id) = entry.file_name().to_str().map(str::to_string) else { continue };
        let deleted: Option<Option<String>> = sqlx::query_scalar("SELECT deleted_at FROM bots WHERE id = ?")
            .bind(&id)
            .fetch_optional(&app.db)
            .await
            .ok()
            .flatten();
        if !matches!(deleted, Some(Some(_))) {
            continue;
        }
        if matches!(db::active_run(&app.db, &id).await, Ok(Some(_))) {
            continue;
        }
        match std::fs::remove_dir_all(entry.path()) {
            Ok(()) => removed += 1,
            Err(e) => tracing::warn!(dir = %entry.path().display(), error = %e, "could not remove a deleted bot's directory"),
        }
    }
    if removed > 0 {
        tracing::info!(removed, "removed bots/<id>/ directories left behind by deleted bots");
    }
    removed
}

pub async fn purge_bot_dir(app: &Arc<App>, bot_id: &str, host: &str) {
    if !valid_id(bot_id) {
        tracing::warn!(host, bot = %bot_id, "invalid bot id; bot config dir left in place");
        return;
    }
    if host == LOCAL_HOST {
        let Ok(dir) = app.bot_dir(bot_id) else { return };
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => tracing::info!(dir = %dir.display(), "removed bot config dir"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(dir = %dir.display(), error = %e, "could not remove bot config dir"),
        }
        return;
    }
    let Some(conn) = app.hosts.get(host).await else {
        tracing::warn!(host, bot = %bot_id, "unknown host; remote bot dir left in place");
        return;
    };
    let res = async {
        let p = remote_bot_dir(&conn, bot_id).await?;
        conn.ssh_exec(&format!("rm -rf {}\n", sh_quote(&p.dir))).await?;
        Ok::<_, anyhow::Error>(p.dir)
    }
    .await;
    match res {
        Ok(dir) => tracing::info!(host, %dir, "removed remote bot config dir"),
        Err(e) => tracing::warn!(host, bot = %bot_id, error = %format!("{e:#}"), "could not remove remote bot config dir"),
    }
}

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

pub async fn interrupt_bot(app: &Arc<App>, bot_id: &str) -> LcResult<()> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let run = db::active_run(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("run".into()))?;
    let target = db::run_target(&run, &bot);
    let client = client_for_run(app, &run).await?;
    client.agent_send_keys(&target, &["esc".to_string()]).await.map_err(up)?;
    // 先記接管，再收 in-flight：`fail_in_flight` 會 emit turn → 觸發 flush，順序反過來排隊的派工就搶進去了。
    // 同時記下被中斷的是哪一回合：它的 `StopFailure` 回聲才認得出來，新回合的失敗不會被當成回聲（#117）。
    let in_flight = db::in_flight_turn(&app.db, &run.id).await.ok().flatten();
    note_user_interrupt_of(app, &bot, &run, in_flight.as_ref()).await;
    fail_in_flight(app, &run.id, "interrupted by user").await;
    clear_restored_prompt(&client, &run, &bot).await;
    Ok(())
}

/// claude 在吐出第一個字前被 `esc` 打斷，會把 prompt 放回輸入框（2026-09-08 實測），下一則
/// 貼上會黏在後面。中斷後 composer 有字就 `ctrl+c` 清掉（有字時只清不退出）。只做 claude，失敗不報錯。
async fn clear_restored_prompt(client: &HerdrClient, run: &db::Run, bot: &db::Bot) {
    if bot.kind != "claude" {
        return;
    }
    let Some(pane) = run.pane_id.as_deref() else { return };
    tokio::time::sleep(Duration::from_millis(400)).await;
    let Ok(read) = client.pane_read(pane, "visible", 80).await else { return };
    if composer_text(&bot.kind, &read.text).is_none() {
        return;
    }
    match client.pane_send_keys(pane, &["ctrl+c"]).await {
        Ok(()) => tracing::info!(run = %run.id, "interrupt put the prompt back into the composer; cleared it"),
        Err(e) => tracing::warn!(run = %run.id, error = ?e, "could not clear the restored prompt from the composer"),
    }
}

/// 強制結束目前回合（`POST /api/bots/:id/abort`）。和 [`interrupt_bot`] 相反，先保證 DB 解開、
/// 送 `esc` 只是盡力（`keys_sent`）：in-flight 標 failed、`delivery = unknown` 也一併收（§6.3，
/// 同樣鎖輸入框）。沒有 active run 不算錯——那正是最需要這支的情況。
pub async fn abort_turns(app: &Arc<App>, bot_id: &str) -> LcResult<Value> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let run = db::active_run(&app.db, bot_id).await.map_err(up)?;

    let mut keys_sent = false;
    let mut key_error: Option<String> = None;
    let mut client: Option<HerdrClient> = None;
    if let Some(r) = run.as_ref() {
        let target = db::run_target(r, &bot);
        match client_for_run(app, r).await {
            Ok(c) => match c.agent_send_keys(&target, &["esc".to_string()]).await {
                Ok(()) => {
                    keys_sent = true;
                    client = Some(c);
                }
                Err(e) => key_error = Some(format!("{e:#}")),
            },
            Err(e) => key_error = Some(format!("{e:?}")),
        }
    }

    let mut aborted: Vec<String> = Vec::new();
    if let Some(r) = run.as_ref() {
        let in_flight = db::in_flight_turn(&app.db, &r.id).await.ok().flatten();
        if let Some(t) = &in_flight {
            aborted.push(t.id.clone());
        }
        // 強制中止也是使用者要接手：排著的派工照樣不撤，只是先讓使用者拿回輸入框（§4.4a）。
        note_user_interrupt_of(app, &bot, r, in_flight.as_ref()).await;
        fail_in_flight(app, &r.id, "回合已由使用者強制中止").await;
        if let Some(c) = client.as_ref() {
            clear_restored_prompt(c, r, &bot).await;
        }
    }
    // Unknown-delivery turns live on the bot, not a run: a stopped run can leave one behind.
    let unknown = sqlx::query_as::<_, db::Turn>(
        "SELECT t.* FROM turns t JOIN conversations c ON c.id = t.conversation_id
         WHERE c.bot_id = ? AND (t.status = 'in_flight' OR t.delivery = 'unknown')",
    )
    .bind(bot_id)
    .fetch_all(&app.db)
    .await
    .map_err(up)?;
    for t in unknown {
        if aborted.contains(&t.id) && t.delivery != "unknown" {
            continue;
        }
        // 上面那句 SELECT 撈的是「in_flight **或** delivery='unknown'」，所以這裡拿到的不一定還在飛：
        // §4.3 的備援關掉的回合（`completed_fallback`）不會動 `delivery`，所以它可以是已經收好、
        // 但 `delivery` 還停在 `unknown` 的狀態。那種的**只清掉 `unknown` 這個停車位**（它是擋住下一則
        // prompt 的東西，見 `prompt_inner` 的前置檢查），不改它的 status——回合已經收好了，
        // 把它改寫成 failed 會讓使用者看到一筆「失敗」的回合，而它其實答完了。
        // （`turn_controller` 的轉移表也沒有 `completed_fallback -> failed` 這條邊，issue #68。）
        if t.status == "in_flight" {
            super::turn_controller::fail(&app.db, &t.id, super::turn_controller::DeliveryOnFail::FailedIfUnknown, "使用者強制中止")
                .await
                .map_err(up)?;
        } else if t.delivery == "unknown" {
            sqlx::query("UPDATE turns SET delivery='failed' WHERE id=? AND delivery='unknown'")
                .bind(&t.id)
                .execute(&app.db)
                .await
                .map_err(up)?;
        }
        if !aborted.contains(&t.id) {
            let _ = insert_message(app, &t.conversation_id, Some(&t.id), "system", "回合已由使用者強制中止", "system", false, None).await;
            aborted.push(t.id.clone());
        }
        emit_turn(app, &t.id).await;
    }

    tracing::info!(bot = %bot.name, keys_sent, aborted = aborted.len(), "turn(s) force-aborted by user");
    Ok(json!({"aborted": aborted, "keys_sent": keys_sent, "key_error": key_error}))
}

/// Give a running pre one-bot-one-tab bot its own tab. A move, not a restart: herdr keeps
/// `pane_id` across `pane.move` (0.8.2), so mapping, poller and in-flight turn carry on.
/// Idempotent: re-moving a solo pane would rebuild a tab and renumber the user's tab bar.
pub async fn move_pane_to_own_tab(app: &Arc<App>, bot_id: &str) -> LcResult<()> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let run = db::active_run(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("run".into()))?;
    let pane_id = run
        .pane_id
        .clone()
        .filter(|p| !p.trim().is_empty())
        .ok_or_else(|| LcError::NotFound("pane".into()))?;
    let client = client_for_run(app, &run).await?;

    // `runs.tab_id` is NULL for old runs and stale if the user dragged the pane.
    let pane = client.pane_get(&pane_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("pane".into()))?;
    let workspace_id = pane.workspace_id.clone();
    let current_tab = pane.tab_id.clone();

    let solo = client
        .tab_list(&workspace_id)
        .await
        .map_err(up)?
        .into_iter()
        .find(|t| t.tab_id == current_tab)
        .map(|t| t.pane_count <= 1)
        .unwrap_or(false);
    let tab_id = if solo {
        current_tab
    } else {
        let (new_tab, previous) = client.pane_move_to_new_tab(&pane_id, &tab_label(&bot)).await.map_err(up)?;
        // The shared tidy-up is idempotent and the one place that decides a tab may go.
        if !previous.is_empty() && previous != new_tab {
            close_tab_if_empty(&client, &workspace_id, &previous).await;
        }
        new_tab
    };

    sqlx::query("UPDATE runs SET workspace_id = ?, tab_id = ? WHERE id = ?")
        .bind(&workspace_id)
        .bind(&tab_id)
        .bind(&run.id)
        .execute(&app.db)
        .await
        .map_err(up)?;
    app.emit_bot_status(bot_id).await;
    Ok(())
}

#[cfg(test)]
mod abort_tests {
    use super::*;

    /// `abort_turns` must work when `esc` cannot be delivered (no herdr here): the turn still leaves `in_flight`.
    #[tokio::test]
    async fn unlocks_even_when_the_keys_cannot_be_sent() {
        let dir = std::env::temp_dir().join(format!("am-abort-{}", db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = db::open(&dir.join("db.sqlite3")).await.unwrap();
        let cfg = crate::config::ConfigStore::load(dir.join("config.toml")).await.unwrap();
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
            .bind(&pid).bind("/tmp/p").bind("p").bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO bots (id, project_id, name, kind, hook_token, created_at) VALUES (?,?,?,?,?,?)")
            .bind(&bid).bind(&pid).bind("b").bind("claude").bind("tok").bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO runs (id, bot_id, state, pane_id, started_at) VALUES (?,?,'running','w1:p1',?)")
            .bind(&rid).bind(&bid).bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO conversations (id, bot_id, created_at) VALUES (?,?,?)")
            .bind(&cid).bind(&bid).bind(&now).execute(&app.db).await.unwrap();
        // Both an in-flight turn and an unknown-delivery one block the next prompt.
        let (t_flight, t_unknown) = (db::ulid(), db::ulid());
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
            .bind(&t_flight).bind(&cid).bind(&rid).bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','failed','unknown',?)")
            .bind(&t_unknown).bind(&cid).bind(&rid).bind(&now).execute(&app.db).await.unwrap();
        // §4.3 的備援關掉的回合不會動 `delivery`：這種「已經收好、但 delivery 停在 unknown」的
        // 一樣擋住下一則 prompt，也一樣要被解開——但它的 status 不可以被改寫成 failed
        // （回合其實答完了，而且 `completed_fallback -> failed` 不是合法邊，issue #68）。
        let t_fallback = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, completed_at, created_at) VALUES (?,?,?,'web','completed_fallback','unknown',?,?)")
            .bind(&t_fallback).bind(&cid).bind(&rid).bind(&now).bind(&now).execute(&app.db).await.unwrap();

        let out = abort_turns(&app, &bid).await.expect("abort must not fail just because the keys did");
        assert_eq!(out["keys_sent"], false, "no herdr behind the socket");
        let aborted = out["aborted"].as_array().unwrap();
        assert_eq!(aborted.len(), 3, "in-flight、unknown-delivery 與備援關掉的那一筆: {out}");
        let fallback: (String, String) = sqlx::query_as("SELECT status, delivery FROM turns WHERE id=?")
            .bind(&t_fallback).fetch_one(&app.db).await.unwrap();
        assert_eq!(
            fallback,
            ("completed_fallback".into(), "failed".into()),
            "只解開擋住下一則 prompt 的 unknown，不把一筆已經收好的回合改寫成 failed",
        );

        let flight: (String, String) = sqlx::query_as("SELECT status, delivery FROM turns WHERE id=?")
            .bind(&t_flight).fetch_one(&app.db).await.unwrap();
        assert_eq!(flight, ("failed".into(), "ok".into()), "in-flight turn is closed, delivery untouched");
        let unknown: (String, String) = sqlx::query_as("SELECT status, delivery FROM turns WHERE id=?")
            .bind(&t_unknown).fetch_one(&app.db).await.unwrap();
        assert_eq!(unknown, ("failed".into(), "failed".into()), "unknown delivery is resolved, not left to block");
        assert!(db::in_flight_turn(&app.db, &rid).await.unwrap().is_none(), "nothing left in flight");

        // Idempotent: nothing to abort the second time round.
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
        .execute(&app.db)
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
        .execute(&app.db)
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

        let run = db::run(&app.db, &run_id).await.unwrap().unwrap();
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
        assert_eq!(db::active_run(&app.db, &bot_id).await.unwrap().map(|r| r.id), Some(run_id.clone()), "still running");
        assert!(!env.herdr.methods().iter().any(|m| m == "agent.send_keys"), "no ctrl+c was sent");

        // With no run at all, `start` is what the sidebar button would call.
        sqlx::query("UPDATE runs SET state='stopped', ended_at=? WHERE id=?")
            .bind(db::now())
            .bind(&run_id)
            .execute(&app.db)
            .await
            .unwrap();
        assert_eq!(reason(start_bot(&app, &bot_id).await.unwrap_err()), "default_session");
        let creates = env.herdr.methods().iter().filter(|m| *m == "workspace.create").count();
        assert_eq!(creates, 1, "only the fixture's own workspace.create; the daemon made none in the user's session");
    }
}

