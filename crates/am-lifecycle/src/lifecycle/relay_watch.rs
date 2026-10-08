//! agent→agent 交辦（`herdr agent prompt`，SPEC §6.5d）之後盯著收件的 pane（#380）。
//!
//! shim 送出前先 `POST /relay/announce`，daemon 只記了「誰要送什麼給誰」等 hook 回音認領；那句話若被 TUI 吞掉最後的 Enter
//! （多行文字當成貼上），停在輸入列，就不會有 hook、也不會有回合：父 bot 與 child 都是 idle，UI 什麼都看不出來，也沒人補 Enter
//! （`poller::nudge_unsent_prompt` 只認 daemon 自己送的、有 turn 的 prompt）。announce 命中一顆在跑的 bot 之後：
//!
//! 1. 立刻開一個進行中的 `external` 回合（使用者訊息帶 `relay_from`），側欄與標題列看得出在跑；收尾照既有的 hook／終端備援。
//! 2. 盯 pane：宣告的字還在輸入列、agent 仍 idle 超過 [`NUDGE_AFTER`] 才補 Enter（比對文字，使用者自己打的字不動）。
//!    最後一個副作用（agent 開始 working、回合被收掉）出現就收工；[`GIVE_UP_AFTER`] 內字既沒進輸入列也沒被收下就把回合標失敗。
//!
//! 每一步都是 [`step`]：時間由呼叫端傳進來（測試不睡覺），pane 走 app 的 herdr client（測試用 MockHerdr）。

#[cfg(all(test, feature = "daemon-test-harness"))]
use crate::capabilities::Db;
use super::*;
use super::s6_ports::RelayWatchContext;
use std::time::Instant;

/// 字留在輸入列、agent 還是 idle 多久才補 Enter：給 TUI 自己畫完、bracketed paste 收尾的時間。
pub const NUDGE_AFTER: Duration = Duration::from_secs(4);
/// 補完 Enter 之後最多再補幾次（一次沒用，多按只會打到別的東西）。
const MAX_NUDGES: u32 = 2;
/// 整段盯多久：這之後字既不在輸入列、agent 也沒接手，就當沒送達。
pub const GIVE_UP_AFTER: Duration = Duration::from_secs(90);
pub const POLL: Duration = Duration::from_secs(2);

/// 一條進行中的盯梢。
pub struct Watch {
    pub run_id: String,
    pub bot_id: String,
    pub text: String,
    pub turn_id: Option<String>,
    started: Instant,
    /// 宣告的字第一次被看到留在輸入列（agent idle）的時刻；看不到就清掉。
    held_since: Option<Instant>,
    nudges: u32,
    /// 補 Enter 前那一刻 pane 的 revision：補完之後畫面沒重繪＝鍵根本沒送到那個行程（#380 實測），不再重試。
    rev_at_nudge: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// 還要再看。
    Waiting,
    /// 補了一次 Enter。
    Nudged,
    /// 收工：agent 接手了、回合被收掉、或 run 不在了。
    Done,
    /// 送不到：回合已標失敗。
    GaveUp,
}

/// Resolve a herdr target only among runs on the sender's host and session. Pane IDs are local to
/// a herdr server/session, so an ambiguous target is ignored instead of guessed by start time.
pub async fn resolve(app: &(impl crate::capabilities::Db + crate::capabilities::HerdrRoutes), from_bot: &str, to_agent: &str) -> anyhow::Result<Option<db::Run>> {
    let to = to_agent.trim();
    if to.is_empty() {
        return Ok(None);
    }
    let host = db::bot_host(app.db(), from_bot).await?;
    let Some(fallback_session) = app.session_for_host(&host).await else { return Ok(None) };
    let sender_run = db::active_run(app.db(), from_bot).await?;
    let session = sender_run
        .as_ref()
        .and_then(|run| run.herdr_session.as_deref())
        .filter(|session| !session.trim().is_empty())
        .unwrap_or(&fallback_session);
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT r.id FROM runs r JOIN bots b ON b.id = r.bot_id JOIN projects p ON p.id = b.project_id
          WHERE r.state = 'running' AND b.deleted_at IS NULL AND p.host = ?1
            AND COALESCE(r.herdr_session, ?2) = ?3 AND (r.agent_name = ?4 OR r.pane_id = ?4)
          LIMIT 2",
    )
    .bind(&host)
    .bind(&fallback_session)
    .bind(session)
    .bind(to)
    .fetch_all(app.db())
    .await?;
    if ids.len() != 1 {
        return Ok(None);
    }
    let id = &ids[0];
    db::run(app.db(), id).await
}

