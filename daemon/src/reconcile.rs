//! Reconciliation (SPEC §6.5, §11.3.4). Always scoped to **one host**: pane / workspace /
//! agent ids are only unique within a host's herdr session.

use crate::config::LOCAL_HOST;
use crate::db;
use crate::state::App;
use anyhow::Result;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

#[allow(dead_code)]
pub async fn reconcile(app: &Arc<App>) -> Result<()> {
    let mut first_err = None;
    for host in app.hosts.names().await {
        if let Err(e) = reconcile_host(app, &host).await {
            tracing::error!(host = %host, error = ?e, "reconcile failed");
            if host == LOCAL_HOST && first_err.is_none() {
                first_err = Some(e);
            }
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// `autostart = true` 且沒有 active Run 的 bot 走 §6.2。`only_host` 給了就只看那一台
/// （主機剛連上時用），`None` ＝ 全部（開機時用，連不上的那些留給連上的那一刻）。
///
/// 一定要在對帳**之後**才叫：否則會把 herdr 上還活著、只是 DB 還沒認回來的那顆再開一次。
pub async fn autostart_connected(app: &Arc<App>, only_host: Option<&str>) {
    for bot in db::live_bots(&app.db).await.unwrap_or_default() {
        if bot.autostart != 1 {
            continue;
        }
        let host = db::bot_host(&app.db, &bot.id).await.unwrap_or_else(|_| crate::config::LOCAL_HOST.to_string());
        if only_host.is_some_and(|h| h != host) {
            continue;
        }
        if !app.host_connected(&host).await {
            tracing::info!(bot = %bot.name, host, "autostart skipped: host not connected");
            continue;
        }
        if db::active_run(&app.db, &bot.id).await.ok().flatten().is_some() {
            continue;
        }
        tracing::info!(bot = %bot.name, host, "autostart");
        if let Err(e) = crate::lifecycle::start_bot(app, &bot.id).await {
            tracing::error!(bot = %bot.name, error = ?e, "autostart failed");
        }
    }
}

/// A Turn that outlives a restart has no poller: no live bubble, and nothing completes it if its
/// hook never arrives. Re-arm every in-flight Turn once the runs are adopted.
pub async fn rearm_progress(app: &Arc<App>) {
    let runs = match crate::db::all_active_runs(&app.db).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = ?e, "cannot re-arm progress pollers");
            return;
        }
    };
    for run in runs {
        if let Ok(Some(turn)) = crate::db::in_flight_turn(&app.db, &run.id).await {
            tracing::info!(run = %run.id, turn = %turn.id, "re-arming progress poller after restart");
            adopt_orphan_delivery(app, &turn).await;
            crate::lifecycle::arm_progress(app, &run.id, &run.bot_id, &turn.id).await;
            // 送出後幾秒內被重啟：補 Enter 與「畫面上找不到就重送」這兩層網都只活在上一個行程裡
            // （review 2026-09-16）。只對**剛送出**的補，不然會把幾小時前的 prompt 重送一次。
            if turn.delivery == "ok" && fresh_enough(&turn.created_at) {
                crate::lifecycle::arm_stall(app, &run.id, &run.bot_id, &turn.id).await;
            }
        }
    }
    // Queued prompts in a backoff lost their timers with the old process (SPEC §4.4a).
    crate::lifecycle::rearm_queue_retries(app).await;
}

/// 重啟前正在送出的那一筆（`in_flight` 而 `delivery` 還是 `pending`）：它的收尾者只活在上一個行程的
/// 那個 async 任務裡，重啟後沒有任何人會碰它——`try_fallback` 只處理 `ok`、佇列看到 in-flight 就返回，
/// 而 AGM 的 safety 會把它讀成「daemon 正在打字」而永遠不給重啟窗口（review 2026-09-16）。
///
/// 鍵可能已經按下去了，所以不能當成沒送：標成 `unknown`（＝「按過了，證不出來」）交給既有的
/// 放棄／人工判斷那條路，UI 也才會顯示「送出狀態不明」而不是一直轉。
async fn adopt_orphan_delivery(app: &Arc<App>, turn: &db::Turn) {
    if turn.delivery != "pending" {
        return;
    }
    let n = sqlx::query("UPDATE turns SET delivery='unknown' WHERE id = ? AND status='in_flight' AND delivery='pending'")
        .bind(&turn.id)
        .execute(&app.db)
        .await
        .map(|r| r.rows_affected())
        .unwrap_or(0);
    if n > 0 {
        tracing::warn!(turn = %turn.id, "a prompt was mid-delivery when the daemon stopped; marked unknown so somebody can decide");
        crate::lifecycle::emit_turn(app, &turn.id).await;
    }
}

/// 剛送出不久才值得補上 stall watchdog：它會在 12 秒後判「畫面上完全沒有這則」並重送一次，
/// 對一筆幾小時前的 turn 那是把舊訊息又送一次，比不補更糟。
fn fresh_enough(created_at: &str) -> bool {
    const MAX_AGE_SECS: i64 = 120;
    chrono::DateTime::parse_from_rfc3339(created_at)
        .map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds() <= MAX_AGE_SECS)
        .unwrap_or(false)
}

