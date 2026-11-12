//! Reconciliation (SPEC §6.5, §11.3.4). Always scoped to **one host**: pane / workspace /
//! agent ids are only unique within a host's herdr session.

use crate::events::ports::{BotOpsPort, BotOpsRepo, HandoffRepo, HostSidePort, IngressCommands, ReconcileCommands, TurnCommands};
use crate::capabilities::{BotLocks, BotStatusEmit, DataDir, Db, Emit, HerdrRoutes, Isolation};
use crate::db;
use anyhow::Result;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

#[cfg(test)]
pub(crate) use crate::runners::reconcile::{
    autostart_after_reconcile, rearm_progress, reconcile_host, AutostartClaim,
};

pub trait PaneIdentitySync: Send + Sync {
    fn sync_child_identity(&self, host: &str, bot: &crate::db::Bot, pane_id: &str, pid: Option<i64>) -> impl std::future::Future<Output = ()> + Send;
    fn claude_default_effort(&self, host: &str, identity: Option<&str>, alias: &str) -> impl std::future::Future<Output = anyhow::Result<String>> + Send;
}

pub trait ReconcileHostEnv:
    DataDir
    + Db
    + Emit
    + BotLocks
    + BotStatusEmit
    + Isolation
    + HerdrRoutes
    + BotOpsPort
    + HostSidePort
    + TurnCommands
    + IngressCommands
    + ReconcileCommands
    + BotOpsRepo
    + crate::shared_host::SharedHostEnv
    + PaneIdentitySync
    + Send
    + Sync
{}

impl<T> ReconcileHostEnv for T where
    T: DataDir
        + Db
        + Emit
        + BotLocks
        + BotStatusEmit
        + Isolation
        + HerdrRoutes
        + BotOpsPort
        + HostSidePort
        + TurnCommands
        + IngressCommands
        + ReconcileCommands
        + BotOpsRepo
        + crate::shared_host::SharedHostEnv
        + PaneIdentitySync
        + Send
        + Sync
        + ?Sized
{}

/// 開機恢復讀不到之後，第 `attempt` 次重試前等多久：很快就好的多半是 busy，一直讀不到就放慢到每分鐘一次。
pub(crate) fn recovery_retry_delay(attempt: usize) -> std::time::Duration {
    if cfg!(test) {
        return std::time::Duration::from_millis(20);
    }
    const SECS: [u64; 5] = [2, 5, 15, 30, 60];
    std::time::Duration::from_secs(SECS[attempt.min(SECS.len() - 1)])
}

/// 開機恢復還欠著的部分。
pub(crate) struct Recovery {
    /// 開機那一刻。之後才開始的 run、之後才建立的插隊送出、之後才結束的 run 都是這個行程自己的帳。
    boot: String,
    /// 還沒把 in-flight 回合接回來的 run；`None`＝連 run 清單都還沒讀到。
    runs: Option<std::collections::HashSet<String>>,
    queue: bool,
    send_nows: bool,
    ended_runs: bool,
    queued_prompt_restamps: bool,
}

impl Recovery {
    pub(crate) fn new() -> Self {
        Self { boot: db::now(), runs: None, queue: true, send_nows: true, ended_runs: true, queued_prompt_restamps: true }
    }

    /// 補一輪；回 `true`＝什麼都不欠了。
    pub(crate) async fn pass(&mut self, app: &(impl crate::capabilities::BotLocks + crate::capabilities::Db + crate::events::ports::TurnCommands)) -> bool {
        // 插隊送出途中停掉、還沒掛上 run 的那一則（#120）。排在接回 poller 之前：鍵其實生效了的那一則會在這裡掛上 run（#229），
        // 接回的才是它、不是已經被它打斷的那一筆。
        if self.send_nows {
            self.send_nows = !app.adopt_unbound_send_nows(&self.boot).await;
        }
        self.rearm_in_flight(app).await;
        // Queued prompts in a backoff lost their timers with the old process (SPEC §4.4a).
        if self.queue {
            match app.rearm_queue_retries().await {
                Ok(_) => self.queue = false,
                Err(e) => tracing::warn!(error = %e, "cannot re-arm queued prompt retries yet"),
            }
        }
        // run 已經結束、收尾卻欠著（帳只在記憶體，重啟就沒了）的那一筆（#156）。
        if self.ended_runs {
            self.ended_runs = !app.adopt_turns_of_ended_runs(&self.boot).await;
        }
        if self.queued_prompt_restamps {
            match app.rearm_queued_prompt_restamps().await {
                Ok(()) => self.queued_prompt_restamps = false,
                Err(e) => tracing::warn!(error = %e, "cannot re-arm queued prompt restamps yet"),
            }
        }
        self.runs.as_ref().is_some_and(|r| r.is_empty()) && !self.queue && !self.send_nows && !self.ended_runs && !self.queued_prompt_restamps
    }

    async fn rearm_in_flight(&mut self, app: &(impl crate::capabilities::BotLocks + crate::capabilities::Db + crate::events::ports::TurnCommands)) {
        if self.runs.as_ref().is_some_and(|r| r.is_empty()) {
            return;
        }
        let runs = match crate::db::all_active_runs(app.db()).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = ?e, "cannot re-arm progress pollers yet");
                return;
            }
        };
        let mut still = std::collections::HashSet::new();
        for run in runs {
            let owed = match &self.runs {
                Some(ids) => ids.contains(&run.id),
                None => run.started_at <= self.boot,
            };
            if !owed { continue; }
            match db::bot(app.db(), &run.bot_id).await {
                Ok(Some(bot)) if bot.kind == "claude-api" => continue,
                Ok(Some(_)) => {},
                Ok(None) | Err(_) => { still.insert(run.id); continue; }
            }
            if !rearm_run(app, &run).await {
                still.insert(run.id);
            }
        }
        // 清單上已經不在的 run（結束了）不再欠：它的回合歸結束那條路收。
        self.runs = Some(still);
    }
}

/// 一個 run 在飛的那一筆接回 poller（剛送出的再補 stall watchdog）。`false`＝這一輪讀寫不到，之後再補。
async fn rearm_run(app: &(impl crate::capabilities::BotLocks + crate::capabilities::Db + crate::events::ports::TurnCommands), run: &db::Run) -> bool {
    // 跟送出同一把鎖：這個行程送到一半的那一筆不會被當成重啟前的孤兒。
    let lock = app.bot_lock(&run.bot_id).await;
    let _g = lock.lock().await;
    // 這個行程欠著的送達結果先結清：DB 裡那一筆還是 pending，不能被當成重啟前送到一半的收成 unknown（#149 的帳）。
    if let Err(e) = app.settle_owed_deliveries(&run.bot_id).await {
        tracing::warn!(run = %run.id, error = %e, "cannot settle this bot's owed delivery results yet; its in-flight turn waits");
        return false;
    }
    // 已經有人盯著這個 run（重試期間這個行程自己送出的回合掛了 poller）：不再掛一次。
    if app.has_progress_poller(&run.id).await {
        return true;
    }
    let turn = match crate::db::in_flight_turn(app.db(), &run.id).await {
        Ok(Some(t)) => t,
        Ok(None) => return true,
        Err(e) => {
            tracing::warn!(run = %run.id, error = ?e, "cannot read this run's in-flight turn yet");
            return false;
        }
    };
    tracing::info!(run = %run.id, turn = %turn.id, "re-arming progress poller after restart");
    if !adopt_orphan_delivery(app, &turn).await {
        return false;
    }
    // 重啟前就被按停的（打斷的帳只在記憶體；claude 2.1.276+ 按 Esc 不送 hook）：照 Esc 收，不接回（#235）。
    match app.adopt_interrupted_on_restart(run, &turn).await {
        Ok(true) => return true,
        Ok(false) => {}
        Err(e) => {
            tracing::warn!(run = %run.id, turn = %turn.id, error = %e, "cannot check whether this turn was interrupted before the restart yet");
            return false;
        }
    }
    app.arm_progress(&run.id, &run.bot_id, &turn.id).await;
    // 送出後幾秒內被重啟：補 Enter 與「畫面上找不到就重送」這兩層網都只活在上一個行程裡
    // （review 2026-09-16）。只對**剛送出**的補，不然會把幾小時前的 prompt 重送一次。
    // 「剛送出」看送出的時間：排隊的 turn 的 created_at 是排進佇列的時間，flush 可能晚了半小時（deliv L3）。
    if turn.delivery == "ok" && fresh_enough(turn.delivered_at.as_deref().unwrap_or(&turn.created_at)) {
        app.arm_stall(&run.id, &run.bot_id, &turn.id).await;
    }
    true
}