/// 開進行中的回合。已經有回合在飛（收件方正忙，字會排在它後面）就不開，UI 本來就看得到。寫不進去只記 log，不擋補 Enter。
pub(super) async fn open_turn(app: &impl RelayWatchContext, run: &db::Run, from_bot: &str, text: &str) -> Option<String> {
    match try_open_turn(app, run, from_bot, text).await {
        Ok(tid) => tid,
        Err(e) => {
            tracing::warn!(error = ?e, run = %run.id, "agent-to-agent prompt: could not open a turn");
            None
        }
    }
}

async fn try_open_turn(app: &impl RelayWatchContext, run: &db::Run, from_bot: &str, text: &str) -> anyhow::Result<Option<String>> {
    let lock = app.bot_lock(&run.bot_id).await;
    let _g = lock.lock().await;
    let Some(run) = db::run(app.db(), &run.id).await? else { return Ok(None) };
    if run.state != "running" {
        return Ok(None);
    }
    // 沒有 hook 的 grok 由它的對話檔記回合（§12.5）：slash 指令不是對話；收件方忙著時這句先記下，等對話檔出現它再收進自己的回合。
    let transcript = run.adopted != 0 && db::bot(app.db(), &run.bot_id).await?.is_some_and(|b| matches!(b.kind.as_str(), "grok" | "agy") && b.inject_hooks == 0);
    if transcript && super::grok_transcript::is_slash_command(text) {
        return Ok(None);
    }
    let conv = db::conversation_id(app.db(), &run.bot_id).await?;
    if db::in_flight_turn(app.db(), &run.id).await?.is_some() {
        if transcript {
            let mut tx = app.db().begin().await?;
            let msg = insert_message_relayed_tx(&mut tx, &conv, None, "user", text, "hook", false, None, Some(from_bot)).await?;
            tx.commit().await?;
            super::messages::emit_message_added(app, &run.bot_id, msg).await;
        }
        return Ok(None);
    }
    let tid = db::ulid();
    let mut tx = app.db().begin().await?;
    sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'external','in_flight','ok',?)")
        .bind(&tid)
        .bind(&conv)
        .bind(&run.id)
        .bind(db::now())
        .execute(&mut *tx)
        .await?;
    let msg = insert_message_relayed_tx(&mut tx, &conv, Some(&tid), "user", text, "hook", false, None, Some(from_bot)).await?;
    tx.commit().await?;
    super::messages::emit_message_added(app, &run.bot_id, msg).await;
    app.turn_changed(&tid).await;
    app.arm_progress(&run.id, &run.bot_id, &tid).await;
    tracing::info!(run = %run.id, turn = %tid, from = from_bot, "agent-to-agent prompt: opened an in-flight turn");
    Ok(Some(tid))
}