/// A possible parent this pass. One bot, one tab: the tab makes descent observable whatever a
/// spawned agent called itself.
struct Parent {
    agent_name: String,
    tab_id: Option<String>,
    bot: db::Bot,
}

/// Length of `parent` as a `<parent>-<suffix>` prefix of `name`; 0 when it is not.
fn prefix_score(parent: &str, name: &str) -> usize {
    if name.len() > parent.len() + 1 && name.starts_with(parent) && name.as_bytes()[parent.len()] == b'-' {
        parent.len()
    } else {
        0
    }
}

/// herdr's agent name minus what `valid_bot_name` forbids, cut to 32.
fn child_name_from_agent(name: &str) -> String {
    let cleaned: String =
        name.chars().filter(|c| !c.is_whitespace() && !matches!(c, '@' | ',' | ':' | ';')).take(32).collect();
    if cleaned.is_empty() {
        "child".to_string()
    } else {
        cleaned
    }
}

pub async fn reconcile_host(app: &Arc<App>, host: &str) -> Result<()> {
    let Some(session) = app.session_for_host(host).await else {
        anyhow::bail!("unknown host `{host}`");
    };
    let Some(client) = app.herdr_for_session(host, &session).await else {
        anyhow::bail!("unknown host `{host}`");
    };
    crate::github::spawn_detect_host(app.clone(), host.to_string());
    let snapshot = client.snapshot().await?;
    // A1 的同一條規則也要套在 snapshot 上：`panes`／`workspaces` 這兩個 key 不在（不是「陣列是空的」，
    // 是「連 key 都沒有」）＝這份回應不是我們認得的形狀，下面每一段都會把它讀成「什麼都不存在」，
    // 於是整台主機的 workspace 映射被清光（review 2026-09-16）。跳過這一輪，不要清任何東西。
    for key in ["panes", "workspaces"] {
        if snapshot.get(key).is_none() {
            anyhow::bail!("session.snapshot on host `{host}` has no `{key}`; skipping reconcile so nothing is cleared");
        }
    }
    // A1: never reconcile against an empty list — a transient RPC failure would exit every Run.
    let agents = client.agent_list().await.map_err(|e| {
        anyhow::anyhow!("agent.list failed on host `{host}`: {e}; skipping reconcile so runs are not falsely exited")
    })?;
    let by_name: HashMap<String, &crate::herdr::AgentInfo> =
        agents.iter().filter_map(|a| a.name.clone().map(|n| (n, a))).collect();

    let live_ws: Vec<String> = snapshot
        .get("workspaces")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|w| w.get("workspace_id").and_then(|s| s.as_str()).map(String::from)).collect())
        .unwrap_or_default();
    let live_panes: Vec<String> = snapshot
        .get("panes")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|p| p.get("pane_id").and_then(|s| s.as_str()).map(String::from)).collect())
        .unwrap_or_default();
    let panes_with_agent: Vec<String> = snapshot
        .get("panes")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter(|p| p.get("agent").map(|g| !g.is_null()).unwrap_or(false))
                .filter_map(|p| p.get("pane_id").and_then(|s| s.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();

    for p in db::live_projects(&app.db).await?.into_iter().filter(|p| p.host == host) {
        if let Some(ws) = p.workspace_id.as_deref() {
            if !live_ws.contains(&ws.to_string()) {
                sqlx::query("UPDATE projects SET workspace_id=NULL WHERE id=?").bind(&p.id).execute(&app.db).await?;
                tracing::info!(host, project = %p.label, "workspace disappeared; mapping cleared");
            }
        }
    }

    let bots = db::live_bots_on_host(&app.db, host).await?;
    // Unclaimed agent names are strangers, candidates for someone's child (below).
    let mut claimed: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut parents: Vec<Parent> = Vec::new();
    for bot in bots {
        // Default-session bots are default_session::sync's; absent from our agent.list ≠ exited.
        if bot.herdr_session.as_deref().map(|s| s != session).unwrap_or(false) {
            continue;
        }
        let lock = app.bot_lock(&bot.id).await;
        let _g = lock.lock().await;
        let active = db::active_run(&app.db, &bot.id).await?;
        if active
            .as_ref()
            .and_then(|r| r.herdr_session.as_deref())
            .map(|s| s != session)
            .unwrap_or(false)
        {
            continue;
        }
        let computed = db::agent_name_for_bot(&app.db, &bot).await?;
        let mut candidates: Vec<String> = Vec::new();
        if let Some(r) = &active {
            if let Some(n) = r.agent_name.clone().filter(|s| !s.is_empty()) {
                candidates.push(n);
            }
        } else if bot.managed_by == "child" {
            // No live run: its last run's herdr name is still how to find it.
            if let Some(n) = sqlx::query_scalar::<_, Option<String>>(
                "SELECT agent_name FROM runs WHERE bot_id = ? ORDER BY started_at DESC LIMIT 1",
            )
            .bind(&bot.id)
            .fetch_optional(&app.db)
            .await?
            .flatten()
            .filter(|s| !s.is_empty())
            {
                candidates.push(n);
            }
        }
        // A child is only its adopted agent; the computed name would match a pane started by mistake.
        if bot.managed_by != "child" && !candidates.contains(&computed) {
            candidates.push(computed.clone());
        }
        let mut found: Option<crate::herdr::AgentInfo> = None;
        let mut found_name: Option<String> = None;
        for n in &candidates {
            if let Some(a) = by_name.get(n) {
                found = Some((*a).clone());
                found_name = Some(n.clone());
                break;
            }
        }
        // The snapshot predates this bot's lock; a Run started meanwhile would look dead. Re-check.
        if found.is_none() && active.is_some() {
            for n in &candidates {
                if let Some(a) = client.agent_get(n).await.ok().flatten() {
                    tracing::debug!(host, bot = %bot.name, agent = %n, "reconcile: agent appeared after the snapshot");
                    found = Some(a);
                    found_name = Some(n.clone());
                    break;
                }
            }
        }
        if let Some(n) = &found_name {
            claimed.insert(n.clone());
        }
        // …and the other direction (2026-09-10 23:02, AGM down 5.5 h): a stop/restart held the lock,
        // so the listed entry may be history — adopting it blocks the restart, or moves the fresh run
        // onto the closed pane. Ask herdr again. The name stays `claimed` either way.
        if let Some(listed) = found.clone() {
            let stale_possible = match &active {
                None => true,
                Some(r) => r.pane_id.as_deref() != Some(listed.pane_id.as_str()),
            };
            if stale_possible {
                let name = found_name.clone().unwrap_or_default();
                let got = client.agent_get(&name).await;
                let got_pane = got.as_ref().ok().and_then(|a| a.as_ref().map(|a| a.pane_id.clone()));
                let current = match got {
                    Ok(Some(a)) => match client.pane_get(&a.pane_id).await {
                        Ok(None) => None,
                        _ => Some(a),
                    },
                    Ok(None) => None,
                    // Cannot tell: keep what the list said.
                    Err(_) => Some(listed.clone()),
                };
                if current.as_ref().map(|a| a.pane_id.as_str()) != Some(listed.pane_id.as_str()) {
                    tracing::info!(host, bot = %bot.name, agent = %name, listed_pane = %listed.pane_id,
                        current_pane = ?current.as_ref().map(|a| a.pane_id.clone()), agent_get = ?got_pane,
                        run_pane = ?active.as_ref().and_then(|r| r.pane_id.clone()),
                        "reconcile: agent list went stale while waiting for the bot's lock; using herdr's current answer");
                }
                match (&active, current) {
                    // Adopt only what herdr confirms, on its current pane.
                    (None, current) => {
                        if current.is_none() {
                            found_name = None;
                        }
                        found = current;
                    }
                    // Pane move: the run follows it.
                    (Some(_), Some(a)) => found = Some(a),
                    // Right after a same-named restart herdr cannot confirm by name; not "gone" yet —
                    // the run's own pane decides below.
                    (Some(_), None) => {
                        found = None;
                        found_name = None;
                    }
                }
            }
        }
        // A bot only owns a tab while herdr still lists its agent.
        parents.push(Parent {
            agent_name: found_name.clone().unwrap_or_else(|| computed.clone()),
            tab_id: found.as_ref().map(|a| a.tab_id.clone()),
            bot: bot.clone(),
        });
        match (active, found.as_ref()) {
            (Some(run), Some(agent)) => {
                let agent: &crate::herdr::AgentInfo = agent;
                // Liveness is by name, not by having a tab, so old shared-tab runs (tab_id NULL) just
                // learn their tab. Status is re-asked under the lock: a stale `idle` eats the next
                // `working -> idle` edge (review 2026-09-12 a).
                let status = match client.agent_get(&agent.pane_id).await {
                    Ok(Some(fresh)) => fresh.agent_status.normalized().as_str().to_string(),
                    _ => agent.agent_status.normalized().as_str().to_string(),
                };
                // `stopping` is healed too: a give-up stop left it stuck (review 2026-09-12 #1).
                sqlx::query("UPDATE runs SET pane_id=?, workspace_id=?, tab_id=?, agent_status=?, agent_name=COALESCE(?, agent_name), herdr_session=COALESCE(herdr_session, ?), state=CASE WHEN state IN ('starting','stopping') THEN 'running' ELSE state END WHERE id=?")
                    .bind(&agent.pane_id)
                    .bind(&agent.workspace_id)
                    .bind(&agent.tab_id)
                    .bind(&status)
                    .bind(&found_name)
                    .bind(&session)
                    .bind(&run.id)
                    .execute(&app.db)
                    .await?;
                if run.pane_id.as_deref() != Some(agent.pane_id.as_str()) {
                    if let Some(old) = run.pane_id.as_deref() {
                        crate::events::unwatch_pane_on_session(app, host, &session, old).await;
                    }
                }
                crate::events::watch_pane_on_session(app, host, &session, &agent.pane_id).await;
                if bot.kind == "codex" {
                    crate::lifecycle::schedule_codex_notice_capture(app, &bot.id, &run.id);
                }
                // Hookless runs (children) only have their pane, unwatched while the daemon was down.
                crate::lifecycle::spawn_adopted_capture(app, &run.id, &bot.id);
                sync_pane_model(app, host, &client, &bot, agent).await;
                tracing::info!(host, bot = %bot.name, run = %run.id, pane = %agent.pane_id, "reconcile: kept active run");
            }
            (Some(run), None) => {
                // After a same-named restart the old agent's late exit clears the new one's name while
                // the pane still hosts it; exiting the run (2026-09-11) killed every restarted bot.
                // Keep the run and put the name back.
                if let Some(p) = run.pane_id.as_deref() {
                    // `agent.get` takes a pane id too (`name: null` once herdr cleared it).
                    let occupant = client.agent_get(p).await;
                    tracing::info!(host, bot = %bot.name, run = %run.id, pane = %p, candidates = ?candidates,
                        occupant = ?occupant.as_ref().map(|o| o.as_ref().map(|a| (a.name.clone(), a.agent.clone(), a.pane_id.clone()))).map_err(|e| e.to_string()),
                        "reconcile: run's agent not listed by name; asking its pane");
                    match occupant {
                        Ok(Some(occupant)) if occupant.name.as_deref().map_or(true, |n| candidates.iter().any(|c| c == n)) => {
                            let name = run.agent_name.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| computed.clone());
                            if occupant.name.is_none() {
                                match client.agent_rename(p, &name).await {
                                    Ok(_) => tracing::info!(host, bot = %bot.name, run = %run.id, pane = %p, agent = %name,
                                        "reconcile: herdr had lost the agent's name but its pane still hosts it; name re-applied, run kept"),
                                    Err(e) => tracing::warn!(host, bot = %bot.name, run = %run.id, pane = %p, agent = %name, error = ?e,
                                        "reconcile: herdr had lost the agent's name and would not take it back; run kept anyway"),
                                }
                            }
                            let status = occupant.agent_status.normalized().as_str().to_string();
                            sqlx::query("UPDATE runs SET agent_status=?, state=CASE WHEN state IN ('starting','stopping') THEN 'running' ELSE state END WHERE id=?")
                                .bind(&status)
                                .bind(&run.id)
                                .execute(&app.db)
                                .await?;
                            claimed.insert(name);
                            app.emit_bot_status(&bot.id).await;
                            continue;
                        }
                        // Someone else's agent, or nobody: the run's agent really is gone.
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(host, bot = %bot.name, run = %run.id, pane = %p, error = ?e,
                                "reconcile: could not ask herdr what is in the run's pane; leaving the run alone this pass");
                            continue;
                        }
                    }
                }
                tracing::info!(host, bot = %bot.name, run = %run.id, "reconcile: agent gone, marking run exited");
                crate::lifecycle::mark_run_exited(app, &run.id, "agent not found during reconcile").await;
                // A child exists only as long as its pane; its conversation is kept.
                if bot.managed_by == "child" {
                    sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?")
                        .bind(db::now())
                        .bind(&bot.id)
                        .execute(&app.db)
                        .await?;
                    app.emit("project_changed", json!({"project_id": bot.project_id})).await;
                    tracing::info!(host, bot = %bot.name, "reconcile: spawned child retired with its pane");
                }
            }
            (None, Some(agent)) => {
                // 隔離實例不收編既有 pane：它是別顆 daemon 開的，hook 仍寫著那顆的資料目錄。
                if app.isolated() {
                    tracing::error!(host, bot = %bot.name, pane = %agent.pane_id,
                        "隔離實例不認領既有 pane（hook 指向別的資料目錄）；要在這顆 daemon 底下跑就重啟這顆 bot");
                    continue;
                }
                let run_id = db::ulid();
                let status = agent.agent_status.normalized().as_str().to_string();
                sqlx::query(
                    "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, adopted, agent_name, herdr_session, started_at)
                     VALUES (?,?,'running',?,?,?,?,1,?,?,?)",
                )
                .bind(&run_id)
                .bind(&bot.id)
                .bind(&status)
                .bind(&agent.workspace_id)
                .bind(&agent.pane_id)
                .bind(&agent.tab_id)
                .bind(&found_name)
                .bind(&session)
                .bind(db::now())
                .execute(&app.db)
                .await?;
                sqlx::query("UPDATE projects SET workspace_id=? WHERE id=? AND workspace_id IS NULL")
                    .bind(&agent.workspace_id)
                    .bind(&bot.project_id)
                    .execute(&app.db)
                    .await?;
                crate::events::watch_pane_on_session(app, host, &session, &agent.pane_id).await;
                if bot.kind == "codex" {
                    crate::lifecycle::schedule_codex_notice_capture(app, &bot.id, &run_id);
                }
                crate::lifecycle::spawn_adopted_capture(app, &run_id, &bot.id);
                sync_pane_model(app, host, &client, &bot, agent).await;
                tracing::info!(host, bot = %bot.name, run = %run_id, pane = %agent.pane_id, "reconcile: adopted existing agent");
            }
            (None, None) => {
                // #60: `pane_closed` ended the child's run before we got here, so "agent gone" above
                // never sees it. A child cannot be restarted (`start_bot` refuses): retire it.
                if bot.managed_by == "child" {
                    let ended: i64 = sqlx::query_scalar(
                        "SELECT COUNT(*) FROM runs WHERE bot_id = ? AND state NOT IN ('starting','running','stopping')",
                    )
                    .bind(&bot.id)
                    .fetch_one(&app.db)
                    .await?;
                    if ended > 0 {
                        sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ? AND deleted_at IS NULL")
                            .bind(db::now())
                            .bind(&bot.id)
                            .execute(&app.db)
                            .await?;
                        app.emit("project_changed", json!({"project_id": bot.project_id})).await;
                        tracing::info!(host, bot = %bot.name, "reconcile: spawned child retired — its run had already ended and herdr no longer lists its agent");
                    }
                }
            }
        }
        app.emit_bot_status(&bot.id).await;
    }

    // Spawned children. **Descent first**: an unclaimed agent in a bot's tab is its child — the
    // `<parent>-<suffix>` naming is only a request agents forget. Prefix match covers other tabs;
    // in a tab the longest prefix wins (grandchild under child), no prefix → the tab's own bot.
    let mut new_children = 0usize;
    for agent in agents.iter() {
        let Some(name) = agent.name.as_deref() else { continue };
        if claimed.contains(name) {
            continue;
        }
        let by_tab = parents
            .iter()
            .filter(|p| p.tab_id.as_deref() == Some(agent.tab_id.as_str()))
            .max_by_key(|p| (prefix_score(&p.agent_name, name), u8::from(p.bot.managed_by != "child")));
        let by_prefix =
            parents.iter().filter(|p| prefix_score(&p.agent_name, name) > 0).max_by_key(|p| p.agent_name.len());
        let Some(entry) = by_tab.or(by_prefix) else { continue };
        let (parent_name, parent) = (&entry.agent_name, &entry.bot);
        let child_name = match prefix_score(parent_name, name) {
            0 => child_name_from_agent(name),
            n => {
                let suffix = &name[n + 1..];
                if crate::config::valid_bot_name(suffix) { suffix.to_string() } else { child_name_from_agent(name) }
            }
        };
        let kind = agent
            .agent
            .as_deref()
            .filter(|k| crate::config::valid_kind(k))
            .unwrap_or(parent.kind.as_str())
            .to_string();
        // One failed child is logged and skipped; a `?` here aborted the whole host (review 2026-09-12 #2).
        match adopt_child(app, host, &client, &session, agent, name, parent, &child_name, &kind).await {
            Ok(bot_id) => {
                claimed.insert(name.to_string());
                app.emit("bot_changed", json!({"bot_id": bot_id})).await;
                app.emit_bot_status(&bot_id).await;
                new_children += 1;
                tracing::info!(host, parent = %parent.name, child = %child_name, agent = %name, pane = %agent.pane_id,
                               "reconcile: adopted a spawned child agent");
            }
            Err(e) => {
                tracing::warn!(host, parent = %parent.name, agent = %name, pane = %agent.pane_id, error = ?e,
                               "reconcile: could not adopt a spawned child agent this pass");
            }
        }
    }
    if new_children > 0 {
        app.emit("project_changed", json!({})).await;
    }

    // Orphan panes of finished runs; the tab goes along so an emptied tab is closed too.
    let dead: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT DISTINCT r.pane_id, r.tab_id, r.workspace_id FROM runs r JOIN bots b ON b.id = r.bot_id JOIN projects p ON p.id = b.project_id
         WHERE r.pane_id IS NOT NULL AND p.host = ? AND r.state IN ('exited','stopped')
         AND COALESCE(r.herdr_session, ?) = ?
         AND r.pane_id NOT IN (
           SELECT r2.pane_id FROM runs r2 JOIN bots b2 ON b2.id = r2.bot_id JOIN projects p2 ON p2.id = b2.project_id
           WHERE r2.pane_id IS NOT NULL AND p2.host = ? AND r2.state IN ('starting','running','stopping')
           AND COALESCE(r2.herdr_session, ?) = ?)",
    )
    .bind(host)
    .bind(&session)
    .bind(&session)
    .bind(host)
    .bind(&session)
    .bind(&session)
    .fetch_all(&app.db)
    .await?;
    for (pane, tab, ws) in dead {
        if live_panes.contains(&pane) && !panes_with_agent.contains(&pane) {
            tracing::info!(host, pane_id = %pane, "reconcile: closing orphan pane");
            crate::lifecycle::close_pane_and_tab(&client, ws.as_deref(), tab.as_deref(), &pane).await;
        }
    }
    // SPEC §4.4a: adopted runs have NULL `runtime_*`; codex's status line has all three.
    fill_codex_runtime(app, host, &client).await;
    // §6.5e：非 agent 的 shell／服務 pane 收進 `panes`。與上面 agent pane 的邏輯完全分開，失敗只記 warn。
    let all_panes: Vec<serde_json::Value> =
        snapshot.get("panes").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    match crate::panes::scan_host(app, host, &all_panes).await {
        Ok(n) => tracing::debug!(host, panes = n, "scanned non-agent panes"),
        Err(e) => tracing::warn!(host, error = ?e, "non-agent pane scan failed"),
    }
    Ok(())
}

