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

#[cfg(feature = "test-hooks")]
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

test_visible_struct! {
/// 一條進行中的盯梢。
pub struct Watch {
    pub run_id: String,
    pub bot_id: String,
    pub text: String,
    pub turn_id: Option<String>,
    started: Instant,
    /// 期限到了可不可以把回合判成「沒送達」。接回時回合已經開超過 [`GIVE_UP_AFTER`] 的不行：
    /// 重啟前它有沒有被收下，這裡已經看不出來，收尾留給 hook／終端備援／`stuck_turns`（#1202）。
    may_fail: bool,
    /// 宣告的字第一次被看到留在輸入列（agent idle）的時刻；看不到就清掉。
    held_since: Option<Instant>,
    nudges: u32,
    /// 上次補 Enter 的讀值；草稿框與位置未變就停止重試，不推論按鍵是否送達。
    snapshot_at_nudge: Option<crate::herdr::PaneRead>,
    /// 這筆回合的盯梢登記（#1366）；`Watch` 被 drop＝盯梢結束，登記跟著放掉。
    _registered: Option<Registered>,
}
}

/// 行程內「已經有一條盯梢」的回合 id。主機每次重連都會 `rearm_host` 一遍，不去重就是同一筆回合好幾條各自補 Enter（#1366）。
fn watching() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static WATCHING: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> = std::sync::OnceLock::new();
    WATCHING.get_or_init(Default::default)
}

#[cfg(any(test, feature = "test-hooks"))]
pub struct Registered(pub String);
#[cfg(not(any(test, feature = "test-hooks")))]
struct Registered(String);

impl Registered {
    /// 拿到登記＝這筆回合還沒有盯梢；已經有一條在盯就回 `None`。
    fn claim(turn_id: &str) -> Option<Self> {
        let mut set = watching().lock().unwrap_or_else(|e| e.into_inner());
        set.insert(turn_id.to_string()).then(|| Registered(turn_id.to_string()))
    }
}