/// 看一次。`now` 由呼叫端給。
pub async fn step(app: &impl RelayWatchContext, w: &mut Watch, now: Instant) -> Step {
    let run = match db::run(app.db(), &w.run_id).await {
        Ok(Some(run)) => run,
        Ok(None) => return Step::Done,
        Err(error) => {
            tracing::warn!(%error, run = %w.run_id, "relay watch could not read run; retrying next poll");
            return Step::Waiting;
        }
    };
    if run.state != "running" {
        return Step::Done;
    }
    if let Some(t) = &w.turn_id {
        // 收掉了（hook 或終端備援）＝送達、答完了。
        match db::in_flight_turn(app.db(), &w.run_id).await {
            Ok(Some(cur)) if &cur.id == t => {}
            Ok(_) => return Step::Done,
            Err(error) => {
                tracing::warn!(%error, run = %w.run_id, turn = %t, "relay watch could not read in-flight turn; retrying next poll");
                return Step::Waiting;
            }
        }
    }
    // agent 開始做事＝字被收下了，補 Enter 只會打到別的東西。
    if run.agent_status == "working" || run.agent_status == "blocked" {
        return Step::Done;
    }
    // pane 或 agent 已經不在：不對死掉的 pane 重試；把 run 收掉，回合失敗（不是「沒送達」，是收件方沒了）。
    if super::dead_panes::pane_gone(app, &run).await {
        app.mark_run_exited(&w.run_id, "pane gone").await;
        return Step::Done;
    }
    let bot = match db::bot(app.db(), &w.bot_id).await {
        Ok(Some(bot)) => bot,
        Ok(None) => return Step::Done,
        Err(error) => {
            tracing::warn!(%error, bot = %w.bot_id, "relay watch could not read bot; retrying next poll");
            return Step::Waiting;
        }
    };
    let read = match (run.pane_id.as_deref(), client_for_run(app, &run).await) {
        (Some(pane), Ok(client)) => {
            super::delivery::read_styled(&client, pane, "visible", 80).await.ok().map(|text| (client, pane.to_string(), text))
        }
        _ => None,
    };
    let held = read.as_ref().is_some_and(|(_, _, screen)| composer_holds_prompt(&bot.kind, screen, &w.text));
    if !held {
        w.held_since = None;
        if now.duration_since(w.started) >= GIVE_UP_AFTER {
            return give_up(app, w).await;
        }
        return Step::Waiting;
    }
    let since = *w.held_since.get_or_insert(now);
    if let (Some(before), Some((client, pane, _))) = (w.rev_at_nudge, read.as_ref()) {
        // 按過 Enter 了，字還在、畫面一次都沒重繪：鍵沒送進去，再按也一樣；停手，等期限到了回報。
        if matches!(client.pane_get(pane).await, Ok(Some(p)) if p.revision == before) {
            tracing::warn!(run = %w.run_id, "Enter did not reach the pane (revision unchanged); not retrying");
            w.nudges = MAX_NUDGES;
        }
    }
    if now.duration_since(since) < NUDGE_AFTER || w.nudges >= MAX_NUDGES {
        return if w.nudges >= MAX_NUDGES && now.duration_since(w.started) >= GIVE_UP_AFTER { give_up(app, w).await } else { Step::Waiting };
    }
    let Some((client, pane, _)) = read else { return Step::Waiting };
    w.rev_at_nudge = client.pane_get(&pane).await.ok().flatten().map(|p| p.revision);
    if let Err(e) = client.pane_send_keys(&pane, &["Enter"]).await {
        tracing::warn!(error = ?e, run = %w.run_id, "could not re-send Enter for a relayed prompt");
        return Step::Waiting;
    }
    w.nudges += 1;
    w.held_since = Some(now);
    tracing::warn!(run = %w.run_id, bot = %w.bot_id, "relayed prompt was still in the composer; re-sent Enter");
    Step::Nudged
}

async fn give_up(app: &impl RelayWatchContext, w: &Watch) -> Step {
    let Some(turn_id) = w.turn_id.as_deref() else { return Step::Done };
    let why = "別的 agent 交辦的這句話沒有被收下：字沒進輸入列，或補了 Enter 也沒有反應（畫面沒有重繪），agent 沒有接手，這一回合不會有回覆。";
    let res: anyhow::Result<Option<db::Message>> = async {
        let mut tx = app.db().begin().await?;
        let out = turn_controller::fail_on(&mut tx, turn_id, turn_controller::DeliveryOnFail::Failed, "交辦沒有送達").await?;
        if out != turn_controller::Outcome::Applied {
            return Ok(None);
        }
        let conv = db::conversation_id(app.db(), &w.bot_id).await?;
        let m = insert_message_tx(&mut tx, &conv, Some(turn_id), "system", why, "system", false, None).await?;
        tx.commit().await?;
        Ok(Some(m))
    }
    .await;
    match res {
        Ok(Some(m)) => {
            super::messages::emit_message_added(app, &w.bot_id, m).await;
            app.turn_changed(turn_id).await;
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(error = ?e, turn = turn_id, "could not fail an undelivered relayed turn"),
    }
    Step::GaveUp
}

/// Start the watch only after the caller has completed [`resolve`] successfully. The announce API
/// does this synchronously before acknowledging the shim, so a DB failure cannot be mistaken for
/// an unmanaged target and followed by an unobserved direct prompt.
pub async fn on_resolved_announce(app: &impl RelayWatchContext, from_bot: &str, text: &str, run: Option<db::Run>) {
    if text.trim().is_empty() {
        return;
    }
    let Some(run) = run else { return };
    if run.bot_id == from_bot {
        return;
    }
    // 收件方的 pane 早就不在了（側欄還畫成活的）：收掉 run，不開回合、不盯（字打進去也沒人會送）。
    if super::dead_panes::pane_gone(app, &run).await {
        app.sweep_dead_panes().await;
        return;
    }
    let turn_id = open_turn(app, &run, from_bot, text).await;
    spawn_watch(
        app,
        Watch { run_id: run.id.clone(), bot_id: run.bot_id.clone(), text: text.to_string(), turn_id, started: Instant::now(), held_since: None, nudges: 0, rev_at_nudge: None },
    );
}

/// Restore composer delivery watchers from the durable relay turns after startup or host reconnect.
pub async fn rearm_host(app: &impl RelayWatchContext, host: &str) {
    match restore_open_watches(app, host).await {
        Ok(watches) => {
            for watch in watches {
                spawn_watch(app, watch);
            }
        }
        Err(error) => tracing::warn!(%error, host, "could not restore open relay delivery watchers"),
    }
}

async fn restore_open_watches(app: &impl crate::capabilities::Db, host: &str) -> anyhow::Result<Vec<Watch>> {
    let rows: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT t.id, r.id, r.bot_id, m.content
           FROM turns t
           JOIN runs r ON r.id = t.run_id
           JOIN bots b ON b.id = r.bot_id
           JOIN projects p ON p.id = b.project_id
           JOIN messages m ON m.turn_id = t.id AND m.role = 'user' AND m.relay_from IS NOT NULL
          WHERE t.origin = 'external' AND t.status = 'in_flight'
            AND r.state = 'running' AND b.deleted_at IS NULL AND p.host = ?1",
    )
    .bind(host)
    .fetch_all(app.db())
    .await?;
    Ok(rows
        .into_iter()
        .map(|(turn_id, run_id, bot_id, text)| Watch {
            run_id,
            bot_id,
            text,
            turn_id: Some(turn_id),
            started: Instant::now(),
            held_since: None,
            nudges: 0,
            rev_at_nudge: None,
        })
        .collect())
}