/// Returns the bot id. Lookup keyed on parent + name: name alone hit another parent's same-named
/// child (review 2026-09-12 #2). A short name already taken in the project falls back to the full
/// herdr agent name (unique); both spellings are looked up.
#[allow(clippy::too_many_arguments)]
async fn adopt_child(
    app: &Arc<App>,
    host: &str,
    client: &crate::herdr::HerdrClient,
    session: &str,
    agent: &crate::herdr::AgentInfo,
    name: &str,
    parent: &db::Bot,
    child_name: &str,
    kind: &str,
) -> anyhow::Result<String> {
    if app.isolated() {
        anyhow::bail!("隔離實例不認領既有子 agent（`{child_name}` 的 hook 指向別的資料目錄）；請在這顆 daemon 底下重開");
    }
    let now = db::now();
    let full_name = child_name_from_agent(name);
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT id FROM bots WHERE project_id = ? AND parent_bot_id = ? AND managed_by = 'child' AND deleted_at IS NULL
         AND name IN (?, ?) ORDER BY CASE WHEN name = ? THEN 0 ELSE 1 END LIMIT 1",
    )
    .bind(&parent.project_id)
    .bind(&parent.id)
    .bind(child_name)
    .bind(&full_name)
    .bind(child_name)
    .fetch_optional(&app.db)
    .await?;
    let bot_id = match existing {
        Some(id) => {
            // The per-bot loop sorts that run out first; adopting now would trip `runs_one_active`.
            if let Some(r) = db::active_run(&app.db, &id).await? {
                anyhow::bail!("child `{child_name}` still has active run `{}` under agent `{:?}`", r.id, r.agent_name);
            }
            // kind 換了就把舊 identity 丟掉（SQLite 的 SET 右邊讀的是舊列值）：身分有 kind，
            // 換成別的 CLI 之後那個身分就不適用了（`identity_kind`）。
            sqlx::query("UPDATE bots SET cwd = COALESCE(?, cwd), identity = CASE WHEN kind = ? THEN identity ELSE NULL END, kind = ? WHERE id = ?")
                .bind(agent.cwd.clone())
                .bind(kind)
                .bind(kind)
                .bind(&id)
                .execute(&app.db)
                .await?;
            id
        }
        None => {
            let taken = |n: String| {
                let db = app.db.clone();
                let project = parent.project_id.clone();
                async move {
                    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM bots WHERE project_id = ? AND name = ? AND deleted_at IS NULL")
                        .bind(&project)
                        .bind(&n)
                        .fetch_one(&db)
                        .await
                        .map(|c| c > 0)
                }
            };
            let use_name = if !taken(child_name.to_string()).await? {
                child_name.to_string()
            } else if full_name != child_name && !taken(full_name.clone()).await? {
                tracing::info!(host, parent = %parent.name, agent = %name, taken = %child_name, using = %full_name,
                               "reconcile: child's short name is already a bot in this project; using its full agent name");
                full_name.clone()
            } else {
                anyhow::bail!("both `{child_name}` and `{full_name}` are already live bots in this project");
            };
            let bot_id = db::ulid();
            // No hooks (the parent started the pane): replies come from the terminal fallback.
            sqlx::query(
                "INSERT INTO bots (id, project_id, name, kind, model, effort, fast, persona, args_json, autostart, inject_hooks, auto_approve,
                   identity, env_json, managed_by, cwd, herdr_session, parent_bot_id, hook_token, created_at)
                 VALUES (?,?,?,?,NULL,NULL,0,NULL,'[]',0,0,1,?,'{}','child',?,?,?,?,?)",
            )
            .bind(&bot_id)
            .bind(&parent.project_id)
            .bind(&use_name)
            .bind(kind)
            // 只繼承同 kind 母 bot 的身分：codex 子 agent 抄到 claude 的 cc1，quota 就長出 `codex:cc1`。
            .bind(crate::identity_kind::child_identity(parent.identity.as_deref(), &parent.kind, kind))
            .bind(agent.cwd.clone())
            .bind(session)
            .bind(&parent.id)
            .bind(db::ulid())
            .bind(&now)
            .execute(&app.db)
            .await?;
            bot_id
        }
    };
    let run_id = db::ulid();
    let status = agent.agent_status.normalized().as_str().to_string();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, adopted, agent_name, herdr_session, started_at)
         VALUES (?,?,'running',?,?,?,?,1,?,?,?)",
    )
    .bind(&run_id)
    .bind(&bot_id)
    .bind(&status)
    .bind(&agent.workspace_id)
    .bind(&agent.pane_id)
    .bind(&agent.tab_id)
    .bind(name)
    .bind(session)
    .bind(&now)
    .execute(&app.db)
    .await?;
    crate::events::watch_pane_on_session(app, host, session, &agent.pane_id).await;
    // The pane is the only source for both its conversation (§4.3) and its model (argv).
    crate::lifecycle::spawn_adopted_capture(app, &run_id, &bot_id);
    if let Ok(Some(child)) = db::bot(&app.db, &bot_id).await {
        sync_pane_model(app, host, client, &child, agent).await;
    }
    Ok(bot_id)
}

