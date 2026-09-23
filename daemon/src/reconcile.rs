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

/// `host` 上 `autostart = true` 且沒有 active Run 的 bot 走 §6.2，一輪。回 `true`＝每一顆都判斷完了。
///
/// 一定要在對帳**之後**才叫：否則會把 herdr 上還活著、只是 DB 還沒認回來的那顆再開一次。
///
/// 讀不到不當成沒有（#209）：清單讀不到＝這一輪一顆都不起、`owed` 維持原樣；某顆讀不到就只跳過那顆、記進 `owed`，
/// 其他照起。`owed`：`None`＝清單還沒讀到過（全部都要看）；`Some`＝上一輪沒判斷完的那幾顆，這一輪只看它們。
async fn autostart_pass(app: &Arc<App>, host: &str, owed: &mut Option<std::collections::HashSet<String>>, since: &str) -> bool {
    let bots = match db::live_bots(&app.db).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(host, error = ?e, "autostart: cannot list bots; none started this pass, will try again");
            return false;
        }
    };
    let mut still = std::collections::HashSet::new();
    for bot in bots {
        if bot.autostart != 1 || owed.as_ref().is_some_and(|o| !o.contains(&bot.id)) {
            continue;
        }
        if let Err(e) = autostart_one(app, host, &bot, since).await {
            tracing::warn!(host, bot = %bot.name, error = ?e, "autostart: cannot tell whether this bot should start; will look again");
            still.insert(bot.id.clone());
        }
    }
    let done = still.is_empty();
    *owed = Some(still);
    done
}

/// 一顆 bot 要不要起、起它。`Err`＝判斷要用的東西讀不到（起失敗不算：那是 `start_bot` 的結論，照舊只記 log）。
async fn autostart_one(app: &Arc<App>, host: &str, bot: &db::Bot, since: &str) -> Result<()> {
    // 讀不到主機不能當成本機：那會在本機這一輪對別台的 bot 下 `start_bot`，而那台可能還沒連上、還沒對帳。
    let bot_host = db::bot_host(&app.db, &bot.id).await?;
    if bot_host != host {
        return Ok(());
    }
    if !app.host_connected(host).await {
        tracing::info!(bot = %bot.name, host, "autostart skipped: host not connected");
        return Ok(());
    }
    // 讀不到 active run 也不能當成沒在跑。
    if db::active_run(&app.db, &bot.id).await?.is_some() {
        return Ok(());
    }
    // 第一次嘗試之後才有 run 的：使用者（或別的路）已經動過它，重試不再替它啟動——停掉的就是要它停。
    let touched: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE bot_id = ? AND started_at >= ?")
        .bind(&bot.id)
        .bind(since)
        .fetch_one(&app.db)
        .await?;
    if touched > 0 {
        return Ok(());
    }
    tracing::info!(bot = %bot.name, host, "autostart");
    if let Err(e) = crate::lifecycle::start_bot(app, &bot.id).await {
        tracing::error!(bot = %bot.name, error = ?e, "autostart failed");
    }
    Ok(())
}

/// 對帳做完之後才叫的 autostart 入口（§6.1 第 6 步，review 2026-09-16 core 5）。回 `true`＝這次真的跑了。
///
/// - **對帳沒成功就不跑**：不知道 herdr 上哪些 agent 其實還活著，開下去就是同一顆 bot 兩個 agent。
/// - **每台主機在這顆 daemon 的一生只跑一次**：遠端 ssh 斷線重連會再走一次「連上」，而 `stop` 不會改 `autostart`——
///   使用者停掉的 autostart bot 在筆電睡醒重連後被重開、開始吃額度，本機同樣設定的卻不會。對帳失敗不算數，之後任一次成功的對帳（supervisor 連上、或全域訂閱建好後那次，#259）補跑。
pub async fn autostart_after_reconcile(app: &Arc<App>, host: &str, reconciled: bool) -> bool {
    if !reconciled {
        tracing::warn!(host, "autostart skipped: reconcile did not succeed; will retry on the next successful connect");
        return false;
    }
    // 被打斷的重啟先補完（#355 P2）：要在下面 autostart 判斷「使用者停掉的」之前——不然剛停掉的那顆會被當成使用者要它停。
    // 每次對帳成功都做（重啟 intent 隨時可能產生），不受下面「每台主機一生一次」限制。
    crate::restart_intents::recover_host(app, host).await;
    crate::delete_intents::recover_host(app, host).await;
    crate::promote_intents::recover_host(app, host).await;
    if !app.autostarted_hosts.lock().await.insert(host.to_string()) {
        tracing::info!(host, "autostart already ran for this host in this daemon's lifetime; not restarting stopped bots");
        return false;
    }
    // 讀不到的部分背景補到判斷完（#209）：本機不會有「下次連上」，只等重連的話 AGM 在內的 autostart bot 要到下次重啟才起。
    // 主機照樣只算跑過一次：補的只是這一次還沒判斷完的，已經判斷過（起了、或本來就在跑）的不再碰。
    let since = db::now();
    let mut owed = None;
    if !autostart_pass(app, host, &mut owed, &since).await {
        let (app, host) = (app.clone(), host.to_string());
        tokio::spawn(async move {
            for attempt in 0.. {
                tokio::time::sleep(recovery_retry_delay(attempt)).await;
                if autostart_pass(&app, &host, &mut owed, &since).await {
                    tracing::info!(host = %host, "autostart caught up");
                    return;
                }
            }
        });
    }
    // bot 沒在跑時收下、還在等它起來的訊息（issue #122）：重啟前那次啟動可能沒做完，這裡再替它起一次。
    crate::lifecycle::start_send::resume_after_boot(app, host).await;
    true
}

/// A Turn that outlives a restart has no poller: no live bubble, and nothing completes it if its
/// hook never arrives. Re-arm every in-flight Turn once the runs are adopted.
///
/// 開機只跑這一次，所以**讀不到不算做完**（#75 重開）：真相都在 DB（in-flight 回合、`next_flush_at`、送到一半的那一筆），
/// 把它變回 poller／timer 的卻只有這一步——這一步讀不到，那個回合重啟後就沒人盯，閒著的 bot 也不會再有事件叫醒排著的那一則。
/// 讀不到的部分記成欠著，背景照退避一直補到做完。做完的不再碰（poller／watchdog 各只掛一次）；重啟之後才開始的 run、
/// 之後才建立或才結束的回合是這個行程自己的，補收不碰。
pub async fn rearm_progress(app: &Arc<App>) {
    let mut owed = Recovery::new();
    if owed.pass(app).await {
        return;
    }
    tracing::warn!("startup recovery could not read everything it needs; retrying in the background until it can");
    let app = app.clone();
    tokio::spawn(async move {
        for attempt in 0.. {
            tokio::time::sleep(recovery_retry_delay(attempt)).await;
            if owed.pass(&app).await {
                tracing::info!(retries = attempt + 1, "startup recovery caught up");
                return;
            }
        }
    });
}

/// 開機恢復讀不到之後，第 `attempt` 次重試前等多久：很快就好的多半是 busy，一直讀不到就放慢到每分鐘一次。
pub(crate) fn recovery_retry_delay(attempt: usize) -> std::time::Duration {
    if cfg!(test) {
        return std::time::Duration::from_millis(20);
    }
    const SECS: [u64; 5] = [2, 5, 15, 30, 60];
    std::time::Duration::from_secs(SECS[attempt.min(SECS.len() - 1)])
}

/// 開機恢復還欠著的部分。
struct Recovery {
    /// 開機那一刻。之後才開始的 run、之後才建立的插隊送出、之後才結束的 run 都是這個行程自己的帳。
    boot: String,
    /// 還沒把 in-flight 回合接回來的 run；`None`＝連 run 清單都還沒讀到。
    runs: Option<std::collections::HashSet<String>>,
    queue: bool,
    send_nows: bool,
    ended_runs: bool,
}

impl Recovery {
    fn new() -> Self {
        Self { boot: db::now(), runs: None, queue: true, send_nows: true, ended_runs: true }
    }

    /// 補一輪；回 `true`＝什麼都不欠了。
    async fn pass(&mut self, app: &Arc<App>) -> bool {
        // 插隊送出途中停掉、還沒掛上 run 的那一則（#120）。排在接回 poller 之前：鍵其實生效了的那一則會在這裡掛上 run（#229），
        // 接回的才是它、不是已經被它打斷的那一筆。
        if self.send_nows {
            self.send_nows = !crate::lifecycle::adopt_unbound_send_nows(app, &self.boot).await;
        }
        self.rearm_in_flight(app).await;
        // Queued prompts in a backoff lost their timers with the old process (SPEC §4.4a).
        if self.queue {
            match crate::lifecycle::rearm_queue_retries(app).await {
                Ok(_) => self.queue = false,
                Err(e) => tracing::warn!(error = %e, "cannot re-arm queued prompt retries yet"),
            }
        }
        // run 已經結束、收尾卻欠著（帳只在記憶體，重啟就沒了）的那一筆（#156）。
        if self.ended_runs {
            self.ended_runs = !crate::lifecycle::adopt_turns_of_ended_runs(app, &self.boot).await;
        }
        self.runs.as_ref().is_some_and(|r| r.is_empty()) && !self.queue && !self.send_nows && !self.ended_runs
    }

    async fn rearm_in_flight(&mut self, app: &Arc<App>) {
        if self.runs.as_ref().is_some_and(|r| r.is_empty()) {
            return;
        }
        let runs = match crate::db::all_active_runs(&app.db).await {
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
            if owed && !rearm_run(app, &run).await {
                still.insert(run.id);
            }
        }
        // 清單上已經不在的 run（結束了）不再欠：它的回合歸結束那條路收。
        self.runs = Some(still);
    }
}