/// 重啟前正在送出的那一筆（`in_flight` 而 `delivery` 還是 `pending`）：它的收尾者只活在上一個行程的
/// 那個 async 任務裡，重啟後沒有任何人會碰它——`try_fallback` 只處理 `ok`、佇列看到 in-flight 就返回，
/// 而 AGM 的 safety 會把它讀成「daemon 正在打字」而永遠不給重啟窗口（review 2026-09-16）。
///
/// 鍵可能已經按下去了，所以不能當成沒送：標成 `unknown`（＝「按過了，證不出來」）交給既有的
/// 放棄／人工判斷那條路，UI 也才會顯示「送出狀態不明」而不是一直轉。送達結果寫不回去、帳在重啟時丟了的那一筆
/// 也走這裡（#149）：不自動重送，送出時間最晚就是現在——閒置 watchdog 才不會把剛送出的看成排隊那時一樣老。
///
/// 寫不進去回 `false`（#75）：那一筆還是 pending，不能照樣掛 poller 當成收好了，留給開機恢復的下一輪。
async fn adopt_orphan_delivery(app: &(impl crate::capabilities::Db + crate::events::ports::TurnCommands), turn: &db::Turn) -> bool {
    if turn.delivery != "pending" {
        return true;
    }
    let n = match sqlx::query(
        "UPDATE turns SET delivery='unknown', auto_resend=0, delivered_at=COALESCE(delivered_at, ?)
          WHERE id = ? AND status='in_flight' AND delivery='pending'",
    )
    .bind(crate::db::now())
    .bind(&turn.id)
    .execute(app.db())
    .await
    {
        Ok(r) => r.rows_affected(),
        Err(e) => {
            tracing::warn!(turn = %turn.id, error = ?e, "cannot mark a prompt caught mid-delivery by the restart as unknown yet");
            return false;
        }
    };
    if n > 0 {
        tracing::warn!(turn = %turn.id, "a prompt was mid-delivery when the daemon stopped; marked unknown so somebody can decide");
        app.emit_turn(&turn.id).await;
    }
    true
}

/// 剛送出不久才值得補上 stall watchdog：它會在 12 秒後判「畫面上完全沒有這則」並重送一次，
/// 對一筆幾小時前的 turn 那是把舊訊息又送一次，比不補更糟。
fn fresh_enough(created_at: &str) -> bool {
    fresh_enough_at(created_at, chrono::Utc::now())
}

/// [`fresh_enough`] 對指定的「現在」：測試傳固定的時刻，不靠牆鐘（#950：高負載下兩次取時間差幾秒，邊界值會翻）。
fn fresh_enough_at(created_at: &str, now: chrono::DateTime<chrono::Utc>) -> bool {
    const MAX_AGE_SECS: i64 = 120;
    chrono::DateTime::parse_from_rfc3339(created_at)
        .map(|t| {
            let age = now - t.with_timezone(&chrono::Utc);
            age >= chrono::Duration::zero() && age.num_seconds() <= MAX_AGE_SECS
        })
        .unwrap_or(false)
}

/// A possible parent this pass; spawn ownership chooses the bot, names only format its display.
struct Parent {
    agent_name: String,
    tab_id: Option<String>,
    bot: db::Bot,
}

struct ParentReplacementEvidence<'a> {
    agents: &'a [crate::herdr::AgentInfo],
    hints: &'a HashMap<String, String>,
    claimed: &'a std::collections::HashSet<String>,
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

/// herdr agent 名稱最長 32 字、字元集 `[a-z][a-z0-9_-]*`（見 `config::agent_name`／shim 的 `am_child_name`）。
const HERDR_NAME_MAX: usize = 32;

/// 行程 argv 的 `--name X`／`--name=X`／`-n X`（argv[0] 是執行檔，不算）。
fn argv_agent_name(argv: &[String]) -> Option<&str> {
    let mut it = argv.iter().skip(1);
    while let Some(a) = it.next() {
        if a == "--name" || a == "-n" {
            return it.next().map(String::as_str);
        }
        if let Some(v) = a.strip_prefix("--name=") {
            return Some(v);
        }
    }
    None
}

/// 無名子 agent 要補上的 `<parent>-<字尾>` 之字尾：argv 的 `--name` 去掉 parent 前綴後的部分（合法才用），
/// 否則由 pane id 產生（小寫、非英數換成 `-`）。整個名字放不進 32 字就回 None（不改名）。
fn unnamed_child_suffix_from(pane_id: &str, parent_agent: &str, argv_name: Option<&str>) -> Option<String> {
    let fits = |suffix: &str| !suffix.is_empty() && parent_agent.len() + 1 + suffix.len() <= HERDR_NAME_MAX;
    let legal = |suffix: &str| {
        suffix.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
            && !suffix.starts_with('-')
            && fits(suffix)
    };
    if let Some(suffix) = argv_name.and_then(|n| n.strip_prefix(parent_agent)).and_then(|r| r.strip_prefix('-')) {
        if legal(suffix) {
            return Some(suffix.to_string());
        }
    }
    unnamed_child_suffix(pane_id, parent_agent)
}

/// [`unnamed_child_suffix_from`] 沒有 argv 名字可用時的字尾：由 pane id 產生。
fn unnamed_child_suffix(pane_id: &str, parent_agent: &str) -> Option<String> {
    let mut slug = String::new();
    for c in pane_id.chars().map(|c| c.to_ascii_lowercase()) {
        let c = if c.is_ascii_lowercase() || c.is_ascii_digit() { c } else { '-' };
        if c == '-' && (slug.is_empty() || slug.ends_with('-')) {
            continue;
        }
        slug.push(c);
    }
    let slug = slug.trim_end_matches('-');
    // 放不進 32 字就留右邊（pane 自己的編號在右邊，左邊是 workspace）；一個字都放不下才不改名。
    let room = HERDR_NAME_MAX.saturating_sub(parent_agent.len() + 1);
    let skip = slug.len().saturating_sub(room);
    let slug = slug[skip..].trim_start_matches('-');
    (!slug.is_empty()).then(|| slug.to_string())
}

/// issue #1472：用 `herdr pane run <pane> "claude …"` 開的子 agent 在 herdr 沒有名稱，認領迴圈只看名字就整顆跳過。
/// 有 spawn hint 指向活著的 parent
/// 才補上 `<parent>-<字尾>`，回傳新名字；沒有證據就不碰（同 tab 裡可能是使用者自己開的 claude）。
/// rename 失敗只記 log、回 None：這一輪不收、下一輪再試，不中斷整台主機的對帳。
async fn name_unnamed_child(
    host: &str,
    client: &crate::herdr::HerdrClient,
    agent: &crate::herdr::AgentInfo,
    parents: &[Parent],
    hints: &HashMap<String, String>,
) -> Option<String> {
    let live = |p: &&Parent| p.tab_id.is_some();
    let hinted = hints.get(&agent.pane_id).and_then(|bid| parents.iter().filter(live).find(|p| &p.bot.id == bid));
    let parent = hinted?;
    let processes = client.pane_process_info(&agent.pane_id).await.unwrap_or_default();
    let argv_name = processes.iter().find_map(|p| argv_agent_name(&p.argv));
    let suffix = unnamed_child_suffix_from(&agent.pane_id, &parent.agent_name, argv_name)?;
    let name = format!("{}-{suffix}", parent.agent_name);
    match client.agent_rename(&agent.pane_id, &name).await {
        Ok(renamed) => {
            tracing::info!(host, parent = %parent.bot.name, pane = %agent.pane_id, agent = %name,
                           "reconcile: gave an unnamed spawned agent a herdr name so it can be adopted");
            Some(renamed.name.filter(|n| !n.is_empty()).unwrap_or(name))
        }
        Err(e) => {
            tracing::warn!(host, parent = %parent.bot.name, pane = %agent.pane_id, agent = %name, error = ?e,
                           "reconcile: could not name an unnamed spawned agent; leaving it for the next pass");
            None
        }
    }
}

/// Herdr normally tells us the CLI kind.  During launch it can report an agent before that
/// field is populated, so use the foreground executable as a deterministic fallback instead of
/// permanently inheriting the parent's kind.
fn known_kind(value: Option<&str>) -> Option<&'static str> {
    let value = value?.trim().to_ascii_lowercase();
    crate::config::KINDS.iter().copied().find(|kind| *kind == value)
}

fn kind_from_argv(argv: &[String]) -> Option<&'static str> {
    let executable = argv.first()?.rsplit('/').next();
    known_kind(executable)
}

async fn child_kind(client: &crate::herdr::HerdrClient, agent: &crate::herdr::AgentInfo, fallback: &str) -> String {
    if let Some(kind) = known_kind(agent.agent.as_deref()) {
        return kind.to_string();
    }
    if let Ok(processes) = client.pane_process_info(&agent.pane_id).await {
        if let Some(kind) = processes.iter().find_map(|p| kind_from_argv(&p.argv)) {
            return kind.to_string();
        }
    }
    fallback.to_string()
}