fn spawn_watch(app: &impl RelayWatchContext, watch: Watch) {
    app.spawn_relay_watch(watch);
}

#[cfg(all(test, feature = "daemon-test-harness"))]
mod tests {
    use super::*;
    use crate::testing as tt;

    const TEXT: &str = "請再打一次臺產，看 errorCode 的值是多少";

    struct F {
        env: tt::Env,
        bot_id: String,
        run_id: String,
        pane: String,
    }

    async fn fixture(name: &str) -> F {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, name).await;
        let run_id = tt::fake_run(&env.app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        // herdr 認得這個 pane（沒登記的 pane 在 MockHerdr 是 `pane_not_found`＝已經死了）。
        env.herdr.tabs.lock().unwrap().push(tt::MockTab { tab_id: format!("tab-{}", bot.id), workspace_id: "ws-1".into(), label: "t".into(), panes: vec![pane.clone()] });
        F { env, bot_id: bot.id, run_id, pane }
    }

    fn watch(f: &F, turn_id: Option<String>, at: Instant) -> Watch {
        Watch { run_id: f.run_id.clone(), bot_id: f.bot_id.clone(), text: TEXT.into(), turn_id, started: at, held_since: None, nudges: 0, rev_at_nudge: None }
    }

    fn keys_sent(f: &F) -> usize {
        f.env.herdr.calls.lock().unwrap().iter().filter(|(m, _)| m == "pane.send_keys").count()
    }

    async fn status_of(f: &F, turn: &str) -> String {
        sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(turn).fetch_one(f.env.app.db()).await.unwrap()
    }