/// `runs.runtime_*` for adopted codex runs (SPEC §4.4a: no silently unapplied values; `/fast` is
/// a toggle). Only fills NULLs — a start or live apply already wrote the truth.
async fn fill_codex_runtime(app: &Arc<App>, host: &str, client: &crate::herdr::HerdrClient) {
    let rows: Vec<(String, String)> = match sqlx::query_as(
        "SELECT r.id, r.pane_id FROM runs r JOIN bots b ON b.id = r.bot_id JOIN projects p ON p.id = b.project_id
         WHERE p.host = ? AND b.kind = 'codex' AND r.state = 'running' AND r.pane_id IS NOT NULL
         AND r.runtime_model IS NULL AND r.runtime_effort IS NULL AND r.runtime_fast IS NULL",
    )
    .bind(host)
    .fetch_all(&app.db)
    .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(host, error = ?e, "codex runtime probe: query failed");
            return;
        }
    };
    for (run_id, pane_id) in rows {
        let Ok(read) = client.pane_read(&pane_id, "visible", 60).await else { continue };
        let Some(seen) = crate::codex_live::parse_status_line(&read.text) else { continue };
        let _ = sqlx::query("UPDATE runs SET runtime_model = ?, runtime_effort = ?, runtime_fast = ? WHERE id = ?")
            .bind(&seen.model)
            .bind(&seen.effort)
            .bind(i64::from(seen.fast))
            .bind(&run_id)
            .execute(&app.db)
            .await;
        if let Ok(Some(run)) = crate::db::run(&app.db, &run_id).await {
            app.emit_bot_status(&run.bot_id).await;
        }
        tracing::info!(host, run = %run_id, model = %seen.model, effort = ?seen.effort, fast = seen.fast,
                       "codex runtime read off an adopted pane's status line");
    }
}