/// 一個 run 在飛的那一筆接回 poller（剛送出的再補 stall watchdog）。`false`＝這一輪讀寫不到，之後再補。
async fn rearm_run(app: &Arc<App>, run: &db::Run) -> bool {
    // 跟送出同一把鎖：這個行程送到一半的那一筆不會被當成重啟前的孤兒。
    let lock = app.bot_lock(&run.bot_id).await;
    let _g = lock.lock().await;
    // 這個行程欠著的送達結果先結清：DB 裡那一筆還是 pending，不能被當成重啟前送到一半的收成 unknown（#149 的帳）。
    if let Err(e) = crate::lifecycle::settle_owed_deliveries(app, &run.bot_id).await {
        tracing::warn!(run = %run.id, error = %e, "cannot settle this bot's owed delivery results yet; its in-flight turn waits");
        return false;
    }
    // 已經有人盯著這個 run（重試期間這個行程自己送出的回合掛了 poller）：不再掛一次。
    if app.progress_pollers.lock().await.contains_key(&run.id) {
        return true;
    }
    let turn = match crate::db::in_flight_turn(&app.db, &run.id).await {
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
    match crate::lifecycle::adopt_interrupted_on_restart(app, run, &turn).await {
        Ok(true) => return true,
        Ok(false) => {}
        Err(e) => {
            tracing::warn!(run = %run.id, turn = %turn.id, error = %e, "cannot check whether this turn was interrupted before the restart yet");
            return false;
        }
    }
    crate::lifecycle::arm_progress(app, &run.id, &run.bot_id, &turn.id).await;
    // 送出後幾秒內被重啟：補 Enter 與「畫面上找不到就重送」這兩層網都只活在上一個行程裡
    // （review 2026-09-16）。只對**剛送出**的補，不然會把幾小時前的 prompt 重送一次。
    // 「剛送出」看送出的時間：排隊的 turn 的 created_at 是排進佇列的時間，flush 可能晚了半小時（deliv L3）。
    if turn.delivery == "ok" && fresh_enough(turn.delivered_at.as_deref().unwrap_or(&turn.created_at)) {
        crate::lifecycle::arm_stall(app, &run.id, &run.bot_id, &turn.id).await;
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
async fn adopt_orphan_delivery(app: &Arc<App>, turn: &db::Turn) -> bool {
    if turn.delivery != "pending" {
        return true;
    }
    let n = match sqlx::query(
        "UPDATE turns SET delivery='unknown', auto_resend=0, delivered_at=COALESCE(delivered_at, ?)
          WHERE id = ? AND status='in_flight' AND delivery='pending'",
    )
    .bind(crate::db::now())
    .bind(&turn.id)
    .execute(&app.db)
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
        crate::lifecycle::emit_turn(app, &turn.id).await;
    }
    true
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
async fn refresh_child_kind(app: &Arc<App>, bot: &mut db::Bot, pane_id: &str, kind: &str) -> anyhow::Result<()> {
    if bot.managed_by != "child" || bot.kind == kind {
        return Ok(());
    }
    let mut tx = app.db.begin().await?;
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
        "UPDATE runs SET runtime_model = NULL, runtime_effort = NULL, runtime_fast = NULL
         WHERE bot_id = ? AND state IN ('starting','running','stopping')",
    )
    .bind(&bot.id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    crate::pane_identity::reset_probe(&bot.id, pane_id);
    bot.kind = kind.to_string();
    bot.identity = None;
    bot.model = None;
    bot.effort = None;
    bot.fast = 0;
    app.emit("bot_changed", json!({"bot_id": bot.id})).await;
    Ok(())
}

/// 子 agent 的 agent 不在了，能不能照 #60 退休它（#191）。兩條退休路徑共用這一支，免得一邊修、一邊漏。
///
/// 只有**確定沒在維護**才可以：herdr 重啟的那幾分鐘正是所有 pane 同時消失的時候，這時讀不到維護狀態就當成
/// 「沒在維護」，子 bot 被軟刪，pane 回來之後也接不回原對話與血緣。讀不到＝這一輪留著、晚一點再看。
async fn may_retire_child(app: &Arc<App>, host: &str, bot: &db::Bot) -> bool {
    match crate::child_reconcile_safety::retirement_block(&app.db, &bot.id).await {
        Ok(Some(reason)) => {
            tracing::info!(host, bot = %bot.name, reason, "reconcile: child kept by a persisted restore/restart guard");
            return false;
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(host, bot = %bot.name, error = ?e, "reconcile: cannot read child retirement guards; child kept this pass");
            schedule_deferred_pass(app, host);
            return false;
        }
    }
    match crate::herdr_maintenance::active(app).await {
        Ok(None) => true,
        Ok(Some(_)) => {
            tracing::info!(host, bot = %bot.name, "reconcile: herdr maintenance in progress, child kept");
            false
        }
        Err(e) => {
            tracing::warn!(host, bot = %bot.name, error = ?e,
                "reconcile: cannot read the herdr maintenance state; child kept this pass, will look again");
            schedule_deferred_pass(app, host);
            false
        }
    }
}

/// 延後的那一輪多久之後補跑。
const DEFERRED_PASS_DELAY: std::time::Duration =
    if cfg!(test) { std::time::Duration::from_millis(50) } else { std::time::Duration::from_secs(15) };

/// 這一輪有一件事因為讀不到而延後了（#94、#191）。對帳平常只在事件上跑（連上、`pane.agent_detected`、子 agent 的
/// pane 關掉），延後的那一件若等不到下一個事件就一直掛著——排一輪晚一點的補跑。同一台主機同時只排一輪；補跑還是
/// 讀不到就會再排，整輪失敗（DB、herdr）也再排；那台主機斷線或不在設定裡就停，重新連上時本來就會對帳。
fn schedule_deferred_pass(app: &Arc<App>, host: &str) {
    static PENDING: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> = std::sync::OnceLock::new();
    let pending = PENDING.get_or_init(Default::default);
    // 測試共用這個行程：key 帶資料目錄，不同的 App 才不會互相吃掉。
    let key = format!("{}\u{0}{host}", app.data_dir.display());
    if !pending.lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone()) {
        return;
    }
    let (app, host) = (app.clone(), host.to_string());
    tokio::spawn(async move {
        tokio::time::sleep(DEFERRED_PASS_DELAY).await;
        pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
        if let Err(e) = reconcile_host(&app, &host).await {
            if app.session_for_host(&host).await.is_some() && app.host_connected(&host).await {
                tracing::warn!(host = %host, error = ?e, "deferred reconcile pass failed; trying again");
                schedule_deferred_pass(&app, &host);
            }
        }
    });
}

/// 同一台主機的對帳**一輪一輪來**：事件驅動的一輪與延後補跑的一輪（`schedule_deferred_pass`）在負載高時會重疊，後到的那一輪
/// `claimed` 讀在前一輪認領之前、spawn hint 卻讀在前一輪 `consume` 之後，就退回同 tab 推斷——已經認領的子 agent 又掛到別顆底下、
/// 因為短名字被占用長出重複 bot（`proj-xxxx-k2`）。key 帶資料目錄：測試共用同一個行程，不同的 App 不能互相排隊。
async fn host_pass_lock(app: &Arc<App>, host: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: std::sync::OnceLock<std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = std::sync::OnceLock::new();
    let key = format!("{}\u{0}{host}", app.data_dir.display());
    LOCKS.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner()).entry(key).or_default().clone()
}

pub async fn reconcile_host(app: &Arc<App>, host: &str) -> Result<()> {
    let lock = host_pass_lock(app, host).await;
    let _pass = lock.lock().await;
    reconcile_host_locked(app, host).await
}

async fn reconcile_host_locked(app: &Arc<App>, host: &str) -> Result<()> {
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
        let mut bot = bot;
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
        if bot.managed_by == "child" {
            if let Some(agent) = found.as_ref() {
                let kind = child_kind(&client, agent, &bot.kind).await;
                refresh_child_kind(app, &mut bot, &agent.pane_id, &kind).await?;
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
                let exit = crate::lifecycle::mark_run_exited(app, &run.id, "agent not found during reconcile").await;
                // A child exists only as long as its pane; its conversation is kept.
                // 計畫中的 herdr 重啟期間不算：所有 pane 同時消失不是子 agent 做完了（§6.5.2）。
                // 維護結束時仍沒接回的，由 `herdr_maintenance` 照同一條規則退休。
                // run 的結束沒寫進去（#191）：DB 裡它還在跑，這時退休就是一顆刪掉的 bot 掛著活的 run。這一輪不動，
                // 晚一點再對一次帳：結束寫得進去之後，子 agent 走下面的 `(None, None)` 退休。
                if exit == crate::lifecycle::RunExit::NotRecorded {
                    tracing::warn!(host, bot = %bot.name, run = %run.id, "reconcile: the run's exit was not recorded; bot left as is, will look again");
                    schedule_deferred_pass(app, host);
                } else if bot.managed_by == "child" && may_retire_child(app, host, &bot).await {
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
                if bot.managed_by == "child" && may_retire_child(app, host, &bot).await {
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
    //
    // issue #94：一顆 parent 一次在**新** tab 裡開好幾顆子代理時，「同 tab」這條線索會在第一顆被認領
    // 之後把它自己也變成那個 tab 的候選 parent，後面幾顆因此一顆掛一顆串成鏈、還可能因為短名字被占用
    // 而另外建出重複 bot（2026-09-17 使用者實戰）。`spawn_hints`（一顆 bot 自己的 `PostToolUse` 事件流
    // 看到 `herdr pane split`／`agent start` 的 stdout）給的是「這個 pane_id 就是我剛開的」這個事實，
    // 比同 tab／名字前綴更早也更精確，排最前面；查不到（CLI 版本太舊、這顆 bot 沒有 hook——典型是
    // child 自己開孫代，§4.3 一律沒有 hook——或根本不是這樣開的）就照舊退回血緣／前綴推斷，那條路一個
    // 位元組都沒動，也是唯一在 CLI 不發 hook 時能依靠的線索。
    //
    // 讀不到 hint 不等於沒有 hint（#94 重開）：hint 可能好好地在表裡，只是這一輪 SELECT 失敗；這時退回同 tab／前綴推斷，
    // 正好把新 tab 裡的一排子代理串回鏈、掛錯 parent。所以這一輪一顆都不認領，排一輪晚一點的補跑；`Ok(空)` 才是真的沒有。
    crate::spawn_hints::prune_stale(app).await;
    let (hints, strangers): (HashMap<String, String>, &[crate::herdr::AgentInfo]) =
        match crate::spawn_hints::for_host(app, host).await {
            Ok(h) => (h, agents.as_slice()),
            Err(e) => {
                if agents.iter().any(|a| a.name.as_deref().is_some_and(|n| !claimed.contains(n))) {
                    tracing::warn!(host, error = ?e, "reconcile: cannot read spawn hints; adopting no new child this pass, will look again");
                    schedule_deferred_pass(app, host);
                }
                (HashMap::new(), &[][..])
            }
        };
    let mut new_children = 0usize;
    for agent in strangers.iter() {
        let Some(name) = agent.name.as_deref() else { continue };
        if claimed.contains(name) {
            continue;
        }
        let by_hint = hints.get(&agent.pane_id).and_then(|bid| parents.iter().find(|p| &p.bot.id == bid));
        let by_tab = parents
            .iter()
            .filter(|p| p.tab_id.as_deref() == Some(agent.tab_id.as_str()))
            .max_by_key(|p| (prefix_score(&p.agent_name, name), u8::from(p.bot.managed_by != "child")));
        let by_prefix =
            parents.iter().filter(|p| prefix_score(&p.agent_name, name) > 0).max_by_key(|p| p.agent_name.len());
        let Some(entry) = by_hint.or(by_tab).or(by_prefix) else { continue };
        let (parent_name, parent) = (&entry.agent_name, &entry.bot);
        let child_name = match prefix_score(parent_name, name) {
            0 => child_name_from_agent(name),
            n => {
                let suffix = &name[n + 1..];
                if crate::config::valid_bot_name(suffix) { suffix.to_string() } else { child_name_from_agent(name) }
            }
        };
        let kind = child_kind(&client, agent, &parent.kind).await;
        // One failed child is logged and skipped; a `?` here aborted the whole host (review 2026-09-12 #2).
        match adopt_child(app, host, &client, &session, agent, name, parent, &child_name, &kind).await {
            Ok(bot_id) => {
                claimed.insert(name.to_string());
                if by_hint.is_some() {
                    crate::spawn_hints::consume(app, &agent.pane_id).await;
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
    match crate::panes::scan_snapshot(app, host, &snapshot).await {
        Ok(scan) if !scan.complete => {
            // 有 pane 的事實這一輪讀不到：那幾列沿用上一輪，GC 與通知等下一輪讀得到再說（§6.5e）。
            tracing::info!(host, panes = scan.panes, "pane 事實不完整，這一輪不跑 pane GC 與通知");
        }
        Ok(scan) => {
            tracing::debug!(host, panes = scan.panes, "scanned non-agent panes");
            // 掃完才 GC：同一輪先有最新的歸屬與 last_output_at，再決定關誰（§6.5e）。
            match crate::panes::gc_host(app, host).await {
                Ok(0) => {}
                Ok(k) => tracing::info!(host, closed = k, "pane GC 關掉了閒置的 shell pane"),
                Err(e) => tracing::warn!(host, error = ?e, "pane GC failed"),
            }
            if let Err(e) = crate::panes::notify_unowned_and_orphans(app, host).await {
                tracing::warn!(host, error = ?e, "pane 通知失敗");
            }
        }
        Err(e) => tracing::warn!(host, error = ?e, "non-agent pane scan failed"),
    }
    // agent 早就 idle、turn 還停在 in_flight：收尾並放行排在後面的 queued（AGM 2026-09-16）。
    crate::lifecycle::sweep_stuck_turns(app, Some(host)).await;
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
    // #393：子 agent 用 `-c service_tier="priority"` 起的，bots.fast 也要記成 1。model／effort 已有值時上面的
    // `want_model` 是 false，fast 就永遠沒被補到，UI 平白亮「fast 需重啟」。只在剛收編的窗口內補（不然使用者
    // 之後把 fast 關掉、argv 還是 priority，這裡會反過來把它改回去）。
    let want_fast = bot.kind == "codex" && bot.fast == 0 && fresh_adoption(app, &bot.id).await;
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
        crate::pane_identity::sync_child_identity(app, host, bot, &agent.pane_id, cli.and_then(|p| p.pid)).await;
    }
    if !want_model && !want_fast {
        return;
    }
    let argv: &[String] = cli.map(|p| p.argv.as_slice()).unwrap_or(&[]);
    let (mut model, mut effort) = crate::models::model_effort_from_argv(&bot.kind, argv);
    let fast = crate::models::fast_from_argv(&bot.kind, argv)
        .filter(|fast| *fast && bot.fast == 0)
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
            match crate::models::claude_default_effort(app, host, bot.identity.as_deref(), alias).await {
                Ok(e) => effort = Some(e),
                Err(e) => tracing::warn!(bot = %bot.name, host, error = %format!("{e:#}"), "cannot read the claude settings; child effort left unset"),
            }
        }
    }
    let model = model.filter(|_| bot.model.is_none());
    let effort = effort.filter(|_| bot.effort.is_none());
    if model.is_none() && effort.is_none() && fast.is_none() {
        return;
    }
    if let Err(e) = sqlx::query("UPDATE bots SET model = COALESCE(?, model), effort = COALESCE(?, effort), fast = COALESCE(?, fast) WHERE id = ?")
        .bind(&model)
        .bind(&effort)
        .bind(fast)
        .bind(&bot.id)
        .execute(&app.db)
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

async fn fresh_adoption(app: &Arc<App>, bot_id: &str) -> bool {
    match db::active_run(&app.db, bot_id).await {
        Ok(Some(run)) => within_fresh_window(&run.started_at, chrono::Utc::now()),
        _ => false,
    }
}

#[cfg(test)]
mod fast_adoption_tests {
    use super::within_fresh_window;
    use chrono::{Duration, Utc};

    /// #393：收編後 10 分鐘內才從 argv 補 fast；之後使用者改的設定不能被 argv 蓋回去。
    #[test]
    fn only_a_recently_adopted_run_may_take_fast_from_its_argv() {
        let now = Utc::now();
        let at = |mins: i64| (now - Duration::minutes(mins)).to_rfc3339();
        assert!(within_fresh_window(&at(1), now));
        assert!(within_fresh_window(&at(9), now));
        assert!(!within_fresh_window(&at(11), now));
        assert!(!within_fresh_window("not a time", now), "讀不懂就不補");
    }
}

#[cfg(test)]
mod autostart_tests {
    /// review 2026-09-16 core 5：遠端每次重連都走一次「連上」分支。使用者停掉的 autostart bot（`stop` 不改 `autostart`）
    /// 不能在筆電睡醒重連後被重開；對帳失敗的那一次也不能跑（不知道哪些 agent 其實還活著）。
    #[tokio::test]
    async fn autostart_runs_once_per_host_and_only_after_a_successful_reconcile() {
        let env = crate::testing::env().await;
        let app = &env.app;
        assert!(!super::autostart_after_reconcile(app, "m4p", false).await, "對帳失敗不跑");
        assert!(super::autostart_after_reconcile(app, "m4p", true).await, "失敗那次不算數：下次連上照跑");
        assert!(!super::autostart_after_reconcile(app, "m4p", true).await, "重連不再跑");
        assert!(super::autostart_after_reconcile(app, "local", true).await, "每台主機各算各的");
    }

    use crate::db;
    use crate::state::App;
    use crate::testing as tt;
    use std::sync::Arc;

    async fn autostart_bot(env: &tt::Env, project: &str, name: &str) -> String {
        let bot = tt::claude_bot(&env.app, project, name).await;
        sqlx::query("UPDATE bots SET autostart=1 WHERE id=?").bind(&bot.id).execute(&env.app.db).await.unwrap();
        bot.id
    }

    async fn running(app: &Arc<App>, bot: &str) -> bool {
        db::active_run(&app.db, bot).await.unwrap().is_some()
    }

    async fn eventually_running(app: &Arc<App>, bot: &str) -> bool {
        crate::testing::eventually!(running(app, bot).await)
    }

    /// **#209.** 開機那一刻 bot 清單讀不到：以前變成空清單、一顆都不起，主機卻已經標成「跑過」——本機不會有下一次連上，
    /// AGM 在內的 autostart bot 要到下次重啟才起。現在這一輪不起，DB 恢復後背景自己補起，而且主機仍只算跑過一次。
    #[tokio::test]
    async fn an_autostart_that_cannot_list_bots_starts_them_once_the_db_can() {
        let env = tt::env().await;
        let app = &env.app;
        let bot = autostart_bot(&env, &env.project_id, "auto").await;

        sqlx::query("ALTER TABLE bots RENAME TO bots_unreadable").execute(&app.db).await.unwrap();
        assert!(super::autostart_after_reconcile(app, "local", true).await);
        sqlx::query("ALTER TABLE bots_unreadable RENAME TO bots").execute(&app.db).await.unwrap();
        assert!(eventually_running(app, &bot).await, "DB 恢復之後，沒有任何重連也要起來");
        assert!(!super::autostart_after_reconcile(app, "local", true).await, "主機照樣只算跑過一次");
        let runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE bot_id=?").bind(&bot).fetch_one(&app.db).await.unwrap();
        assert_eq!(runs, 1, "只起一次");
    }

    /// #209 同一支：讀不到 active run 也不當成沒在跑——這一輪不起、記成欠著，讀得到之後再判斷（以前落進 `start_bot`、
    /// 在鎖裡再讀一次失敗，之後就再也沒人管）。這一列讀不出來（欄位型別錯，跟 I/O 錯誤一樣是 `Err`）；修好時它已經結束。
    #[tokio::test]
    async fn an_autostart_that_cannot_read_active_runs_retries_that_bot() {
        let env = tt::env().await;
        let app = &env.app;
        let bot = autostart_bot(&env, &env.project_id, "auto").await;
        let run = db::ulid();
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,agent_name,started_at) VALUES (?,?,'running','idle',X'6B6964','2020-01-01T00:00:00.000Z')")
            .bind(&run)
            .bind(&bot)
            .execute(&app.db)
            .await
            .unwrap();

        assert!(super::autostart_after_reconcile(app, "local", true).await);
        let runs = || async { sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM runs WHERE bot_id=?").bind(&bot).fetch_one(&app.db).await.unwrap() };
        assert_eq!(runs().await, 1, "讀不到它在不在跑：不起");
        sqlx::query("UPDATE runs SET agent_name=NULL, state='exited', ended_at=? WHERE id=?").bind(db::now()).bind(&run).execute(&app.db).await.unwrap();
        assert!(eventually_running(app, &bot).await, "讀得到之後再判斷：確定沒在跑就起");
    }

    /// #209 同一支：某顆讀不到主機不能當成本機（那會對別台的 bot 下 start），只跳過那顆、其他照起；讀得到之後補起。
    /// 補的時候，使用者在這之間自己起過又停掉的那顆不再替它起——停掉的就是要它停。
    #[tokio::test]
    async fn an_autostart_bot_whose_host_is_unreadable_is_skipped_and_retried_alone() {
        let env = tt::env().await;
        let app = &env.app;
        let good = autostart_bot(&env, &env.project_id, "good").await;
        // 主機欄讀不出字串（型別錯，跟 I/O 錯誤一樣是 `Err`）：只有這個專案的 bot 讀不到主機。
        let broken = db::ulid();
        let repo = env.dir.join("broken");
        tt::git::init_repo(&repo);
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, ?, 'broken', X'6C6F63616C', ?)")
            .bind(&broken)
            .bind(repo.to_string_lossy().to_string())
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let later = autostart_bot(&env, &broken, "later").await;
        let stopped = autostart_bot(&env, &broken, "stopped").await;

        assert!(super::autostart_after_reconcile(app, "local", true).await);
        assert!(running(app, &good).await, "讀得到的照起");
        assert!(!running(app, &later).await && !running(app, &stopped).await, "讀不到主機：不當成本機起");

        // 這之間使用者自己起過 `stopped` 又停掉它。
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at,ended_at) VALUES (?,?,'stopped','idle',?,?)")
            .bind(db::ulid())
            .bind(&stopped)
            .bind(db::now())
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("UPDATE projects SET host='local' WHERE id=?").bind(&broken).execute(&app.db).await.unwrap();
        assert!(eventually_running(app, &later).await, "讀得到之後補起");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(!running(app, &stopped).await, "使用者停掉的不再替它起");
        let good_runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE bot_id=?").bind(&good).fetch_one(&app.db).await.unwrap();
        assert_eq!(good_runs, 1, "已經判斷過的不再碰");
    }
}

#[cfg(test)]
mod compat_tests {
    //! Old (shared-tab, `tab_id` NULL) and new bots side by side (SPEC §6.5).
    use crate::db;
    use crate::state::App;
    use crate::testing as tt;
    use serde_json::{json, Value};
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
        assert_eq!(t.auto_resend, 0, "鍵可能按過了：舊版本留下的可重送預設也關掉（#149）");
        assert!(t.delivered_at.is_some(), "送出最晚就是重啟那一刻：不退回 created_at（#149）");

        // 已經證出來送到的那種不要動它。
        sqlx::query("UPDATE turns SET delivery='ok' WHERE id=?").bind(&turn).execute(&app.db).await.unwrap();
        super::rearm_progress(app).await;
        let t = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn).fetch_one(&app.db).await.unwrap();
        assert_eq!(t.delivery, "ok");
    }

    /// review 2026-09-16 deliv L3：排隊半小時、剛剛才 flush 送出就遇上重啟的那一筆，watchdog 要補回來——
    /// 「剛送出」看 `delivered_at`，不看排隊時的 `created_at`；沒有 `delivered_at` 的舊列照舊看 `created_at`。
    #[tokio::test]
    async fn a_queued_turn_sent_just_before_a_restart_gets_its_watchdog_back() {
        let env = tt::env().await;
        let app = &env.app;
        let long_ago = (chrono::Utc::now() - chrono::Duration::minutes(30)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let mut runs = Vec::new();
        for (name, delivered_at) in [("flushed", Some(db::now())), ("old", None)] {
            let bot = a_bot(&env, name).await;
            let run = db::ulid();
            sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES (?,?,'running','idle',?)")
                .bind(&run).bind(&bot).bind(db::now()).execute(&app.db).await.unwrap();
            let conv = db::conversation_id(&app.db, &bot).await.unwrap();
            sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,created_at,delivered_at) VALUES (?,?,?,'web','in_flight','ok',?,?)")
                .bind(db::ulid()).bind(&conv).bind(&run).bind(&long_ago).bind(delivered_at).execute(&app.db).await.unwrap();
            runs.push(run);
        }
        super::rearm_progress(app).await;
        let timers = app.stall_timers.lock().await;
        assert!(timers.contains_key(&runs[0]), "排隊很久、剛送出：要補");
        assert!(!timers.contains_key(&runs[1]), "沒有 delivered_at 的舊列照舊看 created_at：半小時前的不補");
    }

    /// `delivered_at` 由 `mark_delivery` 寫、只記第一次：poller 事後補證據再記一次，不能讓舊的看起來像剛送出。
    #[tokio::test]
    async fn the_delivery_time_is_recorded_once() {
        let env = tt::env().await;
        let app = &env.app;
        let bot = a_bot(&env, "sent").await;
        let conv = db::conversation_id(&app.db, &bot).await.unwrap();
        let turn = db::ulid();
        let long_ago = (chrono::Utc::now() - chrono::Duration::minutes(30)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        sqlx::query("INSERT INTO turns (id,conversation_id,origin,status,delivery,created_at) VALUES (?,?,'web','in_flight','pending',?)")
            .bind(&turn).bind(&conv).bind(&long_ago).execute(&app.db).await.unwrap();
        let at = |app: Arc<App>, turn: String| async move {
            sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(turn).fetch_one(&app.db).await.unwrap().delivered_at
        };
        crate::lifecycle::mark_delivery(app, &turn, crate::lifecycle::Delivered::Handed.record().unwrap(), &db::now()).await.unwrap();
        let first = at(app.clone(), turn.clone()).await.expect("送出時記下");
        assert!(first > long_ago);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        crate::lifecycle::mark_delivery(app, &turn, crate::lifecycle::Delivered::Unverified.record().unwrap(), &db::now()).await.unwrap();
        assert_eq!(at(app.clone(), turn).await, Some(first), "只記第一次");
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

    // ---- #75 重開：開機恢復讀不到不算做完 ----

    fn ago(d: chrono::Duration) -> String {
        (chrono::Utc::now() - d).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    async fn a_run(app: &Arc<App>, bot: &str, state: &str, started_at: &str, ended_at: Option<&str>) -> String {
        let run = db::ulid();
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at,ended_at) VALUES (?,?,?,'idle',?,?)")
            .bind(&run)
            .bind(bot)
            .bind(state)
            .bind(started_at)
            .bind(ended_at)
            .execute(&app.db)
            .await
            .unwrap();
        run
    }

    /// 直接寫進 `table`（讀不到的那段時間 `turns` 改了名，要塞「重啟之後才有」的列就寫進改名後的表）。
    #[allow(clippy::too_many_arguments)]
    async fn a_turn(app: &Arc<App>, table: &str, bot: &str, run: Option<&str>, status: &str, delivery: &str, created_at: &str, delivered_at: Option<&str>) -> String {
        let conv = db::conversation_id(&app.db, bot).await.unwrap();
        let id = db::ulid();
        sqlx::query(&format!(
            "INSERT INTO {table} (id,conversation_id,run_id,origin,status,delivery,created_at,delivered_at,next_flush_at,prompt_text)
             VALUES (?,?,?,'web',?,?,?,?,?,'hi')"
        ))
        .bind(&id)
        .bind(&conv)
        .bind(run)
        .bind(status)
        .bind(delivery)
        .bind(created_at)
        .bind(delivered_at)
        // 排著的那一則：一小時後才到期，timer 掛上之後會一直掛著，看得到。
        .bind((status == "queued").then(|| ago(chrono::Duration::hours(-1))))
        .execute(&app.db)
        .await
        .unwrap();
        id
    }

    async fn turn_state(app: &Arc<App>, table: &str, id: &str) -> (String, String) {
        sqlx::query_as(&format!("SELECT status, delivery FROM {table} WHERE id=?")).bind(id).fetch_one(&app.db).await.unwrap()
    }

    async fn rename(app: &Arc<App>, from: &str, to: &str) {
        sqlx::query(&format!("ALTER TABLE {from} RENAME TO {to}")).execute(&app.db).await.unwrap();
    }

    async fn polled(app: &Arc<App>, run: &str) -> bool {
        app.progress_pollers.lock().await.contains_key(run)
    }

    async fn stall_gen(app: &Arc<App>, run: &str) -> Option<u64> {
        app.stall_timers.lock().await.get(run).copied()
    }

    use crate::testing::eventually;

    /// **#75 重開驗收**：開機那一次讀 `turns` 失敗（busy／I/O）。真相都還在 DB，但把它變回 poller／timer 的只有這一步——
    /// 以前讀不到就回 0／跳過，那個回合重啟後沒人盯，閒著的 bot 也不會再有事件叫醒排著的那一則。現在 DB 恢復之後，
    /// **沒有任何 hook／狀態邊／新 prompt**，背景重試自己把每一件接回來；而重啟之後才出現的（這個行程自己的）一件都不碰。
    #[tokio::test]
    async fn a_startup_recovery_that_cannot_read_turns_catches_up_once_the_db_does() {
        let env = tt::env().await;
        let app = &env.app;
        let long_ago = ago(chrono::Duration::minutes(30));
        // 重啟前就在的五件。
        let flying = a_bot(&env, "flying").await;
        let r_flying = a_run(app, &flying, "running", &long_ago, None).await;
        a_turn(app, "turns", &flying, Some(&r_flying), "in_flight", "ok", &long_ago, Some(&db::now())).await;
        let orphan = a_bot(&env, "orphan").await;
        let r_orphan = a_run(app, &orphan, "running", &long_ago, None).await;
        let t_orphan = a_turn(app, "turns", &orphan, Some(&r_orphan), "in_flight", "pending", &long_ago, None).await;
        let waiting = a_bot(&env, "waiting").await;
        a_turn(app, "turns", &waiting, None, "queued", "pending", &long_ago, None).await;
        let send_now = a_bot(&env, "send-now").await;
        let t_send_now = a_turn(app, "turns", &send_now, None, "in_flight", "pending", &long_ago, None).await;
        let ended = a_bot(&env, "ended").await;
        let r_ended = a_run(app, &ended, "exited", &long_ago, Some(&long_ago)).await;
        let t_ended = a_turn(app, "turns", &ended, Some(&r_ended), "in_flight", "ok", &long_ago, Some(&long_ago)).await;

        rename(app, "turns", "turns_unreadable").await;
        super::rearm_progress(app).await;
        assert!(!polled(app, &r_flying).await && !polled(app, &r_orphan).await, "讀不到：這一次什麼都沒接回");
        assert!(stall_gen(app, &r_flying).await.is_none());
        assert!(!crate::lifecycle::queue_retry_timer_armed(&waiting));
        assert_eq!(turn_state(app, "turns_unreadable", &t_orphan).await.1, "pending");

        // 重啟之後才有的：這個行程自己送到一半的插隊送出、剛結束的 run 上還沒收的回合（收尾在這個行程的記憶體帳上）。
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let live_send_now = a_bot(&env, "live-send-now").await;
        let t_live_send_now = a_turn(app, "turns_unreadable", &live_send_now, None, "in_flight", "pending", &db::now(), None).await;
        let live_ended = a_bot(&env, "live-ended").await;
        let r_live_ended = a_run(app, &live_ended, "exited", &long_ago, Some(&db::now())).await;
        let t_live_ended = a_turn(app, "turns_unreadable", &live_ended, Some(&r_live_ended), "in_flight", "ok", &long_ago, Some(&long_ago)).await;

        rename(app, "turns_unreadable", "turns").await;
        let caught_up = eventually!(
            polled(app, &r_flying).await
                && polled(app, &r_orphan).await
                && stall_gen(app, &r_flying).await.is_some()
                && crate::lifecycle::queue_retry_timer_armed(&waiting)
                && turn_state(app, "turns", &t_orphan).await.1 == "unknown"
                && turn_state(app, "turns", &t_send_now).await.0 == "failed"
                && turn_state(app, "turns", &t_ended).await.0 == "failed"
        );
        assert!(caught_up, "DB 恢復之後，沒有任何事件也要全部接回");
        assert_eq!(turn_state(app, "turns", &t_send_now).await, ("failed".into(), "unknown".into()), "送出鍵生效了沒人知道");
        assert!(stall_gen(app, &r_orphan).await.is_none(), "送到一半的那一筆不補 watchdog（不能重送）");

        let armed = stall_gen(app, &r_flying).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(stall_gen(app, &r_flying).await, armed, "接回之後不再掛第二次");
        assert_eq!(turn_state(app, "turns", &t_live_send_now).await, ("in_flight".into(), "pending".into()), "重啟之後才送的插隊送出不是孤兒");
        assert_eq!(turn_state(app, "turns", &t_live_ended).await.0, "in_flight", "重啟之後才結束的 run，收尾歸這個行程的帳");
    }

    /// 只補還欠著的：一個 run 接回來了、另一個還寫不進去時，背景每一輪只碰欠著的那一個——接回來的 watchdog 不會一輪被換一次，
    /// 開機時就確定沒事的 run 之後有了這個行程自己的回合（送達證不出來、所以沒掛 poller 的那種）也不被接手。
    #[tokio::test]
    async fn the_startup_recovery_retries_only_what_it_still_owes() {
        let env = tt::env().await;
        let app = &env.app;
        let long_ago = ago(chrono::Duration::minutes(30));
        let flying = a_bot(&env, "flying").await;
        let r_flying = a_run(app, &flying, "running", &long_ago, None).await;
        a_turn(app, "turns", &flying, Some(&r_flying), "in_flight", "ok", &long_ago, Some(&db::now())).await;
        let orphan = a_bot(&env, "orphan").await;
        let r_orphan = a_run(app, &orphan, "running", &long_ago, None).await;
        let t_orphan = a_turn(app, "turns", &orphan, Some(&r_orphan), "in_flight", "pending", &long_ago, None).await;
        let idle = a_bot(&env, "idle").await;
        let r_idle = a_run(app, &idle, "running", &long_ago, None).await;
        sqlx::query(
            "CREATE TRIGGER orphan_unwritable BEFORE UPDATE OF delivery ON turns WHEN NEW.delivery = 'unknown'
             BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END",
        )
        .execute(&app.db)
        .await
        .unwrap();

        super::rearm_progress(app).await;
        let armed = stall_gen(app, &r_flying).await;
        assert!(armed.is_some() && polled(app, &r_flying).await, "讀得到的那一個當場接回");
        assert!(!polled(app, &r_orphan).await, "收不成 unknown 就先不掛：欠著");
        // 重試期間：這個行程送到 `idle`，送達證不出來（`unknown`，照設計不掛 poller）。
        a_turn(app, "turns", &idle, Some(&r_idle), "in_flight", "unknown", &db::now(), Some(&db::now())).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(!polled(app, &r_idle).await, "開機時就沒欠的 run，之後的回合是這個行程自己的");
        assert_eq!(stall_gen(app, &r_flying).await, armed, "背景重試只碰還欠著的");
        assert_eq!(turn_state(app, "turns", &t_orphan).await.1, "pending");

        sqlx::query("DROP TRIGGER orphan_unwritable").execute(&app.db).await.unwrap();
        assert!(
            eventually!(polled(app, &r_orphan).await && turn_state(app, "turns", &t_orphan).await.1 == "unknown"),
            "寫得進去之後接回"
        );
        assert_eq!(stall_gen(app, &r_flying).await, armed, "從頭到尾只掛一次");
    }

    /// 連 run 清單都讀不到時，欠著的是「開機前就在跑的 run」：重啟之後才開始的 run 不碰；重試期間這個行程自己已經盯上的 run
    /// （新回合送出時掛了 poller）也不再掛一次。
    #[tokio::test]
    async fn the_startup_recovery_leaves_what_this_process_already_owns_alone() {
        let env = tt::env().await;
        let app = &env.app;
        let long_ago = ago(chrono::Duration::minutes(30));
        let flying = a_bot(&env, "flying").await;
        let r_flying = a_run(app, &flying, "running", &long_ago, None).await;
        a_turn(app, "turns", &flying, Some(&r_flying), "in_flight", "ok", &long_ago, Some(&db::now())).await;
        let taken = a_bot(&env, "taken").await;
        let r_taken = a_run(app, &taken, "running", &long_ago, None).await;
        let t_taken = a_turn(app, "turns", &taken, Some(&r_taken), "in_flight", "ok", &long_ago, Some(&db::now())).await;

        rename(app, "runs", "runs_unreadable").await;
        super::rearm_progress(app).await;
        assert!(!polled(app, &r_flying).await, "run 清單讀不到：這一次什麼都沒接回");

        // 重試期間：這個行程送出了 `taken` 的回合、自己掛上 poller 與 watchdog；另一顆 bot 重啟之後才開始跑。
        crate::lifecycle::arm_progress(app, &r_taken, &taken, &t_taken).await;
        crate::lifecycle::arm_stall(app, &r_taken, &taken, &t_taken).await;
        let own = stall_gen(app, &r_taken).await;
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let fresh = a_bot(&env, "fresh").await;
        let r_fresh = db::ulid();
        sqlx::query("INSERT INTO runs_unreadable (id,bot_id,state,agent_status,started_at) VALUES (?,?,'running','idle',?)")
            .bind(&r_fresh)
            .bind(&fresh)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        a_turn(app, "turns", &fresh, Some(&r_fresh), "in_flight", "ok", &db::now(), Some(&db::now())).await;

        rename(app, "runs_unreadable", "runs").await;
        assert!(
            eventually!(polled(app, &r_flying).await && stall_gen(app, &r_flying).await.is_some()),
            "開機前就在跑的那一個接回來"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(stall_gen(app, &r_taken).await, own, "這個行程已經盯著的不再掛一次");
        assert!(!polled(app, &r_fresh).await && stall_gen(app, &r_fresh).await.is_none(), "重啟之後才開始的 run 不是開機恢復的事");
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

    /// 計畫中的 herdr 重啟（§6.5.2）：維護中兩條退休路徑都不刪子 agent；run 照樣結束。
    /// 反向：同樣的情境不在維護中就照舊退休（上面那條測試），逾時的窗口也不算維護中。
    #[tokio::test]
    async fn children_are_kept_while_herdr_maintenance_is_open() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let parent = a_bot(&env, "alfa").await;
        let open_until = |until: &str| {
            sqlx::query("INSERT OR REPLACE INTO herdr_maintenance (id, opened_at, until, opened_by, reason) VALUES (1,?,?,'patrol','test')")
                .bind(db::now())
                .bind(until.to_string())
        };
        open_until("2999-01-01T00:00:00.000Z").execute(&app.db).await.unwrap();

        // 路徑 1：run 還開著，agent 不見了。
        let p1 = client.tab_create(&ws.workspace_id, "/tmp/p", "k1", json!({})).await.unwrap();
        let a1 = format!("{}-k1", crate::config::agent_name("proj", &parent));
        let (k1, r1) = a_child(&env, &parent, "k1", &a1, &ws.workspace_id, &p1.tab_id, &p1.pane_id).await;
        // 路徑 2：pane_closed 先把 run 結束了。
        let p2 = client.tab_create(&ws.workspace_id, "/tmp/p", "k2", json!({})).await.unwrap();
        let a2 = format!("{}-k2", crate::config::agent_name("proj", &parent));
        let (k2, r2) = a_child(&env, &parent, "k2", &a2, &ws.workspace_id, &p2.tab_id, &p2.pane_id).await;
        client.pane_close(&p1.pane_id).await.unwrap();
        client.pane_close(&p2.pane_id).await.unwrap();
        env.herdr.agents.lock().unwrap().clear();
        crate::lifecycle::mark_run_exited(&app, &r2, "pane exited").await;
        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

        for (kid, run) in [(&k1, &r1), (&k2, &r2)] {
            assert!(db::bot(&app.db, kid).await.unwrap().unwrap().deleted_at.is_none(), "maintenance keeps the child");
            let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(run).fetch_one(&app.db).await.unwrap();
            assert!(!["starting", "running", "stopping"].contains(&state.as_str()), "the run still ends: {state}");
        }

        // 窗口過期：下一輪對帳就照原規則退休（過期當下由 herdr_maintenance 收尾）。
        open_until("2020-01-01T00:00:00.000Z").execute(&app.db).await.unwrap();
        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
        for kid in [&k1, &k2] {
            assert!(db::bot(&app.db, kid).await.unwrap().unwrap().deleted_at.is_some(), "expired window: normal rule again");
        }
    }

    /// 讀不到／讀得到之間切換 `herdr_maintenance`（改表名＝SELECT 失敗，跟 SQLite busy／I/O 錯誤同一條路）。
    async fn maintenance_readable(app: &Arc<App>, readable: bool) {
        let sql = if readable {
            "ALTER TABLE herdr_maintenance_unreadable RENAME TO herdr_maintenance"
        } else {
            "ALTER TABLE herdr_maintenance RENAME TO herdr_maintenance_unreadable"
        };
        sqlx::query(sql).execute(&app.db).await.unwrap();
    }

    async fn retired(app: &Arc<App>, bot: &str) -> bool {
        db::bot(&app.db, bot).await.unwrap().unwrap().deleted_at.is_some()
    }

    /// **#191.** 維護狀態讀不到不等於沒在維護：herdr 重啟那幾分鐘所有 pane 同時消失，這時把子 agent 軟刪，pane 回來也接不回。
    /// 兩條退休路徑（run 還開著／run 已經結束）都要留著；DB 恢復後還在維護照樣留；確定窗口結束、pane 也沒回來，才照原規則
    /// 退休——而且不必等下一個 herdr 事件，延後的那一輪自己會補跑。
    #[tokio::test]
    async fn an_unreadable_maintenance_state_never_retires_a_child() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let parent = a_bot(&env, "alfa").await;
        sqlx::query("INSERT INTO herdr_maintenance (id, opened_at, until, opened_by, reason) VALUES (1,?,'2999-01-01T00:00:00.000Z','patrol','test')")
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        // 路徑 1：run 還開著，agent 不見了。路徑 2：pane_closed 先把 run 結束了。
        let p1 = client.tab_create(&ws.workspace_id, "/tmp/p", "k1", json!({})).await.unwrap();
        let a1 = format!("{}-k1", crate::config::agent_name("proj", &parent));
        let (k1, r1) = a_child(&env, &parent, "k1", &a1, &ws.workspace_id, &p1.tab_id, &p1.pane_id).await;
        let p2 = client.tab_create(&ws.workspace_id, "/tmp/p", "k2", json!({})).await.unwrap();
        let a2 = format!("{}-k2", crate::config::agent_name("proj", &parent));
        let (k2, r2) = a_child(&env, &parent, "k2", &a2, &ws.workspace_id, &p2.tab_id, &p2.pane_id).await;
        client.pane_close(&p1.pane_id).await.unwrap();
        client.pane_close(&p2.pane_id).await.unwrap();
        env.herdr.agents.lock().unwrap().clear();
        crate::lifecycle::mark_run_exited(&app, &r2, "pane exited").await;

        maintenance_readable(&app, false).await;
        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
        for kid in [&k1, &k2] {
            assert!(!retired(&app, kid).await, "讀不到維護狀態：這一輪留著");
        }
        let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(&r1).fetch_one(&app.db).await.unwrap();
        assert_eq!(state, "exited", "run 照樣結束，留著的是子 bot");

        // DB 恢復，窗口還開著：照樣留。
        maintenance_readable(&app, true).await;
        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
        for kid in [&k1, &k2] {
            assert!(!retired(&app, kid).await, "還在維護：留著");
        }

        // 兩顆都沒有 active run 了，再讀不到一次：一樣不退休。
        maintenance_readable(&app, false).await;
        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
        for kid in [&k1, &k2] {
            assert!(!retired(&app, kid).await, "沒有 active run 也不因為讀不到而退休");
        }

        // 窗口確定結束、pane 也沒回來：延後的那一輪自己補跑，照原規則退休（這之後沒有任何人叫 reconcile）。
        sqlx::query("DELETE FROM herdr_maintenance_unreadable").execute(&app.db).await.unwrap();
        maintenance_readable(&app, true).await;
        let _ = crate::testing::eventually!(retired(&app, &k1).await && retired(&app, &k2).await);
        for kid in [&k1, &k2] {
            assert!(retired(&app, kid).await, "確定沒在維護：延後的那一輪照原規則退休");
        }
    }

    /// #191 同一條：run 的結束寫不進去時，DB 裡它還在跑——這時把子 bot 軟刪，就是一顆刪掉的 bot 掛著活的 run。
    #[tokio::test]
    async fn a_child_whose_run_exit_was_not_recorded_is_not_retired() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let parent = a_bot(&env, "alfa").await;
        let pane = client.tab_create(&ws.workspace_id, "/tmp/p", "kid", json!({})).await.unwrap();
        let agent = format!("{}-kid", crate::config::agent_name("proj", &parent));
        let (kid, run) = a_child(&env, &parent, "kid", &agent, &ws.workspace_id, &pane.tab_id, &pane.pane_id).await;
        client.pane_close(&pane.pane_id).await.unwrap();
        env.herdr.agents.lock().unwrap().clear();
        sqlx::query(
            "CREATE TRIGGER exit_unwritable BEFORE UPDATE OF state ON runs WHEN NEW.state = 'exited'
             BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END",
        )
        .execute(&app.db)
        .await
        .unwrap();

        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
        let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(&run).fetch_one(&app.db).await.unwrap();
        assert_eq!(state, "running", "前提：結束沒寫進去");
        assert!(!retired(&app, &kid).await, "run 還活著就不退休");

        // 寫得進去之後，延後的那一輪自己把它收掉並退休。
        sqlx::query("DROP TRIGGER exit_unwritable").execute(&app.db).await.unwrap();
        let _ = crate::testing::eventually!(retired(&app, &kid).await);
        assert!(retired(&app, &kid).await, "結束寫進去之後照原規則退休");
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
        assert_eq!(kid.model.as_deref(), Some("claude-opus-5-5"), "read off `--model` after exact retired-alias mapping");
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

    fn codex_identity(name: &str, dir: &str) -> crate::config::IdentityCfg {
        crate::config::IdentityCfg {
            name: name.into(),
            kind: "codex".into(),
            host: None,
            env: std::collections::BTreeMap::from([("CODEX_HOME".into(), dir.into())]),
            args: vec![],
        }
    }

    async fn configure_cross_kind_identities(app: &Arc<App>) {
        app.cfg
            .update(|c| {
                c.identities = vec![
                    claude_identity("cc1", "/tmp/.claude-cc1"),
                    codex_identity("cx1", "/tmp/.codex-cx1"),
                ];
                Ok(())
            })
            .await
            .unwrap();
    }

    fn codex_argv() -> [&'static str; 7] {
        [
            "codex",
            "-m",
            "gpt-5.6-sol",
            "-c",
            "model_reasoning_effort=\"max\"",
            "-c",
            "service_tier=\"priority\"",
        ]
    }

    /// Herdr can list the pane before it has filled `agent`.  The process is still enough to
    /// classify the child on the first pass, and its own CODEX_HOME must win over the Claude
    /// parent's inherited identity.
    #[tokio::test]
    async fn a_codex_child_is_classified_from_argv_before_herdr_reports_its_kind() {
        let env = tt::env().await;
        let app = env.app.clone();
        configure_cross_kind_identities(&app).await;
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let child_pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
        let parent = a_bot(&env, "alfa").await;
        sqlx::query("UPDATE bots SET identity = 'cc1' WHERE id = ?").bind(&parent).execute(&app.db).await.unwrap();
        let parent_agent = crate::config::agent_name("proj", &parent);
        let child_agent = format!("{parent_agent}-codex");
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
        )
        .bind(db::ulid())
        .bind(&parent)
        .bind(&ws.workspace_id)
        .bind(&root.tab_id)
        .bind(&root.pane_id)
        .bind(&parent_agent)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        *env.herdr.agents.lock().unwrap() = vec![
            json!({"name": parent_agent, "agent": "claude", "agent_status": "idle",
                   "workspace_id": ws.workspace_id, "tab_id": root.tab_id, "pane_id": root.pane_id, "cwd": "/tmp/p"}),
            json!({"name": child_agent, "agent": null, "agent_status": "working",
                   "workspace_id": ws.workspace_id, "tab_id": child_pane.tab_id, "pane_id": child_pane.pane_id, "cwd": "/tmp/p"}),
        ];
        let argv = codex_argv();
        env.herdr.set_argv(&child_pane.pane_id, &argv);
        env.herdr.set_pid(&child_pane.pane_id, 7901);
        let fake = Arc::new(FakeProcEnv {
            envs: std::collections::BTreeMap::from([(
                7901,
                std::collections::BTreeMap::from([("CODEX_HOME".into(), "/tmp/.codex-cx1".into())]),
            )]),
            asked: std::sync::Mutex::new(Vec::new()),
        });
        app.proc_env.set(fake);

        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

        let child = sqlx::query_as::<_, db::Bot>("SELECT * FROM bots WHERE parent_bot_id = ?")
            .bind(&parent)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(child.kind, "codex");
        assert_eq!(child.identity.as_deref(), Some("cx1"));
        assert_eq!(child.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(child.effort.as_deref(), Some("max"));
        assert_eq!(child.fast, 1);
    }

    /// If both Herdr's kind and the process argv were unavailable at adoption time, the later
    /// agent-list update still has to repair an already-claimed child and re-run CODEX_HOME.
    #[tokio::test]
    async fn a_late_herdr_kind_repairs_an_existing_child_and_its_identity() {
        let env = tt::env().await;
        let app = env.app.clone();
        configure_cross_kind_identities(&app).await;
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let child_pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
        let parent = a_bot(&env, "alfa").await;
        sqlx::query("UPDATE bots SET identity = 'cc1' WHERE id = ?").bind(&parent).execute(&app.db).await.unwrap();
        let parent_agent = crate::config::agent_name("proj", &parent);
        let child_agent = format!("{parent_agent}-codex");
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
        )
        .bind(db::ulid())
        .bind(&parent)
        .bind(&ws.workspace_id)
        .bind(&root.tab_id)
        .bind(&root.pane_id)
        .bind(&parent_agent)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        *env.herdr.agents.lock().unwrap() = vec![
            json!({"name": parent_agent, "agent": "claude", "agent_status": "idle",
                   "workspace_id": ws.workspace_id, "tab_id": root.tab_id, "pane_id": root.pane_id, "cwd": "/tmp/p"}),
            json!({"name": child_agent, "agent": null, "agent_status": "working",
                   "workspace_id": ws.workspace_id, "tab_id": child_pane.tab_id, "pane_id": child_pane.pane_id, "cwd": "/tmp/p"}),
        ];
        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
        let first = sqlx::query_as::<_, db::Bot>("SELECT * FROM bots WHERE parent_bot_id = ?")
            .bind(&parent)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(first.kind, "claude", "前提：第一輪沒有 kind／argv，只能暫時退回 parent");
        assert_eq!(first.identity.as_deref(), Some("cc1"));

        let argv = codex_argv();
        env.herdr.set_argv(&child_pane.pane_id, &argv);
        env.herdr.set_pid(&child_pane.pane_id, 7902);
        env.herdr
            .agents
            .lock()
            .unwrap()
            .iter_mut()
            .find(|a| a.get("name").and_then(Value::as_str) == Some(child_agent.as_str()))
            .unwrap()["agent"] = json!("codex");
        let fake = Arc::new(FakeProcEnv {
            envs: std::collections::BTreeMap::from([(
                7902,
                std::collections::BTreeMap::from([("CODEX_HOME".into(), "/tmp/.codex-cx1".into())]),
            )]),
            asked: std::sync::Mutex::new(Vec::new()),
        });
        app.proc_env.set(fake);

        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

        let child = sqlx::query_as::<_, db::Bot>("SELECT * FROM bots WHERE parent_bot_id = ?")
            .bind(&parent)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(child.kind, "codex");
        assert_eq!(child.identity.as_deref(), Some("cx1"));
        assert_eq!(child.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(child.effort.as_deref(), Some("max"));
        assert_eq!(child.fast, 1);
    }

    /// Changing a child's kind invalidates both its bot observations and its active run's
    /// observations.  A failed second write must not leave the first write committed, or the
    /// next reconcile will see the new kind and never retry the runtime reset.
    #[tokio::test]
    async fn a_kind_repair_does_not_leave_bot_and_run_observations_half_updated() {
        let env = tt::env().await;
        let app = env.app.clone();
        configure_cross_kind_identities(&app).await;
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let child_pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();

        let parent = a_bot(&env, "alfa").await;
        sqlx::query("UPDATE bots SET identity = 'cc1' WHERE id = ?").bind(&parent).execute(&app.db).await.unwrap();
        let parent_agent = crate::config::agent_name("proj", &parent);
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
        )
        .bind(db::ulid())
        .bind(&parent)
        .bind(&ws.workspace_id)
        .bind(&root.tab_id)
        .bind(&root.pane_id)
        .bind(&parent_agent)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();

        let child_agent = format!("{parent_agent}-codex");
        let (child, child_run) = a_child(
            &env,
            &parent,
            "codex",
            &child_agent,
            &ws.workspace_id,
            &child_pane.tab_id,
            &child_pane.pane_id,
        )
        .await;
        sqlx::query("UPDATE bots SET identity='cc1', model='old-model', effort='old-effort', fast=1 WHERE id=?")
            .bind(&child)
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("UPDATE runs SET runtime_model='old-model', runtime_effort='old-effort', runtime_fast=1 WHERE id=?")
            .bind(&child_run)
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query(&format!(
            "CREATE TRIGGER refuse_runtime_reset BEFORE UPDATE OF runtime_model ON runs
             WHEN OLD.id = '{child_run}' AND NEW.runtime_model IS NULL
             BEGIN SELECT RAISE(ABORT, 'runtime reset unavailable'); END"
        ))
        .execute(&app.db)
        .await
        .unwrap();

        *env.herdr.agents.lock().unwrap() = vec![
            json!({"name": parent_agent, "agent": "claude", "agent_status": "idle",
                   "workspace_id": ws.workspace_id, "tab_id": root.tab_id, "pane_id": root.pane_id, "cwd": "/tmp/p"}),
            json!({"name": child_agent, "agent": "codex", "agent_status": "working",
                   "workspace_id": ws.workspace_id, "tab_id": child_pane.tab_id, "pane_id": child_pane.pane_id, "cwd": "/tmp/p"}),
        ];

        assert!(super::reconcile_host(&app, crate::config::LOCAL_HOST).await.is_err(), "runtime reset failure must be retryable");
        let bot = sqlx::query_as::<_, db::Bot>("SELECT * FROM bots WHERE id=?").bind(&child).fetch_one(&app.db).await.unwrap();
        assert_eq!(bot.kind, "claude", "failed runtime reset must roll back the kind change");
        assert_eq!(bot.identity.as_deref(), Some("cc1"));
        assert_eq!(bot.model.as_deref(), Some("old-model"));
        assert_eq!(bot.effort.as_deref(), Some("old-effort"));
        assert_eq!(bot.fast, 1);
        let run = run_of(&app, &child).await.unwrap();
        assert_eq!(run.runtime_model.as_deref(), Some("old-model"));
        assert_eq!(run.runtime_effort.as_deref(), Some("old-effort"));
        assert_eq!(run.runtime_fast, Some(1));
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
        assert_eq!(head_bot.model.as_deref(), Some("claude-opus-5-5"), "the same process_info still fills the model through the shared mapping");

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

    /// issue #82：native `SubagentStart`／`SubagentStop`（頂層 bot 自己行程內的 Task 工具呼叫，跟
    /// §6.5a 血緣認領的子 pane 完全是兩回事）不能改變、也不會改變誰認領誰。父 bot 收到一則
    /// `SubagentStart`（`runs.subagent_json` 因此被寫入）之後，同一個 tab 底下的孫代仍然照血緣掛在
    /// 子代下面——跟沒有這則 hook 時一模一樣：hookless 的 child／grandchild pane 完全不受影響。
    #[tokio::test]
    async fn a_native_subagent_hook_on_the_parent_does_not_disturb_pane_based_adoption() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let kid_pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
        let grand_pane = client.pane_split(&kid_pane.pane_id, "down", "/tmp/p", json!({})).await.unwrap();

        let parent = a_bot(&env, "alfa").await;
        let parent_agent = crate::config::agent_name("proj", &parent);
        let kid_agent = format!("{parent_agent}-lastq");
        let parent_run = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
        )
        .bind(&parent_run)
        .bind(&parent)
        .bind(&ws.workspace_id)
        .bind(&root.pane_id)
        .bind(&root.tab_id)
        .bind(&parent_agent)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();

        // 父 bot 自己觸發一個 in-process Task 工具子代理：純可見性，不建立任何 pane。
        crate::hookrecv::process(
            &app,
            &crate::hookrecv::HookBody {
                bot_id: parent.clone(),
                provider: "claude".into(),
                payload: json!({"hook_event_name": "SubagentStart", "agent_id": "a1", "agent_type": "general-purpose"}),
                received_at: None,
                truncated: false,
                run_id: None,
            },
        )
        .await
        .unwrap();
        let snap: Option<String> =
            sqlx::query_scalar("SELECT subagent_json FROM runs WHERE id = ?").bind(&parent_run).fetch_one(&app.db).await.unwrap();
        assert!(snap.is_some(), "hook 有正常寫進這顆 run");

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
            .expect("the grandchild was adopted exactly like without the hook");
        assert_eq!(grand.parent_bot_id.as_deref(), Some(kid.id.as_str()), "血緣認領完全不看 subagent_json");

        // hookless：child／grandchild 自己一路都沒收過任何 hook，`subagent_json` 仍是 NULL。
        for id in [&kid.id, &grand.id] {
            let s: Option<String> = sqlx::query_scalar("SELECT subagent_json FROM runs WHERE bot_id = ?").bind(id).fetch_one(&app.db).await.unwrap();
            assert_eq!(s, None, "child 沒有 hook，不該憑空冒出快照");
        }
    }

    /// 2026-09-17 使用者實戰重現（issue #94）：父 agent 開一個**新** tab（不是自己那個），連續在裡面
    /// 開三顆子代理。每一顆開出來時，父 agent 自己的 `PostToolUse` 都送回這個 pane 是它剛開的——這正是
    /// `spawn_hints` 要餵給 `adopt_child` 的線索。三顆都應該直接掛在父 bot 底下，不會一顆掛一顆串成鏈，
    /// 也不會多長出重複 bot；重複跑一次 reconcile（這次沒有新 hint 可用）也不該有任何變化。
    #[tokio::test]
    async fn spawn_hints_keep_a_new_tab_full_of_children_from_chaining_or_duplicating() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();

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

        // 一個全新的 tab（不是父自己的），裡面連續開三顆——今晚實戰的形狀。
        let p1 = client.tab_create(&ws.workspace_id, "/tmp/p", "kids", json!({})).await.unwrap();
        let p2 = client.pane_split(&p1.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
        let p3 = client.pane_split(&p2.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
        assert_eq!((p2.tab_id.as_str(), p3.tab_id.as_str()), (p1.tab_id.as_str(), p1.tab_id.as_str()), "三個 pane 同一個新 tab");

        let agent_json = |name: &str, pane: &crate::herdr::PaneInfo| {
            json!({"name": name, "agent": "claude", "agent_status": "idle",
                   "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"})
        };
        let (name1, name2, name3) = (format!("{parent_agent}-k1"), format!("{parent_agent}-k2"), format!("{parent_agent}-k3"));

        // 三次分開的 reconcile pass，模擬三次 `agent start` 之間真的會有的時間差——正是今晚串成鏈的時序。
        // 父 agent 自己全程還活著（它自己的 pane 一路都在清單裡）。
        crate::spawn_hints::record(&app, &parent, &p1.pane_id).await.unwrap();
        *env.herdr.agents.lock().unwrap() = vec![agent_json(&parent_agent, &root), agent_json(&name1, &p1)];
        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

        crate::spawn_hints::record(&app, &parent, &p2.pane_id).await.unwrap();
        env.herdr.agents.lock().unwrap().push(agent_json(&name2, &p2));
        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

        crate::spawn_hints::record(&app, &parent, &p3.pane_id).await.unwrap();
        env.herdr.agents.lock().unwrap().push(agent_json(&name3, &p3));
        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

        let children: Vec<db::Bot> =
            sqlx::query_as("SELECT * FROM bots WHERE parent_bot_id IS NOT NULL ORDER BY name").fetch_all(&app.db).await.unwrap();
        assert_eq!(children.len(), 3, "剛好三顆，沒有多長出重複 bot：{children:?}");
        for c in &children {
            assert_eq!(c.parent_bot_id.as_deref(), Some(parent.as_str()), "{} 要掛在真正的父 bot 底下，不是前一顆子代理", c.name);
        }

        // 用過的 hint 已經被消耗掉；再跑一次（這次沒有新 hint）靠既有的血緣配對，不該長出任何東西。
        assert!(crate::spawn_hints::for_host(&app, crate::config::LOCAL_HOST).await.unwrap().is_empty(), "hint 用完就該被消耗掉");
        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bots WHERE parent_bot_id IS NOT NULL").fetch_one(&app.db).await.unwrap();
        assert_eq!(n, 3, "重跑一次不該重複建立");
    }

    /// **#94 重開**：hint 讀不到不等於沒有 hint。父 agent 在新 tab 裡連開三顆、每一顆的 hint 都已經記在表裡，只是第二、三顆
    /// 出現的那一輪 SELECT 失敗——退回同 tab 推斷就會把 k2 掛到 k1 底下（串鏈、還因為短名字對錯 parent 取不到而長出重複 bot）。
    /// 讀不到的那一輪一顆都不認領；DB 恢復之後，延後的那一輪自己照 hint 認領（這之後沒有任何人叫 reconcile）。
    #[tokio::test]
    async fn an_unreadable_spawn_hint_defers_adoption_instead_of_guessing_by_tab() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();

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
        let p1 = client.tab_create(&ws.workspace_id, "/tmp/p", "kids", json!({})).await.unwrap();
        let p2 = client.pane_split(&p1.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
        let p3 = client.pane_split(&p2.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
        let agent_json = |name: &str, pane: &crate::herdr::PaneInfo| {
            json!({"name": name, "agent": "claude", "agent_status": "idle",
                   "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"})
        };
        let (name1, name2, name3) = (format!("{parent_agent}-k1"), format!("{parent_agent}-k2"), format!("{parent_agent}-k3"));
        for p in [&p1, &p2, &p3] {
            crate::spawn_hints::record(&app, &parent, &p.pane_id).await.unwrap();
        }
        let children = |app: Arc<App>| async move {
            sqlx::query_as::<_, db::Bot>("SELECT * FROM bots WHERE parent_bot_id IS NOT NULL ORDER BY name").fetch_all(&app.db).await.unwrap()
        };

        // k1 那一輪讀得到：照 hint 掛在父底下。
        *env.herdr.agents.lock().unwrap() = vec![agent_json(&parent_agent, &root), agent_json(&name1, &p1)];
        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
        assert_eq!(children(app.clone()).await.len(), 1);

        // k2、k3 出現的那兩輪 hint 讀不到：k1 已經是那個 tab 的候選 parent，退回推斷就會串鏈。
        sqlx::query("ALTER TABLE spawn_hints RENAME TO spawn_hints_unreadable").execute(&app.db).await.unwrap();
        env.herdr.agents.lock().unwrap().push(agent_json(&name2, &p2));
        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
        env.herdr.agents.lock().unwrap().push(agent_json(&name3, &p3));
        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
        let kids = children(app.clone()).await;
        assert_eq!(kids.len(), 1, "hint 讀不到的那幾輪一顆都不認領，不猜：{kids:?}");

        // DB 恢復：延後的那一輪自己補跑，照 hint 認領，沒有鏈、沒有重複。
        sqlx::query("ALTER TABLE spawn_hints_unreadable RENAME TO spawn_hints").execute(&app.db).await.unwrap();
        let mut kids = Vec::new();
        let _ = crate::testing::eventually!({
            kids = children(app.clone()).await;
            kids.len() == 3
        });
        assert_eq!(kids.len(), 3, "剛好三顆：{kids:?}");
        for c in &kids {
            assert_eq!(c.parent_bot_id.as_deref(), Some(parent.as_str()), "{} 要掛在真正的父 bot 底下", c.name);
        }
        assert_eq!(kids.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["k1", "k2", "k3"], "短名字都取得到：parent 沒選錯");
    }

    /// 兩輪對帳同時跑（事件驅動的一輪＋延後補跑的一輪，負載一高就會重疊）：後到的那一輪 `claimed` 讀在前一輪認領之前、
    /// hint 卻讀在前一輪 `consume` 之後，就退回同 tab 推斷，把已經認領的子 agent 又掛到別顆底下、長出重複 bot。
    /// 同一台主機的對帳要一輪一輪來：不論幾輪同時進來，都只認領一次、掛對 parent。
    #[tokio::test]
    async fn overlapping_reconcile_passes_never_adopt_the_same_child_twice() {
        for round in 0..8 {
            let env = tt::env().await;
            let app = env.app.clone();
            let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
            let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
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
            let p1 = client.tab_create(&ws.workspace_id, "/tmp/p", "kids", json!({})).await.unwrap();
            let p2 = client.pane_split(&p1.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
            let p3 = client.pane_split(&p2.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
            let agent_json = |name: &str, pane: &crate::herdr::PaneInfo| {
                json!({"name": name, "agent": "claude", "agent_status": "idle",
                       "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"})
            };
            let mut agents = vec![agent_json(&parent_agent, &root)];
            for (i, p) in [&p1, &p2, &p3].into_iter().enumerate() {
                crate::spawn_hints::record(&app, &parent, &p.pane_id).await.unwrap();
                agents.push(agent_json(&format!("{parent_agent}-k{}", i + 1), p));
            }
            *env.herdr.agents.lock().unwrap() = agents;

            let host = crate::config::LOCAL_HOST;
            let (a, b, c, d) = tokio::join!(
                super::reconcile_host(&app, host),
                super::reconcile_host(&app, host),
                super::reconcile_host(&app, host),
                super::reconcile_host(&app, host)
            );
            for r in [a, b, c, d] {
                r.unwrap();
            }
            let kids: Vec<db::Bot> =
                sqlx::query_as("SELECT * FROM bots WHERE parent_bot_id IS NOT NULL ORDER BY name").fetch_all(&app.db).await.unwrap();
            assert_eq!(kids.iter().map(|k| k.name.as_str()).collect::<Vec<_>>(), ["k1", "k2", "k3"], "第 {round} 輪：不重複認領：{kids:?}");
            assert!(kids.iter().all(|k| k.parent_bot_id.as_deref() == Some(parent.as_str())), "第 {round} 輪：都掛在真正的父 bot 底下：{kids:?}");
        }
    }

    /// 對照組（issue #94）：跟上面完全同樣的場景，但**不送任何 hint**——證明 hook 缺席時，舊的（有缺陷
    /// 的）血緣推斷完全原樣保留，這是 CLI 不發事件時唯一能依靠的路徑，這張 issue 沒有拿掉它。第二、
    /// 三顆確實串到前一顆底下，正是 2026-09-17 實際發生的現象。
    #[tokio::test]
    async fn without_spawn_hints_a_new_tab_full_of_children_still_chains_like_before() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();

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

        let p1 = client.tab_create(&ws.workspace_id, "/tmp/p", "kids", json!({})).await.unwrap();
        let p2 = client.pane_split(&p1.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
        let p3 = client.pane_split(&p2.pane_id, "right", "/tmp/p", json!({})).await.unwrap();

        let agent_json = |name: &str, pane: &crate::herdr::PaneInfo| {
            json!({"name": name, "agent": "claude", "agent_status": "idle",
                   "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"})
        };
        let (name1, name2, name3) = (format!("{parent_agent}-k1"), format!("{parent_agent}-k2"), format!("{parent_agent}-k3"));

        // 父 agent 自己全程還活著（它自己的 pane 一路都在清單裡）。
        *env.herdr.agents.lock().unwrap() = vec![agent_json(&parent_agent, &root), agent_json(&name1, &p1)];
        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
        env.herdr.agents.lock().unwrap().push(agent_json(&name2, &p2));
        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
        env.herdr.agents.lock().unwrap().push(agent_json(&name3, &p3));
        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

        let k1 = sqlx::query_as::<_, db::Bot>("SELECT * FROM bots WHERE name = 'k1'").fetch_one(&app.db).await.unwrap();
        assert_eq!(k1.parent_bot_id.as_deref(), Some(parent.as_str()), "第一顆本來就對，靠名字前綴");
        // 第二顆被同一個 tab 誤認成第一顆的小孩：`prefix_score` 對錯的那個 parent（k1）算出來是 0，
        // `adopt_child` 因此連短名字都取不到，退而用完整 herdr agent name 建 bot——這正是 issue 裡
        // 「短名字被占用後另外建出重複 bot」那個現象的根：不是名字被搶走，是 parent 從一開始就選錯了。
        let k2 = sqlx::query_as::<_, db::Bot>("SELECT * FROM bots WHERE name = ?").bind(&name2).fetch_one(&app.db).await.unwrap();
        assert_eq!(k2.parent_bot_id.as_deref(), Some(k1.id.as_str()), "沒有 hint 時，第二顆確實串到第一顆底下（既有行為原樣保留）");
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

/// snapshot 的 `workspaces` 暫時是空陣列、pane 還掛著那個 workspace_id：不能把專案映射清成 NULL。
#[cfg(test)]
mod snapshot_workspace_tests {
    use crate::db;
    use crate::testing as tt;
    use serde_json::json;

    #[tokio::test]
    async fn an_empty_workspaces_array_does_not_clear_a_mapping_still_referenced_by_panes() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        sqlx::query("UPDATE projects SET workspace_id=? WHERE id=?")
            .bind(&ws.workspace_id)
            .bind(&env.project_id)
            .execute(&app.db)
            .await
            .unwrap();
        env.herdr.workspaces.lock().unwrap().clear();

        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

        let mapped: Option<String> =
            sqlx::query_scalar("SELECT workspace_id FROM projects WHERE id=?").bind(&env.project_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(mapped.as_deref(), Some(ws.workspace_id.as_str()), "pane 還在那個 workspace，映射不能清掉");
    }
}