    /// 名字對得到在跑的 bot：開進行中的回合，使用者訊息標 `relay_from`；已經有回合在飛就不重開。
    #[tokio::test]
    async fn an_announce_opens_an_in_flight_turn_marked_with_its_sender() {
        let f = fixture("insurer-mt").await;
        let app = f.env.app.clone();
        let sender = tt::claude_bot(&app, &f.env.project_id, "console-fetures").await;
        let run = db::run(app.db(), &f.run_id).await.unwrap().unwrap();

        let tid = open_turn(&app, &run, &sender.id, TEXT).await.expect("開了回合");
        assert_eq!(status_of(&f, &tid).await, "in_flight");
        let (content, from): (String, Option<String>) =
            sqlx::query_as("SELECT content, relay_from FROM messages WHERE turn_id=? AND role='user'").bind(&tid).fetch_one(app.db()).await.unwrap();
        assert_eq!((content.as_str(), from.as_deref()), (TEXT, Some(sender.id.as_str())));
        assert!(open_turn(&app, &run, &sender.id, "第二句").await.is_none(), "已經有回合在飛：不重開");

        assert_eq!(resolve(&app, &sender.id, "agent").await.unwrap().map(|r| r.id), Some(f.run_id.clone()), "agent 名對得到");
        assert_eq!(resolve(&app, &sender.id, &f.pane).await.unwrap().map(|r| r.id), Some(f.run_id.clone()), "pane id 對得到");
        assert!(resolve(&app, &sender.id, "沒有這個人").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_relay_watch_is_tracked_and_stops_at_graceful_shutdown() {
        let f = fixture("tracked-relay-watch").await;
        let app = f.env.app.clone();
        let sender = tt::claude_bot(&app, &f.env.project_id, "tracked-relay-sender").await;
        let run = db::run(app.db(), &f.run_id).await.unwrap().unwrap();

        on_resolved_announce(&app, &sender.id, TEXT, Some(run)).await;
        assert_eq!(app.background_tasks.len(), 1, "the delivery watcher must belong to the daemon shutdown barrier");

        app.shutdown.cancel();
        app.background_tasks.close();
        tokio::time::timeout(Duration::from_secs(1), app.background_tasks.wait())
            .await
            .expect("a relay watcher must leave promptly at shutdown");
        assert_eq!(app.background_tasks.len(), 0);
    }

    #[tokio::test]
    async fn an_open_external_turn_rearms_its_composer_watch_after_restart() {
        let f = fixture("rearmed-relay-watch").await;
        let app = f.env.app.clone();
        let sender = tt::claude_bot(&app, &f.env.project_id, "rearmed-relay-sender").await;
        let run = db::run(app.db(), &f.run_id).await.unwrap().unwrap();
        let turn_id = open_turn(&app, &run, &sender.id, TEXT).await.expect("persist the external turn before shutdown");
        f.env.herdr.live_pane(&f.pane, tt::LivePane { composer: vec![TEXT.into()], width: Some(120), ..Default::default() });

        let mut restored = restore_open_watches(&app, "local").await.unwrap();
        assert_eq!(restored.len(), 1, "startup must reconstruct the open external turn from SQLite");
        let mut watch = restored.pop().unwrap();
        assert_eq!(watch.turn_id.as_deref(), Some(turn_id.as_str()));
        let t0 = Instant::now();
        assert_eq!(step(&app, &mut watch, t0).await, Step::Waiting, "first observe the restored composer");
        assert_eq!(step(&app, &mut watch, t0 + NUDGE_AFTER).await, Step::Nudged, "a prompt left in the composer must still receive Enter");
        assert_eq!(keys_sent(&f), 1);
    }

    #[tokio::test]
    async fn resolve_uses_the_sender_host_and_session_and_rejects_bot_names() {
        let f = fixture("local-target").await;
        let app = f.env.app.clone();
        let sender = tt::claude_bot(&app, &f.env.project_id, "relay-sender").await;
        tt::fake_run(&app, &sender.id).await;

        let remote_project = db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, ?, 'remote', 'mac2', ?)")
            .bind(&remote_project)
            .bind(format!("{}/remote", f.env.dir.display()))
            .bind(db::now())
            .execute(app.db())
            .await
            .unwrap();
        let remote = tt::claude_bot(&app, &remote_project, "remote-collision").await;
        let remote_run = tt::fake_run(&app, &remote.id).await;
        sqlx::query("UPDATE runs SET pane_id = ?, started_at = '9999' WHERE id = ?")
            .bind(&f.pane)
            .bind(&remote_run)
            .execute(app.db())
            .await
            .unwrap();

        let other_session = tt::claude_bot(&app, &f.env.project_id, "session-collision").await;
        let other_session_run = tt::fake_run(&app, &other_session.id).await;
        sqlx::query("UPDATE runs SET pane_id = ?, herdr_session = 'default', started_at = '9998' WHERE id = ?")
            .bind(&f.pane)
            .bind(&other_session_run)
            .execute(app.db())
            .await
            .unwrap();

        assert_eq!(resolve(&app, &sender.id, &f.pane).await.unwrap().map(|r| r.id), Some(f.run_id.clone()));
        assert!(resolve(&app, &sender.id, "local-target").await.unwrap().is_none(), "herdr does not resolve a bot's DB name");
    }

    #[tokio::test]
    async fn resolve_returns_none_when_multiple_runs_match_in_the_sender_scope() {
        let f = fixture("ambiguous-target").await;
        let app = f.env.app.clone();
        let sender = tt::claude_bot(&app, &f.env.project_id, "ambiguity-sender").await;
        tt::fake_run(&app, &sender.id).await;
        let second = tt::claude_bot(&app, &f.env.project_id, "ambiguous-second").await;
        let second_run = tt::fake_run(&app, &second.id).await;
        sqlx::query("UPDATE runs SET pane_id = ?, started_at = '9999' WHERE id = ?")
            .bind(&f.pane)
            .bind(&second_run)
            .execute(app.db())
            .await
            .unwrap();

        assert!(resolve(&app, &sender.id, &f.pane).await.unwrap().is_none(), "do not choose by started_at when multiple recipients match");
    }

    /// 2026-09-21 實機：字卡在輸入列、agent idle。超過 N 秒補一次 Enter，字進 transcript；沒到 N 秒不動。
    #[tokio::test]
    async fn a_prompt_left_in_the_composer_gets_its_enter_after_the_delay() {
        let f = fixture("stuck-enter").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane(&f.pane, tt::LivePane { composer: vec![TEXT.into()], width: Some(120), ..Default::default() });
        let t0 = Instant::now();
        let mut w = watch(&f, None, t0);

        assert_eq!(step(&app, &mut w, t0).await, Step::Waiting, "剛看到，先給 TUI 一點時間");
        assert_eq!(step(&app, &mut w, t0 + NUDGE_AFTER - Duration::from_millis(1)).await, Step::Waiting);
        assert_eq!(keys_sent(&f), 0);
        assert_eq!(step(&app, &mut w, t0 + NUDGE_AFTER).await, Step::Nudged);
        assert_eq!(keys_sent(&f), 1);
        assert_eq!(f.env.herdr.pane(&f.pane).unwrap().transcript.len(), 1, "字送出去了");
    }

    /// 使用者自己在 child 輸入列打的字不是宣告的那句：一個鍵都不按。
    #[tokio::test]
    async fn what_the_user_typed_is_never_submitted() {
        let f = fixture("user-typing").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane(&f.pane, tt::LivePane { composer: vec!["我自己正在打的另一段草稿文字".into()], width: Some(120), ..Default::default() });
        let t0 = Instant::now();
        let mut w = watch(&f, None, t0);
        for s in [0, 5, 30] {
            assert_eq!(step(&app, &mut w, t0 + Duration::from_secs(s)).await, Step::Waiting);
        }
        assert_eq!(keys_sent(&f), 0);
    }

    /// Enter 被吃掉、畫面一次都沒重繪（revision 沒變＝鍵沒送到，#380 實測）：只按一次就停手，不重試到天荒地老；
    /// agent 開始 working 就收工。
    #[tokio::test]
    async fn an_enter_that_never_redraws_the_pane_is_not_retried_and_the_watch_ends_when_the_agent_works() {
        let f = fixture("swallowed").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane(&f.pane, tt::LivePane { composer: vec![TEXT.into()], swallow_enter: true, width: Some(120), ..Default::default() });
        let t0 = Instant::now();
        let mut w = watch(&f, None, t0);
        let mut at = t0;
        assert_eq!(step(&app, &mut w, at).await, Step::Waiting);
        at += NUDGE_AFTER;
        assert_eq!(step(&app, &mut w, at).await, Step::Nudged);
        for _ in 0..3 {
            at += NUDGE_AFTER;
            assert_eq!(step(&app, &mut w, at).await, Step::Waiting, "revision 沒變：不再按");
        }
        assert_eq!(keys_sent(&f), 1);

        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&f.run_id).execute(app.db()).await.unwrap();
        assert_eq!(step(&app, &mut w, at).await, Step::Done);
    }