/// A child's `model` / `effort` (argv, grok title) and `identity` (pid, [`crate::pane_identity`]).
/// * **children only**: other bots' model is user config, projected back to `config.toml`.
/// * **fills, never corrects**: argv cannot see a later `/model`. Unparsed stays NULL.
/// `identity` does correct: nobody set a child's account, the adopt copied the parent's.
async fn sync_pane_model(app: &Arc<App>, host: &str, client: &crate::herdr::HerdrClient, bot: &db::Bot, agent: &crate::herdr::AgentInfo) {
    if bot.managed_by != "child" {
        return;
    }
    let want_model = bot.model.is_none() || bot.effort.is_none();
    let want_identity = crate::pane_identity::probe_due(&bot.id, &agent.pane_id);
    if !want_model && !want_identity {
        return;
    }
    let procs = match client.pane_process_info(&agent.pane_id).await {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!(bot = %bot.name, pane = %agent.pane_id, error = %e, "pane.process_info unavailable");
            return;
        }
    };
    // Pick the process that looks like the CLI (not `git`, a pager, `caffeinate`), else the first.
    let cli = procs
        .iter()
        .find(|p| p.argv.first().map(|a| a.contains(bot.kind.as_str())).unwrap_or(false))
        .or_else(|| procs.iter().find(|p| !p.argv.is_empty()));
    if want_identity {
        crate::pane_identity::sync_child_identity(app, host, bot, &agent.pane_id, cli.and_then(|p| p.pid)).await;
    }
    if !want_model {
        return;
    }
    let argv: &[String] = cli.map(|p| p.argv.as_slice()).unwrap_or(&[]);
    let (mut model, mut effort) = crate::models::model_effort_from_argv(&bot.kind, argv);
    if bot.kind == "grok" && (model.is_none() || effort.is_none()) {
        let (tm, te) = crate::models::grok_title_model_effort(agent.terminal_title_stripped.as_deref().unwrap_or(""));
        model = model.or(tm);
        effort = effort.or(te);
    }
    // claude's argv rarely has `--effort`; resolve the account default like the "預設" hint does.
    if bot.kind == "claude" && effort.is_none() && bot.effort.is_none() {
        if let Some(alias) = model.as_deref().or(bot.model.as_deref()) {
            effort = Some(crate::models::claude_default_effort(app, host, bot.identity.as_deref(), alias).await);
        }
    }
    let model = model.filter(|_| bot.model.is_none());
    let effort = effort.filter(|_| bot.effort.is_none());
    if model.is_none() && effort.is_none() {
        return;
    }
    if let Err(e) = sqlx::query("UPDATE bots SET model = COALESCE(?, model), effort = COALESCE(?, effort) WHERE id = ?")
        .bind(&model)
        .bind(&effort)
        .bind(&bot.id)
        .execute(&app.db)
        .await
    {
        tracing::warn!(bot = %bot.name, error = ?e, "cannot record the model a child agent is running");
        return;
    }
    tracing::info!(bot = %bot.name, pane = %agent.pane_id, ?model, ?effort, "reconcile: read the child's model off its argv");
    app.emit("bot_changed", json!({"bot_id": bot.id})).await;
}