/// A child can be adopted one pass before Herdr has filled `agent`.  Once the real kind is
/// visible, correct the persisted row even though it is already a claimed child.  Reset all
/// kind-dependent observations so the next pane probe uses the new CLI's env and argv rules.
async fn refresh_child_kind(app: &(impl crate::capabilities::Db + crate::capabilities::Emit), host: &str, bot: &mut db::Bot, pane_id: &str, kind: &str) -> anyhow::Result<()> {
    if bot.managed_by != "child" || bot.kind == kind {
        return Ok(());
    }
    let mut tx = app.db().begin().await?;
    let changed = sqlx::query(
        "UPDATE bots SET kind = ?, identity = NULL, model = NULL, effort = NULL, fast = 0
         WHERE id = ? AND managed_by = 'child'",
    )
    .bind(kind)
    .bind(&bot.id)
    .execute(&mut *tx)
    .await?;
    if changed.rows_affected() == 0 {
        tx.rollback().await?;
        return Ok(());
    }
    sqlx::query(
        "UPDATE runs SET runtime_model = NULL, runtime_effort = NULL, runtime_fast = NULL, runtime_identity = NULL
         WHERE bot_id = ? AND state IN ('starting','running','stopping')",
    )
    .bind(&bot.id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    crate::pane_identity::reset_probe(host, &bot.id, pane_id);
    bot.kind = kind.to_string();
    bot.identity = None;
    bot.model = None;
    bot.effort = None;
    bot.fast = 0;
    app.emit("bot_changed", json!({"bot_id": bot.id})).await;
    Ok(())
}

/// 兩條退休路徑真正寫 `deleted_at` 的地方：走 `child_retire` 的唯一入口（#413：記呼叫端；AGM 的 child 不隱式退役）。
/// 讀不到誰是 AGM 的就排延後那一輪再看。
async fn retire_child(app: &impl ReconcileHostEnv, host: &str, bot: &db::Bot, why: &'static str) -> Result<()> {
    if app.retire_child(&bot.id, why, crate::child_retire::Mode::Implicit).await? == crate::child_retire::Outcome::Unreadable {
        app.schedule_deferred_pass(host);
    }
    Ok(())
}

/// 子 agent 的 agent 不在了，能不能照 #60 退休它（#191）。兩條退休路徑共用這一支，免得一邊修、一邊漏。
///
/// 只有**確定沒在維護**才可以：herdr 重啟的那幾分鐘正是所有 pane 同時消失的時候，這時讀不到維護狀態就當成
/// 「沒在維護」，子 bot 被軟刪，pane 回來之後也接不回原對話與血緣。讀不到＝這一輪留著、晚一點再看。
async fn may_retire_child(app: &impl ReconcileHostEnv, host: &str, bot: &db::Bot) -> bool {
    match app.retirement_block(&bot.id).await {
        Ok(Some(reason)) => {
            tracing::info!(host, bot = %bot.name, reason, "reconcile: child kept by a persisted restore/restart guard");
            return false;
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(host, bot = %bot.name, error = ?e, "reconcile: cannot read child retirement guards; child kept this pass");
            app.schedule_deferred_pass(host);
            return false;
        }
    }
    match app.herdr_maintenance_active().await {
        Ok(None) => true,
        Ok(Some(_)) => {
            tracing::info!(host, bot = %bot.name, "reconcile: herdr maintenance in progress, child kept");
            false
        }
        Err(e) => {
            tracing::warn!(host, bot = %bot.name, error = ?e,
                "reconcile: cannot read the herdr maintenance state; child kept this pass, will look again");
            app.schedule_deferred_pass(host);
            false
        }
    }
}

// Bound repeated legacy identity diagnostics to once per run per hour.
fn warn_unverified_run(run_id: &str) -> bool {
    static SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>> = std::sync::OnceLock::new();
    let now = std::time::Instant::now();
    let mut seen = SEEN.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    seen.retain(|_, at| now.duration_since(*at) < std::time::Duration::from_secs(3600));
    if seen.contains_key(run_id) { return false; }
    seen.insert(run_id.to_owned(), now);
    true
}

/// A parent claim proves ownership, never which child process occupies a reused pane.
fn matches_run(run: &db::Run, agent: &crate::herdr::AgentInfo, child: bool) -> bool {
    if agent.pane_id.is_empty() { return false; }
    if !matches!(run.state.as_str(), "starting" | "running" | "stopping") && run.native_session_id.is_none() { return false; }
    match (run.native_session_id.as_deref(), crate::lifecycle::dead_panes::herdr_session_id(agent)) {
        (Some(expected), Some(observed)) => !expected.is_empty() && expected == observed,
        // herdr 沒把 CLI session 綁上（grok 一律如此）：沒有反證，就以同 host／session 的 pane_id 當身份。
        // 以前子 agent 走這裡一律 false，daemon 重啟後 grok 子 agent 再也接不回、狀態不再追蹤（2026-10-19 mac_builder）。
        (Some(_), None) => run.pane_id.as_deref() == Some(agent.pane_id.as_str()),
        (None, _) if !child => run.pane_id.as_deref() == Some(agent.pane_id.as_str()),
        (None, _) => {
            // The caller has already checked host/session. A name or parent claim cannot
            // prove an instance; a persisted terminal can, even before the CLI binds a session.
            run.pane_id.as_deref() == Some(agent.pane_id.as_str())
                && run.terminal_id.as_deref().filter(|id| !id.trim().is_empty())
                    .zip(agent.terminal_id.as_deref().filter(|id| !id.trim().is_empty()))
                    .is_some_and(|(expected, observed)| expected == observed)
        },
    }
}

/// A missing child is a deliberate replacement only when its parent still has a live run and
/// herdr lists an unclaimed successor that the normal child-adoption pass will attribute to it.
/// Only an explicit ownership hint can establish a successor; tab/name similarity is not evidence.
async fn parent_replaced_child(
    app: &impl crate::capabilities::Db,
    host: &str,
    session: &str,
    client: &crate::herdr::HerdrClient,
    child: &db::Bot,
    evidence: ParentReplacementEvidence<'_>,
) -> Result<bool> {
    let ParentReplacementEvidence { agents, hints, claimed } = evidence;
    let Some(parent_id) = child.parent_bot_id.as_deref() else { return Ok(false) };
    let child_run: Option<db::Run> =
        sqlx::query_as("SELECT * FROM runs WHERE bot_id = ? AND COALESCE(herdr_session, ?)=? ORDER BY rowid DESC LIMIT 1")
            .bind(&child.id).bind(session).bind(session).fetch_optional(app.db()).await?;
    let Some(child_pane) = child_run.and_then(|run| run.pane_id) else { return Ok(false) };
    // A listed successor alone does not prove a replacement if the old child's pane is still live.
    // When Herdr cannot confirm the old pane is gone, keep the retirement unconfirmed.
    match client.pane_get(&child_pane).await {
        Ok(None) => {}
        Ok(Some(_)) | Err(_) => return Ok(false),
    }
    let parent: Option<db::Bot> = sqlx::query_as(
        "SELECT b.* FROM bots b JOIN projects p ON p.id = b.project_id
         WHERE b.id = ? AND b.deleted_at IS NULL AND p.deleted_at IS NULL AND p.host = ?",
    )
    .bind(parent_id)
    .bind(host)
    .fetch_optional(app.db())
    .await?;
    let Some(parent) = parent else { return Ok(false) };
    if parent.herdr_session.as_deref().is_some_and(|s| s != session) {
        return Ok(false);
    }
    let Some(run) = db::active_run(app.db(), &parent.id).await? else { return Ok(false) };
    if run.herdr_session.as_deref().is_some_and(|s| s != session) {
        return Ok(false);
    }

    let Some(parent_agent) = agents.iter().find(|a| matches_run(&run, a, parent.managed_by == "child")) else {
        return Ok(false);
    };

    for agent in agents {
        if claimed.contains(&agent.pane_id) || agent.pane_id == parent_agent.pane_id {
            continue;
        }
        let belongs_to_parent = hints.get(&agent.pane_id) == Some(&parent.id);
        if belongs_to_parent {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 同 host 的事件與延後對帳逐輪執行，避免拿著舊 ownership 快照重複認領 child。
/// key 帶資料目錄，彼此獨立的測試 App 不互相排隊。
pub(crate) async fn host_pass_lock(app: &impl crate::capabilities::DataDir, host: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: std::sync::OnceLock<std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = std::sync::OnceLock::new();
    let key = format!("{}\u{0}{host}", app.data_dir().display());
    LOCKS.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner()).entry(key).or_default().clone()
}

pub(crate) async fn reconcile_host_locked(app: &impl ReconcileHostEnv, host: &str) -> Result<Vec<crate::autostart_revive::Lost>> {
    let Some(session) = app.session_for_host(host).await else {
        anyhow::bail!("unknown host `{host}`");
    };
    let Some(client) = app.herdr_for_session(host, &session).await else {
        anyhow::bail!("unknown host `{host}`");
    };
    app.spawn_detect_github_host(host.to_string());
    let mut lost_autostart: Vec<crate::autostart_revive::Lost> = Vec::new();
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


    let mut live_ws: Vec<String> = snapshot
        .get("workspaces")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|w| w.get("workspace_id").and_then(|s| s.as_str()).map(String::from)).collect())
        .unwrap_or_default();
    // `workspaces: []` 但 pane 還掛著 workspace_id＝清單暫時是空的，不是 workspace 消失了。
    // 只看 workspaces 陣列會把專案映射清成 NULL，下次 start 再開一個（舊 pane 變孤兒）。
    if let Some(panes) = snapshot.get("panes").and_then(|v| v.as_array()) {
        for p in panes {
            if let Some(ws) = p.get("workspace_id").and_then(|s| s.as_str()) {
                if !ws.is_empty() && !live_ws.iter().any(|w| w == ws) {
                    live_ws.push(ws.to_string());
                }
            }
        }
    }
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

    let projects: Vec<db::Project> = db::live_projects(app.db()).await?.into_iter().filter(|p| p.host == host).collect();
    let project_labels: HashMap<String, String> = projects.iter().map(|p| (p.id.clone(), p.label.clone())).collect();
    // #708：移交出去的專案整個不碰——workspace 映射、run、child、pane 都歸接手的 daemon。
    let handed_off: std::collections::HashSet<&str> =
        projects.iter().filter(|p| p.handed_off_to.is_some()).map(|p| p.id.as_str()).collect();
    let handed_off_footprint = app.db().handoff_footprint(host, &session, &session).await?;
    for p in projects.iter().filter(|p| p.handed_off_to.is_none()) {
        if let Some(ws) = p.workspace_id.as_deref() {
            if !live_ws.contains(&ws.to_string()) {
                sqlx::query("UPDATE projects SET workspace_id=NULL WHERE id=?").bind(&p.id).execute(app.db()).await?;
                tracing::info!(host, project = %p.label, "workspace disappeared; mapping cleared");
            }
        }
    }

    let bots = db::live_bots_on_host(app.db(), host).await?;
    // Read hints before retiring missing children: a fresh hint is the direct evidence that the
    // still-running parent launched the pane which this same pass will adopt as its successor.
    // Keep read failure distinct from an empty result; adoption is already fail-closed below.
    app.prune_stale_spawn_hints().await;
    crate::spawn_hints::prune_dead_claims(app, host, &session, &live_panes).await;
    let spawn_hints = app.spawn_hints_for_host(host, &session).await;
    // Deduplicate panes within this observed host/session, never display names.
    let mut claimed: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut parents: Vec<Parent> = Vec::new();
    for bot in bots {
        if bot.kind == "claude-api" { continue; }
        let mut bot = bot;
        if handed_off.contains(bot.project_id.as_str()) {
            continue;
        }
        // Default-session bots are default_session::sync's; absent from our agent.list ≠ exited.
        if bot.herdr_session.as_deref().map(|s| s != session).unwrap_or(false) {
            continue;
        }
        let lock = app.bot_lock(&bot.id).await;
        let _g = lock.lock().await;
        let active = db::active_run(app.db(), &bot.id).await?;
        if active
            .as_ref()
            .and_then(|r| r.herdr_session.as_deref())
            .map(|s| s != session)
            .unwrap_or(false)
        {
            continue;
        }
        let computed = db::agent_name_for_bot(app.db(), &bot).await?;
        let preserve_stopping = match &active {
            Some(run) if run.state == "stopping" => app.has_open_restart_for_run(host, &bot.id, &run.id).await?,
            _ => false,
        };
        // Explicit ownership can still name a valid parent whose own agent has ended.
        // Keep its bot ID available; tab/name observations are only display/liveness data.
        let parent_index = parents.len();
        parents.push(Parent { agent_name: computed.clone(), tab_id: None, bot: bot.clone() });
        // The host/session has already been checked. Names never choose a run's occupant.
        let latest: Option<db::Run> = if active.is_some() {
            active.clone()
        } else {
            sqlx::query_as("SELECT * FROM runs WHERE bot_id=? AND COALESCE(herdr_session, ?)=? ORDER BY rowid DESC LIMIT 1")
                .bind(&bot.id).bind(&session).bind(&session).fetch_optional(app.db()).await?
        };
        if latest.is_none() { continue; }
        let missing_identity = latest.as_ref().is_some_and(|r| r.pane_id.as_deref().is_none_or(str::is_empty) && r.native_session_id.as_deref().is_none_or(str::is_empty));
        let unmapped_start = missing_identity && active.as_ref().is_some_and(|r| r.state == "starting");
        if missing_identity {
            // A failed start can close its pane before its mapping/exit reaches the DB.
            // Without IDs, only an empty agent list proves absence; re-read under the bot lock.
            if !unmapped_start || !agents.is_empty() || !client.agent_list().await.is_ok_and(|agents| agents.is_empty()) {
                continue;
            }
        }
        if let Some(run) = latest.as_ref() {
            let matches: Vec<_> = agents.iter().filter(|a| matches_run(run, a, bot.managed_by == "child")).collect();
            if matches.len() > 1 {
                claimed.extend(matches.into_iter().map(|a| a.pane_id.clone()));
                continue;
            }
        }
        let mut found = latest.as_ref().and_then(|run| {
            let mut matches = agents.iter().filter(|a| !claimed.contains(&a.pane_id) && matches_run(run, a, bot.managed_by == "child"));
            let first = matches.next()?.clone();
            matches.next().is_none().then_some(first)
        });
        // Re-read under the bot lock; a restart may have replaced the pre-lock snapshot.
        if let Some(run) = latest.as_ref() {
            let target = found.as_ref().map(|a| a.pane_id.as_str()).or(run.pane_id.as_deref()).filter(|p| !p.is_empty());
            if let Some(pane) = target {
                match client.agent_get(pane).await {
                    Ok(Some(agent)) if matches_run(run, &agent, bot.managed_by == "child") && !claimed.contains(&agent.pane_id) => found = Some(agent),
                    Ok(Some(agent)) => {
                        // Occupied but no child-instance evidence (or a conflicting native ID).
                        // Reserve the pane so the adoption pass cannot attach it to another bot.
                        if warn_unverified_run(&run.id) {
                            tracing::warn!(host, session = %session, bot = %bot.id, run = %run.id, pane = %agent.pane_id,
                                reason = "instance_identity_unverified", "reconcile: keeping run without reconnecting an unverified pane");
                        }
                        claimed.insert(agent.pane_id);
                        continue;
                    }
                    Ok(None) => found = None,
                    Err(e) => {
                        if let Some(pane) = run.pane_id.as_ref() { claimed.insert(pane.clone()); }
                        tracing::warn!(host, bot = %bot.id, error = ?e, "reconcile: cannot verify pane identity; leaving run unchanged");
                        continue;
                    }
                }
            } else if !unmapped_start {
                continue;
            }
        }
        if let Some(agent) = found.as_ref() {
            if active.as_ref().is_none_or(|run| run.pane_id.as_deref() != Some(agent.pane_id.as_str())) {
                match client.pane_get(&agent.pane_id).await {
                    Ok(Some(_)) => {}
                    Ok(None) if active.is_none() => found = None,
                    // A stale/missing destination cannot move or end an existing run.
                    Ok(None) | Err(_) => continue,
                }
            }
        }
        let found_name = found.as_ref().and_then(|a| a.name.clone());
        if let Some(agent) = found.as_ref() { claimed.insert(agent.pane_id.clone()); }
        if bot.managed_by == "child" {
            if let Some(agent) = found.as_ref() {
                let kind = child_kind(&client, agent, &bot.kind).await;
                refresh_child_kind(app, host, &mut bot, &agent.pane_id, &kind).await?;
            }
        }
        // A bot only owns a tab while herdr still lists its agent.
        parents[parent_index] = Parent {
            agent_name: found_name.clone().unwrap_or_else(|| computed.clone()),
            tab_id: found.as_ref().map(|a| a.tab_id.clone()),
            bot: bot.clone(),
        };
        match (active, found.as_ref()) {
            (Some(run), Some(agent)) => {
                let agent: &crate::herdr::AgentInfo = agent;
                if agent.name.is_none() && bot.managed_by != "child" {
                    let display = run.agent_name.as_deref().unwrap_or(&computed);
                    if let Err(e) = client.agent_rename(&agent.pane_id, display).await {
                        tracing::warn!(host, bot = %bot.id, error = ?e, "reconcile: could not restore display name");
                    }
                }
                // Identity follows pane/native session, so old shared-tab runs (tab_id NULL) just
                // learn their tab. Status is re-asked under the lock: a stale `idle` eats the next
                // `working -> idle` edge (review 2026-09-12 a).
                let status = agent.agent_status.normalized().as_str().to_string();
                // Preserve a committed restart stop for recovery; heal other stuck `stopping` runs too.
                sqlx::query("UPDATE runs SET pane_id=?, workspace_id=?, tab_id=?, agent_status=?, agent_name=COALESCE(?, agent_name), herdr_session=COALESCE(herdr_session, ?), native_session_id=COALESCE(native_session_id, ?), terminal_id=COALESCE(terminal_id, ?), state=CASE WHEN state='starting' OR (state='stopping' AND ?=0) THEN 'running' ELSE state END WHERE id=?")
                    .bind(&agent.pane_id)
                    .bind(&agent.workspace_id)
                    .bind(&agent.tab_id)
                    .bind(&status)
                    .bind(&found_name)
                    .bind(&session)
                    .bind(crate::lifecycle::dead_panes::herdr_session_id(agent))
                    .bind(agent.terminal_id.as_deref().filter(|id| !id.trim().is_empty()))
                    .bind(preserve_stopping)
                    .bind(&run.id)
                    .execute(app.db())
                    .await?;
                if run.pane_id.as_deref() != Some(agent.pane_id.as_str()) {
                    if let Some(old) = run.pane_id.as_deref() {
                        app.unwatch_pane_on_session(host, &session, old).await;
                    }
                }
                app.watch_pane_on_session(host, &session, &agent.pane_id).await;
                if bot.kind == "codex" {
                    app.schedule_codex_notice_capture(&bot.id, &run.id);
                }
                // Hookless runs (children) only have their pane, unwatched while the daemon was down.
                app.spawn_adopted_capture(&run.id, &bot.id);
                sync_pane_model(app, host, &client, &bot, agent).await;
                tracing::info!(host, bot = %bot.name, run = %run.id, pane = %agent.pane_id, "reconcile: kept active run");
            }
            (Some(run), None) => {
                tracing::info!(host, bot = %bot.name, run = %run.id, "reconcile: agent gone, marking run exited");
                let exit = app.mark_run_exited(&run.id, "agent not found during reconcile").await;
                // A child exists only as long as its pane; its conversation is kept.
                // 計畫中的 herdr 重啟期間不算：所有 pane 同時消失不是子 agent 做完了（§6.5.2）。
                // 維護結束時仍沒接回的，由 `herdr_maintenance` 照同一條規則退休。
                // run 的結束沒寫進去（#191）：DB 裡它還在跑，這時退休就是一顆刪掉的 bot 掛著活的 run。這一輪不動，
                // 晚一點再對一次帳：結束寫得進去之後，子 agent 走下面的 `(None, None)` 退休。
                if exit == crate::lc_error::RunExit::NotRecorded {
                    tracing::warn!(host, bot = %bot.name, run = %run.id, "reconcile: the run's exit was not recorded; bot left as is, will look again");
                    app.schedule_deferred_pass(host);
                } else if bot.managed_by != "child"
                    && bot.autostart == 1
                    && matches!(exit, crate::lc_error::RunExit::Recorded | crate::lc_error::RunExit::TurnOwed)
                {
                    // 不是使用者停的（那是 `stopped`）：herdr 掉了這個 agent。pass 結束後由 `autostart_revive` 決定要不要再起。
                    // 只有**這一輪自己**把它收成 exited 才算：`AlreadyEnded`＝別的路先收了（使用者的 stop、pane-exited 事件＝使用者在 herdr 裡關了 pane），
                    // 那不是 herdr 掉的，拉起來就是推翻使用者要它停。
                    lost_autostart.push(crate::autostart_revive::Lost { bot_id: bot.id.clone(), run_id: run.id.clone() });
                } else if bot.managed_by == "child" && may_retire_child(app, host, &bot).await {
                    let why = if let Ok(hints) = &spawn_hints {
                        if parent_replaced_child(
                            app,
                            host,
                            &session,
                            &client,
                            &bot,
                            ParentReplacementEvidence { agents: &agents, hints, claimed: &claimed },
                        )
                        .await?
                        {
                            "parent_replaced_child"
                        } else {
                            "reconcile_agent_gone"
                        }
                    } else {
                        "reconcile_agent_gone"
                    };
                    retire_child(app, host, &bot, why).await?;
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
                    "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, adopted, agent_name, herdr_session, native_session_id, started_at)
                     VALUES (?,?,'running',?,?,?,?,1,?,?,?,?)",
                )
                .bind(&run_id)
                .bind(&bot.id)
                .bind(&status)
                .bind(&agent.workspace_id)
                .bind(&agent.pane_id)
                .bind(&agent.tab_id)
                .bind(&found_name)
                .bind(&session)
                .bind(crate::lifecycle::dead_panes::herdr_session_id(agent))
                .bind(db::now())
                .execute(app.db())
                .await?;
                sqlx::query("UPDATE projects SET workspace_id=? WHERE id=? AND workspace_id IS NULL")
                    .bind(&agent.workspace_id)
                    .bind(&bot.project_id)
                    .execute(app.db())
                    .await?;
                app.watch_pane_on_session(host, &session, &agent.pane_id).await;
                if bot.kind == "codex" {
                    app.schedule_codex_notice_capture(&bot.id, &run_id);
                }
                app.spawn_adopted_capture(&run_id, &bot.id);
                sync_pane_model(app, host, &client, &bot, agent).await;
                tracing::info!(host, bot = %bot.name, run = %run_id, pane = %agent.pane_id, "reconcile: adopted existing agent");
            }
            (None, None) => {
                // #60: `pane_closed` ended the child's run before we got here, so "agent gone" above
                // never sees it. A child cannot be restarted (`start_bot` refuses): retire it.
                if bot.managed_by == "child" && may_retire_child(app, host, &bot).await {
                    let ended: i64 = sqlx::query_scalar(
                        "SELECT COUNT(*) FROM runs WHERE bot_id = ? AND state NOT IN ('starting','running','stopping')",
                    )
                    .bind(&bot.id)
                    .fetch_one(app.db())
                    .await?;
                    if ended > 0 {
                        let why = if let Ok(hints) = &spawn_hints {
                            if parent_replaced_child(
                                app,
                                host,
                                &session,
                                &client,
                                &bot,
                                ParentReplacementEvidence { agents: &agents, hints, claimed: &claimed },
                            )
                            .await?
                            {
                                "parent_replaced_child"
                            } else {
                                "reconcile_run_already_ended"
                            }
                        } else {
                            "reconcile_run_already_ended"
                        };
                        retire_child(app, host, &bot, why).await?;
                    }
                }
            }
        }
        app.emit_bot_status(&bot.id).await;
    }

    // Only explicit pane ownership may select a parent. Missing/unreadable claims stay unknown.
    let (hints, strangers): (HashMap<String, String>, &[crate::herdr::AgentInfo]) =
        match spawn_hints {
            Ok(h) => (h, agents.as_slice()),
            Err(e) => {
                // 無名 agent（#1472）也算：它靠 hint 才收得到，這一輪讀不到就一顆都不認領、不改名。
                if agents.iter().any(|a| !claimed.contains(&a.pane_id)) {
                    tracing::warn!(host, error = ?e, "reconcile: cannot read spawn hints; adopting no new child this pass, will look again");
                    app.schedule_deferred_pass(host);
                }
                (HashMap::new(), &[][..])
            }
        };
    // #709：共用 session 的主機上只認領自己 workspace／tab／pane 裡的（連名字前綴也不算），別顆 daemon 的 agent 不收。
    let owned = if crate::shared_host::is_shared(app, host).await { Some(crate::shared_host::owned(app, host).await?) } else { None };
    let mut new_children = 0usize;
    for agent in strangers.iter() {
        if agent.pane_id.is_empty() { continue; }
        // #708：以本 session 的 pane/tab/workspace ID 排除移交出去的 agent。
        if handed_off_footprint.covers_agent(agent) {
            continue;
        }
        // #709：共用主機只收自己的。
        if owned.as_ref().is_some_and(|o| !o.covers_agent(agent)) {
            continue;
        }
        // issue #1472：無名 agent 必須先有明確 ownership 才補顯示名字。
        // 住著某顆 bot 的 active run 的 pane 不是新的子 agent（那是「名字被清掉」，由上面的 per-bot 流程放回去）。
        let renamed = if agent.name.is_none() {
            let occupied: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM runs WHERE pane_id = ? AND state IN ('starting','running','stopping')
                 AND bot_id IN (SELECT b.id FROM bots b JOIN projects p ON p.id=b.project_id WHERE p.host=?)
                 AND COALESCE(herdr_session, ?) = ?",
            )
            .bind(&agent.pane_id)
            .bind(host)
            .bind(&session)
            .bind(&session)
            .fetch_one(app.db())
            .await?;
            if occupied > 0 {
                continue;
            }
            match name_unnamed_child(host, &client, agent, &parents, &hints).await {
                Some(n) => Some(n),
                None => continue,
            }
        } else {
            None
        };
        let Some(name) = renamed.as_deref().or(agent.name.as_deref()) else { continue };
        if claimed.contains(&agent.pane_id) {
            continue;
        }
        let by_hint = hints.get(&agent.pane_id).and_then(|bid| parents.iter().find(|p| &p.bot.id == bid));
        let Some(entry) = by_hint else { continue };
        let (parent_name, parent) = (&entry.agent_name, &entry.bot);
        let raw_child_name = match prefix_score(parent_name, name) {
            0 => child_name_from_agent(name),
            n => {
                let suffix = &name[n + 1..];
                if crate::config::valid_bot_name(suffix) { suffix.to_string() } else { child_name_from_agent(name) }
            }
        };
        // Herdr already stripped the actual parent prefix above. If the requested child suffix
        // contains another full project bot identity (e.g. `hub-dgs9j9-sheet`), keep only the
        // concise nickname; the actual herdr name written to `runs.agent_name` remains intact.
        let project_label = project_labels.get(&parent.project_id).map(String::as_str).unwrap_or("");
        let child_name = crate::config::short_child_name(project_label, &raw_child_name);
        let kind = child_kind(&client, agent, &parent.kind).await;
        // One failed child is logged and skipped; a `?` here aborted the whole host (review 2026-09-12 #2).
        match adopt_child(app, host, &client, &session, agent, name, parent, &child_name, &raw_child_name, &kind).await {
            Ok(bot_id) => {
                claimed.insert(agent.pane_id.clone());
                if by_hint.is_some() {
                    app.consume_spawn_hint(host, &agent.pane_id).await;
                }
                app.emit("bot_changed", json!({"bot_id": bot_id})).await;
                app.emit_bot_status(&bot_id).await;
                new_children += 1;
                tracing::info!(host, parent = %parent.name, child = %child_name, agent = %name, pane = %agent.pane_id,
                               hinted = by_hint.is_some(), "reconcile: adopted a spawned child agent");
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
    //
    // **herdr 重開後 pane id 會被重用**（`panes.rs` 的 inbox key 早就為了同一件事帶 `first_seen`），而
    // `runs.pane_id` 結束後不會清、`runs` 也沒有保留期——所以光比對 id 的話，使用者自己開的 shell 只要
    // 拿到某個歷史 run 用過的 id 就會被當成孤兒關掉（issue #469）。
    //
    // 分辨的依據：真孤兒在**同一輪裡是「先關、後掃」**（這段在 `panes::scan_snapshot` 之前），所以它還沒有
    // `panes` 列；有列、而且 `first_seen` 晚於那個 run 的 `ended_at`，就是熬過至少一輪的**另一顆** pane
    // ——那是 `panes` GC 的事（它有 `user_pane`／`scratch`／`recent_output` 三條守門），不該由這裡硬關。
    // `ended_at` 是 NULL 的舊列照舊（`> NULL` 是 NULL，`NOT EXISTS` 成立），行為不變。
    let dead: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT DISTINCT r.pane_id, r.tab_id, r.workspace_id FROM runs r JOIN bots b ON b.id = r.bot_id JOIN projects p ON p.id = b.project_id
         WHERE r.pane_id IS NOT NULL AND p.host = ? AND r.state IN ('exited','stopped')
         AND p.handed_off_to IS NULL
         AND COALESCE(r.herdr_session, ?) = ?
         AND NOT EXISTS (
           SELECT 1 FROM panes pn WHERE pn.host = ? AND pn.pane_id = r.pane_id AND pn.first_seen > r.ended_at)
         AND r.pane_id NOT IN (
           SELECT r2.pane_id FROM runs r2 JOIN bots b2 ON b2.id = r2.bot_id JOIN projects p2 ON p2.id = b2.project_id
           WHERE r2.pane_id IS NOT NULL AND p2.host = ? AND r2.state IN ('starting','running','stopping')
           AND COALESCE(r2.herdr_session, ?) = ?)",
    )
    .bind(host)
    .bind(&session)
    .bind(&session)
    .bind(host)
    .bind(host)
    .bind(&session)
    .bind(&session)
    .fetch_all(app.db())
    .await?;
    // #709：共用 session 上 pane id 也會被別顆 daemon 的新 pane 用到：只關還在自己 workspace／tab 裡的。
    let not_ours = |pane: &str| {
        owned.as_ref().is_some_and(|o| {
            !snapshot.get("panes").and_then(|v| v.as_array()).into_iter().flatten().any(|p| {
                p.get("pane_id").and_then(|s| s.as_str()) == Some(pane) && o.covers_pane(p)
            })
        })
    };
    for (pane, tab, ws) in dead {
        if live_panes.contains(&pane) && !panes_with_agent.contains(&pane) && !not_ours(&pane) {
            tracing::info!(host, pane_id = %pane, "reconcile: closing orphan pane");
            app.close_pane_and_tab(&client, ws.as_deref(), tab.as_deref(), &pane).await;
        }
    }
    // SPEC §4.4a: adopted runs have NULL `runtime_*`; codex's status line has all three.
    fill_codex_runtime(app, host, &client).await;
    // §6.5e：非 agent 的 shell／服務 pane 收進 `panes`。與上面 agent pane 的邏輯完全分開，失敗只記 warn。
    match app.scan_panes_snapshot(host, &snapshot).await {
        Ok(scan) if !scan.complete => {
            // 有 pane 的事實這一輪讀不到：那幾列沿用上一輪，GC 與通知等下一輪讀得到再說（§6.5e）。
            tracing::info!(host, panes = scan.panes, "pane 事實不完整，這一輪不跑 pane GC 與通知");
        }
        Ok(scan) => {
            tracing::debug!(host, panes = scan.panes, "scanned non-agent panes");
            // 掃完才 GC：同一輪先有最新的歸屬與 last_output_at，再決定關誰（§6.5e）。
            match app.gc_panes(host).await {
                Ok(0) => {}
                Ok(k) => tracing::info!(host, closed = k, "pane GC 關掉了閒置的 shell pane"),
                Err(e) => tracing::warn!(host, error = ?e, "pane GC failed"),
            }
            if let Err(e) = app.notify_unowned_and_orphans(host).await {
                tracing::warn!(host, error = ?e, "pane 通知失敗");
            }
        }
        Err(e) => tracing::warn!(host, error = ?e, "non-agent pane scan failed"),
    }
    // agent 早就 idle、turn 還停在 in_flight：收尾並放行排在後面的 queued（AGM 2026-09-16）。
    app.sweep_stuck_turns(Some(host)).await;
    Ok(lost_autostart)
}

/// Returns the bot id. Reuse requires a native session under the same parent/session;
/// names are only used to choose an available display nickname.
#[allow(clippy::too_many_arguments)]
async fn adopt_child(
    app: &impl ReconcileHostEnv,
    host: &str,
    client: &crate::herdr::HerdrClient,
    session: &str,
    agent: &crate::herdr::AgentInfo,
    name: &str,
    parent: &db::Bot,
    child_name: &str,
    _raw_child_name: &str,
    kind: &str,
) -> anyhow::Result<String> {
    if app.isolated() {
        anyhow::bail!("隔離實例不認領既有子 agent（`{child_name}` 的 hook 指向別的資料目錄）；請在這顆 daemon 底下重開");
    }
    // Deletion locks the parent together with its snapshot of descendants. Adoption must share
    // that lock so it cannot insert a child after delete_bot/delete_project took their snapshot.
    let _parent_guard = app.bot_lock(&parent.id).await.lock_owned().await;
    // `parents` is a reconcile snapshot and may be stale while waiting for the lock. Recheck both
    // sides of the ownership boundary before updating an existing child or inserting a new one.
    let owner: Option<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT b.project_id, b.deleted_at, p.deleted_at FROM bots b JOIN projects p ON p.id = b.project_id WHERE b.id = ?",
    )
    .bind(&parent.id)
    .fetch_optional(app.db())
    .await?;
    if !matches!(owner, Some((ref project_id, None, None)) if project_id == &parent.project_id) {
        anyhow::bail!("parent bot or project was deleted before adopting `{child_name}`");
    }
    let now = db::now();
    let full_name = child_name_from_agent(name);
    let native_id = crate::lifecycle::dead_panes::herdr_session_id(agent);
    let existing_ids: Vec<String> = match native_id.as_deref() {
        Some(native) => sqlx::query_scalar(
            "SELECT b.id FROM bots b JOIN runs r ON r.bot_id=b.id
             WHERE b.project_id=? AND b.parent_bot_id=? AND b.managed_by='child' AND b.deleted_at IS NULL
             AND r.herdr_session=? AND r.native_session_id=? AND b.kind=?
             GROUP BY b.id",
        ).bind(&parent.project_id).bind(&parent.id).bind(session).bind(native).bind(kind)
            .fetch_all(app.db()).await?,
        None => vec![],
    };
    anyhow::ensure!(existing_ids.len() <= 1, "ambiguous child native identity");
    let existing = existing_ids.into_iter().next();
    let bot_id = match existing {
        Some(id) => {
            // The per-bot loop sorts that run out first; adopting now would trip `runs_one_active`.
            if let Some(r) = db::active_run(app.db(), &id).await? {
                anyhow::bail!("child `{child_name}` still has active run `{}` under agent `{:?}`", r.id, r.agent_name);
            }
            // kind 換了就把舊 identity 丟掉（SQLite 的 SET 右邊讀的是舊列值）：身份有 kind，
            // 換成別的 CLI 之後那個身份就不適用了（`identity_kind`）。
            sqlx::query(
                "UPDATE bots SET cwd = COALESCE(?, cwd),
                   identity = CASE WHEN kind = ? THEN identity ELSE NULL END,
                   model = CASE WHEN kind = ? THEN model ELSE NULL END,
                   effort = CASE WHEN kind = ? THEN effort ELSE NULL END,
                   fast = CASE WHEN kind = ? THEN fast ELSE 0 END,
                   kind = ? WHERE id = ?",
            )
                .bind(agent.cwd.clone())
                .bind(kind)
                .bind(kind)
                .bind(kind)
                .bind(kind)
                .bind(kind)
                .bind(&id)
                .execute(app.db())
                .await?;
            id
        }
        None => {
            let taken = |n: String| {
                let db = app.db().clone();
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
            // 只繼承同 kind 母 bot 的身份：codex 子 agent 抄到 claude 的 cc1，quota 就長出 `codex:cc1`。
            .bind(crate::identity_kind::child_identity(parent.identity.as_deref(), &parent.kind, kind))
            .bind(agent.cwd.clone())
            .bind(session)
            .bind(&parent.id)
            .bind(db::ulid())
            .bind(&now)
            .execute(app.db())
            .await?;
            bot_id
        }
    };
    alert_expert_child(app, parent, child_name, &bot_id).await?;
    let run_id = db::ulid();
    let status = agent.agent_status.normalized().as_str().to_string();
    // Publishing the run and activating its credential are one durable operation.
    let mut tx = app.db().begin().await?;
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, adopted, agent_name, herdr_session, native_session_id, terminal_id, started_at)
         VALUES (?,?,'running',?,?,?,?,1,?,?,?,?,?)",
    )
    .bind(&run_id)
    .bind(&bot_id)
    .bind(&status)
    .bind(&agent.workspace_id)
    .bind(&agent.pane_id)
    .bind(&agent.tab_id)
    .bind(name)
    .bind(session)
    .bind(&native_id)
    .bind(agent.terminal_id.as_deref().filter(|id| !id.trim().is_empty()))
    .bind(&now)
    .execute(&mut *tx)
    .await?;
    // The daemon recorded this tuple during authenticated spawn. Do not trust an environment
    // variable or name supplied by the child as proof of its parent or pane.
    let expiry = db::ts_sql("expires_at");
    let pending: Vec<(String, String, String)> = sqlx::query_as(&format!(
        "SELECT spawn_id,parent_bot_id,child_name FROM child_credentials WHERE host=? AND herdr_session=? AND pane_id=? AND state='pending' AND {expiry}>?"
    )).bind(host).bind(session).bind(&agent.pane_id).bind(db::now()).fetch_all(&mut *tx).await?;
    for (spawn_id, expected_parent, expected_name) in pending {
        if expected_parent == parent.id && expected_name == name {
            sqlx::query(&format!("UPDATE child_credentials SET state='active',child_bot_id=?,run_id=? WHERE spawn_id=? AND state='pending' AND {expiry}>?"))
                .bind(&bot_id).bind(&run_id).bind(&spawn_id).bind(db::now()).execute(&mut *tx).await?;
        } else {
            sqlx::query("UPDATE child_credentials SET state='revoked' WHERE spawn_id=? AND state='pending'").bind(&spawn_id).execute(&mut *tx).await?;
            let key = format!("child-credential-mismatch:{spawn_id}");
            let message = format!("child 憑證的 parent 或名稱與認領結果不符：pane {}、agent {}", agent.pane_id, name);
            crate::supervisor_inbox::push_inbox_tx(&mut tx, &key, "ops_alert", None, Some(&parent.id), None, &json!({"message":message,"spawn_id":spawn_id})).await?;
        }
    }
    tx.commit().await?;
    app.watch_pane_on_session(host, session, &agent.pane_id).await;
    // The pane is the only source for both its conversation (§4.3) and its model (argv).
    app.spawn_adopted_capture(&run_id, &bot_id);
    if let Ok(Some(child)) = db::bot(app.db(), &bot_id).await {
        sync_pane_model(app, host, client, &child, agent).await;
    }
    Ok(bot_id)
}