    /// 字既沒進輸入列、agent 也沒接手：超過期限把回合標失敗並說明；agent 接手（回合被收）則收工。
    #[tokio::test]
    async fn an_undelivered_prompt_fails_its_turn_and_a_closed_turn_ends_the_watch() {
        let f = fixture("never-arrived").await;
        let app = f.env.app.clone();
        let sender = tt::claude_bot(&app, &f.env.project_id, "sender").await;
        let run = db::run(app.db(), &f.run_id).await.unwrap().unwrap();
        f.env.herdr.live_pane(&f.pane, tt::LivePane { width: Some(120), ..Default::default() });
        let tid = open_turn(&app, &run, &sender.id, TEXT).await.unwrap();
        let t0 = Instant::now();
        let mut w = watch(&f, Some(tid.clone()), t0);

        assert_eq!(step(&app, &mut w, t0 + GIVE_UP_AFTER - Duration::from_secs(1)).await, Step::Waiting);
        assert_eq!(status_of(&f, &tid).await, "in_flight");
        assert_eq!(step(&app, &mut w, t0 + GIVE_UP_AFTER).await, Step::GaveUp);
        assert_eq!(status_of(&f, &tid).await, "failed");
        let notes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='system'").bind(&tid).fetch_one(app.db()).await.unwrap();
        assert_eq!(notes, 1);
        assert_eq!(step(&app, &mut w, t0 + GIVE_UP_AFTER).await, Step::Done, "回合已經收了");
    }

    fn close_pane(f: &F) {
        f.env.herdr.tabs.lock().unwrap().clear();
    }