impl Drop for Registered {
    fn drop(&mut self) {
        watching().lock().unwrap_or_else(|e| e.into_inner()).remove(&self.0);
    }
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

/// Resolve an explicit run ID only within the sender's host/session. Names and pane addresses
/// are never identity evidence. Handed-off projects belong to another daemon.
pub async fn resolve(app: &(impl crate::capabilities::Db + crate::capabilities::HerdrRoutes), from_bot: &str, to_run_id: &str) -> anyhow::Result<Option<db::Run>> {
    let to = to_run_id.trim();
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
          WHERE r.state = 'running' AND b.deleted_at IS NULL AND p.handed_off_to IS NULL AND p.host = ?1
            AND COALESCE(r.herdr_session, ?2) = ?3 AND r.id = ?4
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

/// #1460：只用穩定 run id 檢查目標是否已移交；結果只供 log。
pub async fn handed_off_target(app: &impl crate::capabilities::Db, from_bot: &str, to_run_id: &str) -> anyhow::Result<bool> {
    let to = to_run_id.trim();
    if to.is_empty() {
        return Ok(false);
    }
    let host = db::bot_host(app.db(), from_bot).await?;
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM runs r JOIN bots b ON b.id = r.bot_id JOIN projects p ON p.id = b.project_id
          WHERE r.state = 'running' AND b.deleted_at IS NULL AND p.handed_off_to IS NOT NULL AND p.host = ?1
            AND r.id = ?2",
    )
    .bind(&host)
    .bind(to)
    .fetch_one(app.db())
    .await?;
    Ok(n > 0)
}

/// #1460：announce 因移交而不盯梢時留一行 log（`relay_handoff_direct`），營運看得到「這句只靠 shim 直送、兩顆 daemon 都不記」。
/// 讀不到就不記，不影響送出。
pub async fn note_handoff_direct(app: &impl crate::capabilities::Db, from_bot: &str, to_run_id: &str) {
    match handed_off_target(app, from_bot, to_run_id).await {
        Ok(true) => tracing::info!(
            event = "relay_handoff_direct",
            bot = from_bot,
            to = to_run_id,
            "relay announce to an agent whose project was handed off: the shim delivers it directly; no turn is recorded on either daemon (SPEC §6.5d)"
        ),
        Ok(false) => {}
        Err(e) => tracing::debug!(bot = from_bot, to = to_run_id, error = ?e, "could not check whether a relay target was handed off"),
    }
}

/// 開進行中的回合。已經有回合在飛（收件方正忙，字會排在它後面）就不開，UI 本來就看得到。寫不進去只記 log，不擋補 Enter。
pub(super) async fn open_turn(app: &impl RelayWatchContext, run: &db::Run, from_bot: &str, from_turn: Option<&str>, text: &str) -> Option<String> {
    match try_open_turn(app, run, from_bot, from_turn, text).await {
        Ok(tid) => tid,
        Err(e) => {
            tracing::warn!(error = ?e, run = %run.id, "agent-to-agent prompt: could not open a turn");
            None
        }
    }
}

async fn try_open_turn(app: &impl RelayWatchContext, run: &db::Run, from_bot: &str, from_turn: Option<&str>, text: &str) -> anyhow::Result<Option<String>> {
    let lock = app.bot_lock(&run.bot_id).await;
    let _g = lock.lock().await;
    let Some(run) = db::run(app.db(), &run.id).await? else { return Ok(None) };
    if run.state != "running" {
        return Ok(None);
    }
    let transcript = run.adopted != 0 && db::bot(app.db(), &run.bot_id).await?.is_some_and(|b| matches!(b.kind.as_str(), "grok" | "agy") && b.inject_hooks == 0);
    if transcript && super::grok_transcript::is_slash_command(text) {
        return Ok(None);
    }
    let conv = db::conversation_id(app.db(), &run.bot_id).await?;
    if db::in_flight_turn(app.db(), &run.id).await?.is_some() {
        if transcript {
            let mut tx = app.db().begin().await?;
            let msg = insert_message_relayed_tx(&mut tx, &conv, None, "user", text, "hook", false, None, Some(from_bot)).await?;
            super::messages::mark_relay_turn(&mut tx, &msg.id, from_turn).await?;
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
    super::messages::mark_relay_turn(&mut tx, &msg.id, from_turn).await?;
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
        match db::in_flight_turn(app.db(), &w.run_id).await {
            Ok(Some(cur)) if &cur.id == t => {}
            Ok(_) => return Step::Done,
            Err(error) => {
                tracing::warn!(%error, run = %w.run_id, turn = %t, "relay watch could not read in-flight turn; retrying next poll");
                return Step::Waiting;
            }
        }
    }
    if run.agent_status == "working" || run.agent_status == "blocked" {
        return Step::Done;
    }
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
            super::delivery::read_styled_snapshot(&client, pane, "visible", 80).await.ok()
                .filter(|read| read.pane_id == pane && read.source == "visible" && !read.truncated)
                .map(|read| (client, pane.to_string(), read))
        }
        _ => None,
    };
    let held = read.as_ref().is_some_and(|(_, _, screen)| composer_holds_prompt(&bot.kind, &screen.text, &w.text));
    if !held {
        w.held_since = None;
        if now.duration_since(w.started) >= GIVE_UP_AFTER {
            return give_up(app, w).await;
        }
        return Step::Waiting;
    }
    let since = *w.held_since.get_or_insert(now);
    if let (Some(before), Some((_, _, current))) = (&w.snapshot_at_nudge, read.as_ref()) {
        if super::composer_draft::same_snapshot(&bot.kind, before, current) {
            tracing::warn!(run = %w.run_id, "relayed composer unchanged after Enter; not retrying");
            w.nudges = MAX_NUDGES;
        }
    }
    if now.duration_since(since) < NUDGE_AFTER || w.nudges >= MAX_NUDGES {
        return if w.nudges >= MAX_NUDGES && now.duration_since(w.started) >= GIVE_UP_AFTER { give_up(app, w).await } else { Step::Waiting };
    }
    let Some((client, pane, candidate)) = read else { return Step::Waiting };
    #[cfg(feature = "test-hooks")]
    super::race_point::hit("relay_before_nudge", &bot.id).await;
    let Ok(fresh) = super::delivery::read_styled_snapshot(&client, &pane, "visible", 80).await else { return Step::Waiting };
    if !super::composer_draft::same_snapshot(&bot.kind, &candidate, &fresh)
        || !composer_holds_prompt(&bot.kind, &fresh.text, &w.text)
    {
        w.held_since = None;
        return Step::Waiting;
    }
    let current = match db::active_run(app.db(), &w.bot_id).await {
        Ok(Some(current)) => current,
        Ok(None) => return Step::Done,
        Err(_) => return Step::Waiting,
    };
    if current.id != run.id || current.state != "running" || current.pane_id != run.pane_id
        || current.herdr_session != run.herdr_session || current.native_session_id != run.native_session_id
        || current.agent_status != run.agent_status
    {
        return Step::Done;
    }
    if let Some(turn_id) = &w.turn_id {
        match db::in_flight_turn(app.db(), &w.run_id).await {
            Ok(Some(turn)) if &turn.id == turn_id => {}
            Ok(_) => return Step::Done,
            Err(_) => return Step::Waiting,
        }
    }
    if let Err(e) = client.pane_send_keys(&pane, &["Enter"]).await {
        tracing::warn!(error = ?e, run = %w.run_id, "could not re-send Enter for a relayed prompt");
        return Step::Waiting;
    }
    w.snapshot_at_nudge = Some(fresh);
    w.nudges += 1;
    w.held_since = Some(now);
    tracing::warn!(run = %w.run_id, bot = %w.bot_id, "relayed prompt was still in the composer; re-sent Enter");
    Step::Nudged
}