async fn alert_expert_child(app: &(impl Db + Emit + TurnCommands), parent: &db::Bot, child_name: &str, bot_id: &str) -> anyhow::Result<()> {
    if let Some(project) = db::project(app.db(), &parent.project_id).await? {
        if project.mode == "expert" && (parent.role.as_deref() != Some("manager") || parent.parent_bot_id.is_some() || parent.managed_by == "child") {
            let key = format!("expert-unauthorized-child:{bot_id}");
            let message = format!("專家模式發現非管理者 {} 開出 child {}；已追蹤此 child，請檢查派工來源。", parent.name, child_name);
            let inserted = crate::supervisor_inbox::push_inbox(app.db(), &key, "ops_alert", None, Some(&parent.id), None, &json!({"project_id":project.id,"child_id":bot_id,"message":message})).await?;
            if inserted.is_some() {
                let manager: Option<String> = sqlx::query_scalar("SELECT id FROM bots WHERE project_id=? AND role='manager' AND managed_by='user' AND deleted_at IS NULL LIMIT 1").bind(&project.id).fetch_optional(app.db()).await?;
                if let Some(manager) = manager {
                    let conv = db::conversation_id(app.db(), &manager).await?;
                    app.insert_message(&conv, None, "system", &message, "system", false, None).await?;
                }
            }
        }
    }
    Ok(())
}