    async fn run_state(f: &F) -> String {
        sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(&f.run_id).fetch_one(f.env.app.db()).await.unwrap()
    }

    /// #380 實測：pane 早就 `pane_not_found`，run 卻還是 running。盯梢看到就收掉 run，一個鍵都不按。
    #[tokio::test]
    async fn a_watch_never_retries_against_a_pane_that_no_longer_exists() {
        let f = fixture("dead-child").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane(&f.pane, tt::LivePane { composer: vec![TEXT.into()], width: Some(120), ..Default::default() });
        close_pane(&f);
        let t0 = Instant::now();
        let mut w = watch(&f, None, t0);
        assert_eq!(step(&app, &mut w, t0 + NUDGE_AFTER * 2).await, Step::Done);
        assert_eq!(keys_sent(&f), 0, "死掉的 pane 不重試");
        assert_eq!(run_state(&f).await, "exited", "run 收掉，側欄不再畫成活的");
    }

    /// 一次暫時 DB read error 代表不知道，不是交辦已結束；DB 恢復後同一個 watcher 還要補 Enter。
    #[tokio::test]
    async fn a_transient_run_read_error_keeps_the_watch_alive() {
        let f = fixture("run-read-retry").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane(&f.pane, tt::LivePane { composer: vec![TEXT.into()], width: Some(120), ..Default::default() });
        let t0 = Instant::now();
        let mut w = watch(&f, None, t0);

        tt::make_table_unreadable(&app, "runs").await;
        assert_eq!(step(&app, &mut w, t0 + NUDGE_AFTER).await, Step::Waiting, "讀取錯誤不能結束 watcher");
        assert_eq!(keys_sent(&f), 0, "未知狀態時不可按鍵");
        tt::make_table_readable(&app, "runs").await;

        assert_eq!(step(&app, &mut w, t0 + NUDGE_AFTER).await, Step::Waiting, "恢復後先確認 composer");
        assert_eq!(step(&app, &mut w, t0 + NUDGE_AFTER * 2).await, Step::Nudged, "持續看到 composer 超過 NUDGE_AFTER 後補 Enter");
        assert_eq!(keys_sent(&f), 1);
    }

    /// in_flight_turn 讀取失敗也不能被當成「原回合已收掉」。
    #[tokio::test]
    async fn a_transient_in_flight_turn_read_error_keeps_the_watch_alive() {
        let f = fixture("turn-read-retry").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane(&f.pane, tt::LivePane { composer: vec![TEXT.into()], width: Some(120), ..Default::default() });
        let sender = tt::claude_bot(&app, &f.env.project_id, "turn-read-sender").await;
        let run = db::run(app.db(), &f.run_id).await.unwrap().unwrap();
        let turn_id = open_turn(&app, &run, &sender.id, TEXT).await.unwrap();
        let t0 = Instant::now();
        let mut w = watch(&f, Some(turn_id), t0);

        tt::make_table_unreadable(&app, "turns").await;
        assert_eq!(step(&app, &mut w, t0 + NUDGE_AFTER).await, Step::Waiting, "讀取錯誤不能被當成回合已收掉");
        assert_eq!(keys_sent(&f), 0, "未知狀態時不可按鍵");
        tt::make_table_readable(&app, "turns").await;

        assert_eq!(step(&app, &mut w, t0 + NUDGE_AFTER).await, Step::Waiting, "恢復後先確認原回合仍在飛");
        assert_eq!(step(&app, &mut w, t0 + NUDGE_AFTER * 2).await, Step::Nudged, "持續看到 composer 超過 NUDGE_AFTER 後補 Enter");
        assert_eq!(keys_sent(&f), 1);
    }

    /// bot row 讀不到不是 bot 已刪除；恢復後要繼續看同一個 composer。
    #[tokio::test]
    async fn a_transient_bot_read_error_keeps_the_watch_alive() {
        let f = fixture("bot-read-retry").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane(&f.pane, tt::LivePane { composer: vec![TEXT.into()], width: Some(120), ..Default::default() });
        let t0 = Instant::now();
        let mut w = watch(&f, None, t0);

        tt::make_table_unreadable(&app, "bots").await;
        assert_eq!(step(&app, &mut w, t0 + NUDGE_AFTER).await, Step::Waiting, "讀取錯誤不能結束 watcher");
        assert_eq!(keys_sent(&f), 0, "未知狀態時不可按鍵");
        tt::make_table_readable(&app, "bots").await;

        assert_eq!(step(&app, &mut w, t0 + NUDGE_AFTER).await, Step::Waiting, "恢復後先確認 composer");
        assert_eq!(step(&app, &mut w, t0 + NUDGE_AFTER * 2).await, Step::Nudged, "持續看到 composer 超過 NUDGE_AFTER 後補 Enter");
        assert_eq!(keys_sent(&f), 1);
    }