#[cfg(test)]
mod compat_tests {
    //! Old (shared-tab, `tab_id` NULL) and new bots side by side (SPEC §6.5).
    use crate::db;
    use crate::state::App;
    use crate::testing as tt;
    use serde_json::json;
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

        // 已經證出來送到的那種不要動它。
        sqlx::query("UPDATE turns SET delivery='ok' WHERE id=?").bind(&turn).execute(&app.db).await.unwrap();
        super::rearm_progress(app).await;
        let t = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn).fetch_one(&app.db).await.unwrap();
        assert_eq!(t.delivery, "ok");
    }

    /// 補 stall watchdog 只補剛送出的：對幾小時前的 turn 補，等於 12 秒後把舊訊息再送一次。
    #[test]
    fn only_a_freshly_sent_turn_gets_its_watchdog_back() {
        let now = chrono::Utc::now();
        let iso = |d: chrono::Duration| (now - d).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        assert!(super::fresh_enough(&iso(chrono::Duration::seconds(5))));
        assert!(super::fresh_enough(&iso(chrono::Duration::seconds(119))));
        assert!(!super::fresh_enough(&iso(chrono::Duration::seconds(121))));
        assert!(!super::fresh_enough(&iso(chrono::Duration::hours(3))));
        assert!(!super::fresh_enough("not-a-time"), "讀不懂時間就不要補");
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

        // The next pass finds every child under either spelling; no re-parent, no duplicate.
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
        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
        assert_eq!(run_of(&app, &bot).await.expect("reconcile 照收").adopted, 1);

        // default session：使用者自己在專案目錄裡開的 agent。收編會寫回 config.toml，所以專案要在裡面。
        let repo = env.repo.to_string_lossy().to_string();
        let (pid, path, alfa) = (env.project_id.clone(), repo.clone(), bot.clone());
        app.cfg
            .update(move |cfg| {
                let b: crate::config::BotCfg =
                    toml::from_str(&format!("id = '{alfa}'\nname = 'alfa'\nkind = 'claude'\n")).unwrap();
                cfg.projects = vec![crate::config::ProjectCfg {
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
        *env.herdr.agents.lock().unwrap() = if herdr_still_lists_the_old_pane { vec![old_entry, nameless] } else { vec![nameless] };
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

        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

        let kid = sqlx::query_as::<_, db::Bot>("SELECT * FROM bots WHERE parent_bot_id = ?")
            .bind(&parent)
            .fetch_one(&app.db)
            .await
            .expect("the child was adopted");
        assert_eq!(kid.name, "lastq");
        assert_eq!(kid.model.as_deref(), Some("opus"), "read off `--model`");
        assert_eq!(kid.effort.as_deref(), Some("high"), "read off `--effort`");
        assert_eq!(kid.inject_hooks, 0, "still no hooks: 對話 comes from the terminal");

        let p = db::bot(&app.db, &parent).await.unwrap().unwrap();
        assert_eq!(p.model, None, "a user bot's model is configuration, never scraped from its process");

        // `/model` leaves argv untouched, so argv must not win a rematch.
        sqlx::query("UPDATE bots SET model='sonnet' WHERE id=?").bind(&kid.id).execute(&app.db).await.unwrap();
        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
        assert_eq!(db::bot(&app.db, &kid.id).await.unwrap().unwrap().model.as_deref(), Some("sonnet"));
    }

    /// 2026-09-14 使用者指正：codex 子 agent 從 claude 母 bot 抄了 `cc1`，quota 就長出 `codex:cc1`。
    /// 身分有 kind，只有同 kind 的子 agent 才繼承（`identity_kind::child_identity`）。
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

    impl crate::pane_identity::ProcEnv for FakeProcEnv {
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
        ];
        for pane in [&root.pane_id, &head.pane_id, &lost.pane_id] {
            env.herdr.set_argv(pane, &["claude", "--dangerously-skip-permissions", "--model", "opus"]);
        }
        env.herdr.set_pid(&head.pane_id, 4924);
        env.herdr.set_pid(&lost.pane_id, 4925);
        // The parent's pane says `cc2` too, and must never be read: user config.
        let fake = Arc::new(FakeProcEnv {
            envs: std::collections::BTreeMap::from([
                (1, claude_env(&format!("{home}/.claude-cc2"))),
                (4924, claude_env(&format!("{home}/.claude-cc2/"))),
                (4925, claude_env("/tmp/an-account-nobody-configured")),
            ]),
            asked: std::sync::Mutex::new(Vec::new()),
        });
        app.proc_env.set(fake.clone());

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
        assert_eq!(head_bot.model.as_deref(), Some("opus"), "the same process_info still fills the model");

        // An unclaimed directory is no answer: the inherited value stays.
        assert_eq!(kid("lost").await.identity.as_deref(), Some("cc1"));

        let p = db::bot(&app.db, &parent).await.unwrap().unwrap();
        assert_eq!(p.identity.as_deref(), Some("cc1"), "a user bot's account is configuration, never scraped");

        // Once per child, never the parent: a per-pass `ps` is an ssh storm on remote hosts.
        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
        assert_eq!(*fake.asked.lock().unwrap(), vec![4924, 4925]);
        assert_eq!(kid("head").await.identity.as_deref(), Some("cc2"));
    }

    /// **Descent, not naming**: an unprefixed `helper` in the parent's tab is still its child.
    #[tokio::test]
    async fn a_stranger_in_a_bots_tab_is_claimed_as_its_child() {
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
        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

        let grand = sqlx::query_as::<_, db::Bot>("SELECT * FROM bots WHERE name = 'deep'")
            .fetch_one(&app.db)
            .await
            .expect("the grandchild was adopted");
        assert_eq!(grand.parent_bot_id.as_deref(), Some(kid.id.as_str()), "under the child, not the top bot");
    }

    /// A run whose agent herdr no longer lists still exits, tab or not.
    #[tokio::test]
    async fn a_run_whose_agent_is_gone_still_exits() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();

        let bot = a_bot(&env, "alfa").await;
        let ghost = a_bot(&env, "bravo").await;
        let live_agent = crate::config::agent_name("proj", &bot);
        for (b, pane) in [(&bot, &root.pane_id), (&ghost, &root.pane_id)] {
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
}