/// 讀不出狀態列時，連續失敗的兩次讀取之間跳過的拍數上限（一拍約 10 秒，64 拍約 10 分鐘）。
const PROBE_SKIP_MAX: u32 = 64;

/// 收編的 codex run 讀不出狀態列的退避（#1652）：`misses` 是連續讀不出來的次數，`skip` 是接下來還要跳過的拍數。
#[derive(Clone, Copy, Default)]
struct StatusLineMiss {
    misses: u32,
    skip: u32,
}

/// run id → 退避狀態。讀得出來（之後 runtime 會寫進去，run 不再符合條件）就移除；daemon 重啟也會清空。
/// 用拍數而不是牆鐘，所以測試不必等時間。
fn status_line_misses() -> &'static std::sync::Mutex<HashMap<String, StatusLineMiss>> {
    static MISSES: std::sync::OnceLock<std::sync::Mutex<HashMap<String, StatusLineMiss>>> = std::sync::OnceLock::new();
    MISSES.get_or_init(Default::default)
}

/// `runs.runtime_*` for adopted codex runs (SPEC §4.4a: no silently unapplied values; `/fast` is
/// a toggle). Only fills NULLs — a start or live apply already wrote the truth.
async fn fill_codex_runtime(app: &(impl crate::capabilities::BotStatusEmit + crate::capabilities::Db + crate::capabilities::Emit), host: &str, client: &crate::herdr::HerdrClient) {
    let rows: Vec<(String, String)> = match sqlx::query_as(
        "SELECT r.id, r.pane_id FROM runs r JOIN bots b ON b.id = r.bot_id JOIN projects p ON p.id = b.project_id
         WHERE p.host = ? AND b.kind = 'codex' AND r.state = 'running' AND r.pane_id IS NOT NULL
         AND p.handed_off_to IS NULL
         AND r.runtime_model IS NULL AND r.runtime_effort IS NULL AND r.runtime_fast IS NULL",
    )
    .bind(host)
    .fetch_all(app.db())
    .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(host, error = ?e, "codex runtime probe: query failed");
            return;
        }
    };
    for (run_id, pane_id) in rows {
        // 退避中：這一拍不讀畫面，只把要跳過的拍數減一（#1652）。
        let backing_off = {
            let mut misses = status_line_misses().lock().unwrap_or_else(|e| e.into_inner());
            match misses.get_mut(&run_id) {
                Some(miss) if miss.skip > 0 => {
                    miss.skip -= 1;
                    true
                }
                _ => false,
            }
        };
        if backing_off {
            continue;
        }
        let Ok(read) = client.pane_read(&pane_id, "visible", 60).await else { continue };
        if crate::codex_live::parse_status_line(&read.text).is_none() {
            // 讀不出來：第 n 次失敗後跳過 2^(n-1) 拍，封頂 PROBE_SKIP_MAX。
            let mut misses = status_line_misses().lock().unwrap_or_else(|e| e.into_inner());
            let miss = misses.entry(run_id).or_default();
            miss.misses = miss.misses.saturating_add(1);
            miss.skip = (1u32 << (miss.misses - 1).min(6)).min(PROBE_SKIP_MAX);
            continue;
        }
        status_line_misses().lock().unwrap_or_else(|e| e.into_inner()).remove(&run_id);
        // 同巡邏的校正：收編前就在 TUI 換過模型的 child，設定也跟著狀態列（不然 argv 抄來的舊值永遠留著）。
        if crate::codex_live::correct_runtime_from_screen(app, &run_id, &read.text).await {
            tracing::info!(host, run = %run_id, "codex runtime read off an adopted pane's status line");
        }
    }
}