    /// 定時掃描：pane 明確不在的 running run 收成 exited；pane 還在的、herdr 維護中的都不動。
    #[tokio::test]
    async fn the_sweep_ends_only_runs_whose_pane_herdr_says_is_gone() {
        let alive = fixture("alive-child").await;
        assert!(super::super::dead_panes::sweep(&alive.env.app).await.is_empty(), "pane 還在：不動");
        assert_eq!(run_state(&alive).await, "running");

        close_pane(&alive);
        assert_eq!(super::super::dead_panes::sweep(&alive.env.app).await, vec![alive.run_id.clone()]);
        assert_eq!(run_state(&alive).await, "exited");
    }

    /// 舊 pane 消失後 herdr 把同一個 id 交給別的 agent：不能再對那個 id 打字，run 要收成 exited。
    #[tokio::test]
    async fn a_reused_pane_id_held_by_another_agent_is_not_the_old_run() {
        let f = fixture("reused-pane").await;
        f.env.herdr.set_agent("someone-else", &f.pane, true);
        let typed = super::super::send_text(&f.env.app, &f.bot_id, "hello", false, None).await;
        assert!(typed.is_err(), "打進被別人佔走的 pane id：{typed:?}");
        assert!(f.env.herdr.calls_to("pane.send_text").is_empty(), "一個字都不能進新 pane");
        assert_eq!(super::super::dead_panes::sweep(&f.env.app).await, vec![f.run_id.clone()]);
        assert_eq!(run_state(&f).await, "exited");
    }

    /// #878：herdr 的 `agent start` 逾時後把名字從 pane 上拿掉，pane 與 CLI 還活著。沒有名字的 agent 不是「別人」，
    /// 定時掃描不能把這顆 run 收成 `pane gone`，打字也不能被當成打進別人的 pane。
    #[tokio::test]
    async fn a_pane_whose_agent_lost_its_herdr_name_is_still_the_run() {
        let f = fixture("nameless-pane").await;
        f.env.herdr.set_unnamed_agent(&f.pane, None);
        assert!(super::super::dead_panes::sweep(&f.env.app).await.is_empty(), "只是沒有名字：pane 還在、不收");
        assert_eq!(run_state(&f).await, "running");
        let run = db::run(&f.env.app.db, &f.run_id).await.unwrap().unwrap();
        let client = f.env.app.herdr_for_run(&run).await.unwrap();
        assert!(!super::super::dead_panes::pane_target_stale(&client, &run).await, "沒有名字的 agent 不算佔走 pane id");
    }

    /// 沒有名字時兩邊的 session 都知道、而且對不上，才是別的 agent 佔走了這個 id。
    #[tokio::test]
    async fn a_nameless_agent_with_another_session_is_not_the_run() {
        let f = fixture("nameless-other-session").await;
        sqlx::query("UPDATE runs SET native_session_id = 'sess-ours' WHERE id = ?").bind(&f.run_id).execute(&f.env.app.db).await.unwrap();
        f.env.herdr.set_unnamed_agent(&f.pane, Some("sess-theirs"));
        assert_eq!(super::super::dead_panes::sweep(&f.env.app).await, vec![f.run_id.clone()]);

        let same = fixture("nameless-same-session").await;
        sqlx::query("UPDATE runs SET native_session_id = 'sess-ours' WHERE id = ?").bind(&same.run_id).execute(&same.env.app.db).await.unwrap();
        same.env.herdr.set_unnamed_agent(&same.pane, Some("sess-ours"));
        assert!(super::super::dead_panes::sweep(&same.env.app).await.is_empty(), "同一個 session：還是我");
    }

    /// announce 給一顆 pane 已經不在的收件方：收掉 run、不開回合。
    #[tokio::test]
    async fn an_announce_to_a_dead_pane_opens_no_turn_and_ends_the_run() {
        let f = fixture("dead-target").await;
        let app = f.env.app.clone();
        let sender = tt::claude_bot(&app, &f.env.project_id, "dead-sender").await;
        close_pane(&f);
        let run = resolve(&app, &sender.id, "agent").await.unwrap();
        on_resolved_announce(&app, &sender.id, TEXT, run).await;
        assert_eq!(run_state(&f).await, "exited");
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE run_id=?").bind(&f.run_id).fetch_one(app.db()).await.unwrap();
        assert_eq!(turns, 0);
    }
}