async fn give_up(app: &impl RelayWatchContext, w: &Watch) -> Step {
    let Some(turn_id) = w.turn_id.as_deref() else { return Step::Done };
    if !w.may_fail {
        return Step::Done;
    }
    let why = "別的 agent 交辦的這句話沒有被收下：字沒進輸入列，或補了 Enter 也沒有反應（畫面沒有重繪），agent 沒有接手，這一回合不會有回覆。";
    let res: anyhow::Result<Option<db::Message>> = async {
        let mut tx = app.db().begin().await?;
        let out = turn_controller::fail_on(&mut tx, turn_id, turn_controller::DeliveryOnFail::Failed, "交辦沒有送達").await?;
        if out != turn_controller::Outcome::Applied {
            return Ok(None);
        }
        let conv: String = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id=?")
            .bind(turn_id).fetch_one(&mut *tx).await?;
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
pub async fn on_resolved_announce(app: &impl RelayWatchContext, from_bot: &str, from_turn: Option<&str>, text: &str, run: Option<db::Run>) {
    if text.trim().is_empty() {
        return;
    }
    let Some(run) = run else { return };
    if run.bot_id == from_bot {
        return;
    }
    if super::dead_panes::pane_gone(app, &run).await {
        app.sweep_dead_panes().await;
        return;
    }
    let turn_id = open_turn(app, &run, from_bot, from_turn, text).await;
    spawn_watch(
        app,
        Watch {
            run_id: run.id.clone(),
            bot_id: run.bot_id.clone(),
            text: text.to_string(),
            turn_id,
            started: Instant::now(),
            may_fail: true,
            held_since: None,
            nudges: 0,
            snapshot_at_nudge: None,
            _registered: None,
        },
    );
}

/// 寄件 bot 送一句報備當下正在跑的回合（#927）：`at`（RFC 3339 或 `db::now()` 的格式）落在 `[開跑時間, completed_at]` 之間、開跑最晚的那一回合。
/// 開跑時間是 `COALESCE(delivered_at, created_at)`：排過隊的回合要等真的送達才算在跑（#1365）；還沒開始的（`queued`）不是。
/// 本機 API 傳的是現在；遠端 spool 傳的是 shim 寫進報備的 `received_at`（送出當下，不是 daemon 收到的時間）。找不到就是 `None`。
pub async fn sender_turn_at(app: &impl crate::capabilities::Db, bot_id: &str, at: &str) -> anyhow::Result<Option<String>> {
    let turn = sqlx::query_scalar(
        "SELECT t.id FROM turns t
           JOIN conversations c ON c.id = t.conversation_id
          WHERE c.bot_id = ?1
            AND t.status <> 'queued'
            AND julianday(COALESCE(t.delivered_at, t.created_at)) <= julianday(?2)
            AND (t.completed_at IS NULL OR julianday(t.completed_at) >= julianday(?2))
          ORDER BY COALESCE(t.delivered_at, t.created_at) DESC, t.rowid DESC LIMIT 1",
    )
    .bind(bot_id)
    .bind(at)
    .fetch_optional(app.db())
    .await?;
    Ok(turn)
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
    let rows: Vec<(String, String, String, String, String)> = sqlx::query_as(
        "SELECT t.id, r.id, r.bot_id, m.content, t.created_at
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
    let (now, now_wall) = (Instant::now(), chrono::Utc::now());
    Ok(rows
        .into_iter()
        .map(|(turn_id, run_id, bot_id, text, created_at)| {
            let age = match chrono::DateTime::parse_from_rfc3339(&created_at) {
                Ok(t) => (now_wall - t.with_timezone(&chrono::Utc)).to_std().unwrap_or_default(),
                Err(_) => GIVE_UP_AFTER,
            };
            let may_fail = age < GIVE_UP_AFTER;
            Watch {
                run_id,
                bot_id,
                text,
                turn_id: Some(turn_id),
                started: if may_fail { now.checked_sub(age).unwrap_or(now) } else { now },
                may_fail,
                held_since: None,
                nudges: 0,
                snapshot_at_nudge: None,
                _registered: None,
            }
        })
        .collect())
}

/// 開一條盯梢。回合已經有一條在盯就不開（#1366）；`turn_id` 是 `None`（收件方正忙、沒開回合）照舊每次都開。
fn spawn_watch(app: &impl RelayWatchContext, mut watch: Watch) {
    if let Some(turn_id) = watch.turn_id.as_deref() {
        let Some(claim) = Registered::claim(turn_id) else { return };
        watch._registered = Some(claim);
    }
    app.spawn_relay_watch(watch);
}


#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub mod test_seams {
    pub async fn give_up(app: &impl RelayWatchContext, w: &Watch) -> Step { super::give_up(app, w).await }
    pub async fn restore_open_watches(app: &impl crate::capabilities::Db, host: &str) -> anyhow::Result<Vec<Watch>> { super::restore_open_watches(app, host).await }
    use super::*;
    pub async fn open_turn(app: &impl RelayWatchContext, run: &db::Run, from_bot: &str, from_turn: Option<&str>, text: &str) -> Option<String> { super::open_turn(app, run, from_bot, from_turn, text).await }
}