/// A child's `model` / `effort` (argv, grok title) and `identity` (pid, [`crate::pane_identity`]).
/// * **children only**: other bots' model is user config, projected back to `config.toml`.
/// * **fills, never corrects**: argv cannot see a later `/model`. Unparsed stays NULL.
/// `identity` does correct: nobody set a child's account, the adopt copied the parent's.
async fn sync_pane_model(app: &impl ReconcileHostEnv, host: &str, client: &crate::herdr::HerdrClient, bot: &db::Bot, agent: &crate::herdr::AgentInfo) {
    if bot.managed_by != "child" {
        return;
    }
    // codex：畫面已經記下 `runtime_effort`（具體值，或明確的 `default`）就以畫面為準，不再從 argv 補 effort。
    // argv 是啟動時的快照；子 agent 清成 CLI 預設（SPEC §4.4a）之後，下一輪對帳不能把舊的 high 補回來。
    // 「已知」＝active run 的 `runtime_effort` 不是 NULL（具體值或 `default`）；NULL 才是未知，才從 argv 補。
    let screen_effort_known = bot.kind == "codex"
        && matches!(db::active_run(app.db(), &bot.id).await, Ok(Some(run)) if run.runtime_effort.is_some());
    let want_effort = bot.effort.is_none() && !screen_effort_known;
    let want_model = bot.model.is_none() || want_effort;
    let want_identity = crate::pane_identity::probe_due(host, &bot.id, &agent.pane_id);
    // #393：子 agent 用 `-c service_tier="priority"` 起的，bots.fast 也要記成 1。model／effort 已有值時上面的
    // `want_model` 是 false，fast 就永遠沒被補到，UI 平白亮「fast 需重啟」。只在剛收編的窗口內補（不然使用者
    // 之後把 fast 關掉、argv 還是 priority，這裡會反過來把它改回去）。
    let screen_fast_known = bot.kind == "codex"
        && matches!(db::active_run(app.db(), &bot.id).await, Ok(Some(run)) if run.runtime_fast.is_some());
    let want_fast = bot.kind == "codex" && bot.fast == 0 && !screen_fast_known && fresh_adoption(app, &bot.id).await;
    if !want_model && !want_identity && !want_fast {
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
        .find(|p| kind_from_argv(&p.argv) == Some(bot.kind.as_str()))
        .or_else(|| procs.iter().find(|p| !p.argv.is_empty()));
    if want_identity {
        app.sync_child_identity(host, bot, &agent.pane_id, cli.and_then(|p| p.pid)).await;
    }
    if !want_model && !want_fast {
        return;
    }
    let argv: &[String] = cli.map(|p| p.argv.as_slice()).unwrap_or(&[]);
    let (mut model, mut effort) = crate::models::model_effort_from_argv(&bot.kind, argv);
    let fast = crate::models::fast_from_argv(&bot.kind, argv)
        .filter(|fast| *fast && bot.fast == 0 && !screen_fast_known)
        .map(|_| 1_i64);
    if bot.kind == "grok" && (model.is_none() || effort.is_none()) {
        let (tm, te) = crate::models::grok_title_model_effort(agent.terminal_title_stripped.as_deref().unwrap_or(""));
        model = model.or(tm);
        effort = effort.or(te);
    }
    // claude's argv rarely has `--effort`; resolve the account default like the "預設" hint does.
    if bot.kind == "claude" && effort.is_none() && bot.effort.is_none() {
        if let Some(alias) = model.as_deref().or(bot.model.as_deref()) {
            // 讀不到設定檔就留空、下一輪對帳再讀：記下內建預設值等於把猜的當成事實，之後沒人會再改它（#268）。
            match app.claude_default_effort(host, bot.identity.as_deref(), alias).await {
                Ok(e) => effort = Some(e),
                Err(e) => tracing::warn!(bot = %bot.name, host, error = %format!("{e:#}"), "cannot read the claude settings; child effort left unset"),
            }
        }
    }
    // agy：`--model` 就是啟動時實際用的模型（強度在 slug 裡），run 也記；只補空的（網頁沒有 runtime 會畫成「CLI 預設」）。
    if let (true, Some(m)) = (bot.kind == "agy", model.as_deref()) {
        let _ = sqlx::query("UPDATE runs SET runtime_model = ? WHERE bot_id = ? AND state IN ('starting','running') AND runtime_model IS NULL")
            .bind(m)
            .bind(&bot.id)
            .execute(app.db())
            .await;
    }
    let model = model.filter(|_| bot.model.is_none());
    let effort = effort.filter(|_| want_effort);
    if model.is_none() && effort.is_none() && fast.is_none() {
        return;
    }
    // 查 run 與寫入之間畫面可能剛記下 `runtime_effort`（TOCTOU）：codex 的 effort 補值在 SQL 裡再判一次 active run，
    // 已有記錄就保留現值，不讓舊 argv 覆寫剛切回的 default。
    let codex = i64::from(bot.kind == "codex");
    if let Err(e) = sqlx::query(
        "UPDATE bots SET model = COALESCE(?, model),
                effort = CASE WHEN ? = 1 AND EXISTS (
                    SELECT 1 FROM runs WHERE runs.bot_id = bots.id
                      AND runs.state IN ('starting','running','stopping') AND runs.runtime_effort IS NOT NULL
                ) THEN effort ELSE COALESCE(?, effort) END,
                fast = CASE WHEN kind = 'codex' AND EXISTS (
                    SELECT 1 FROM runs WHERE runs.bot_id = bots.id
                      AND runs.state IN ('starting','running','stopping') AND runs.runtime_fast IS NOT NULL
                ) THEN fast ELSE COALESCE(?, fast) END
          WHERE id = ?",
    )
        .bind(&model)
        .bind(codex)
        .bind(&effort)
        .bind(fast)
        .bind(&bot.id)
        .execute(app.db())
        .await
    {
        tracing::warn!(bot = %bot.name, error = ?e, "cannot record the model a child agent is running");
        return;
    }
    tracing::info!(bot = %bot.name, pane = %agent.pane_id, ?model, ?effort, ?fast, "reconcile: read the child's model off its argv");
    app.emit("bot_changed", json!({"bot_id": bot.id})).await;
}

/// 收編後這段時間內才會從 argv 補 fast（見 `sync_pane_model`）。
const FRESH_ADOPTION_WINDOW: chrono::Duration = chrono::Duration::minutes(10);

/// 純判斷：run 是 `started_at`、現在 `now`，還在收編窗口內嗎？讀不懂時間就當不在窗口內。
fn within_fresh_window(started_at: &str, now: chrono::DateTime<chrono::Utc>) -> bool {
    chrono::DateTime::parse_from_rfc3339(started_at)
        .map(|t| now.signed_duration_since(t.with_timezone(&chrono::Utc)) < FRESH_ADOPTION_WINDOW)
        .unwrap_or(false)
}

async fn fresh_adoption(app: &impl crate::capabilities::Db, bot_id: &str) -> bool {
    match db::active_run(app.db(), bot_id).await {
        Ok(Some(run)) => within_fresh_window(&run.started_at, chrono::Utc::now()),
        _ => false,
    }
}

#[cfg(test)]
#[path = "reconcile_tests/fast_adoption_tests.rs"]
mod fast_adoption_tests;

/// `sync_pane_model` 的 effort 補值與 codex 畫面校正的兩輪互動（SPEC §4.4a）：清成 CLI 預設之後，argv 的舊 high 不能再補回來。
#[cfg(test)]
#[path = "reconcile_tests/sync_pane_model_effort_tests.rs"]
mod sync_pane_model_effort_tests;

#[cfg(test)]
#[path = "reconcile_tests/autostart_tests.rs"]
mod autostart_tests;

#[cfg(test)]
#[path = "reconcile_tests/compat_tests.rs"]
mod compat_tests;

/// snapshot 的 `workspaces` 暫時是空陣列、pane 還掛著那個 workspace_id：不能把專案映射清成 NULL。
#[cfg(test)]
#[path = "reconcile_tests/snapshot_workspace_tests.rs"]
mod snapshot_workspace_tests;

#[cfg(test)]
#[path = "reconcile_tests/codex_runtime_probe_tests.rs"]
mod codex_runtime_probe_tests;

#[cfg(test)]
#[path = "reconcile_tests/expert_alert_tests.rs"]
mod expert_alert_tests;

#[cfg(test)]
#[path = "reconcile_tests/child_credential_e2e_tests.rs"]
mod child_credential_e2e_tests;
