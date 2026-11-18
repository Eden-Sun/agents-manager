//! 閒置太久的 bot 主動收起來，只留下可以 `--resume` 的 session（SPEC §6.11，2026-09-17 使用者要求）。
//!
//! 一顆閒置的 claude 佔的記憶體跟一顆正在跑的一樣多，但它什麼都沒在做。使用者機器上同時開著
//! 二十幾顆 bot，其中絕大多數已經幾小時沒動——那些 RAM 是白白押在那裡的。
//!
//! 所以 AGM 的控制迴圈每分鐘巡一次：**超過 90 分鐘沒有任何動作**的 bot，走既有的
//! `stop_bot` 把 agent 收掉（等於在 pane 裡下 exit）、pane 關掉，只把它剛剛那個
//! `native_session_id` 留在資料庫裡。下次有人要用它（送訊息、AGM 派工、按啟動），
//! [`wake`] 用 `StartOpts { resume_native: true }` 把同一段對話 `--resume` 回來。
//!
//! 挑的規則跟 §6.9 的批次重啟一樣保守，理由也一樣：**最不能做的事就是把使用者正在等的那一回合
//! 砍掉**。除此之外還多三條，都是「收起來就回不來」的情況：
//!
//! * 沒有可續接的 session（沒 `native_session_id`、transcript 不在、或 kind 根本沒有續接寫法）——
//!   收起來等於把對話丟掉，那不是省 RAM，是刪資料；
//! * 子 agent（`managed_by = 'child'`）——它的 pane 是父 agent 開的，daemon 起不回來（§6.5a），
//!   收掉就真的沒了。子 agent 的去留歸父 agent 與 AGM 的清理規則管，不歸這裡；
//! * 總管自己那幾顆（`supervisors.bot_id` 與 `supervisor_roles` 的 patrol／responder）——巡邏的人
//!   不收自己，而且 watchdog 反正會把它們拉回來；
//! * 主力 bot（`bots.is_primary`，側欄打星號的那幾顆）——2026-09-18 使用者：「主力 bot 超時也不先 kill」。
//!   主力是使用者隨時會切回去的那幾顆，叫醒要等 `--resume` 起來，比省下的 RAM 更貴。
//! * 專家模式的顧問（`bots.role = 'advisor'` 且所在專案 `mode = 'expert'`）——2026-11-10 使用者：「顧問（advisor）
//!   該常駐，用完不關閉」。顧問是專案裡被問事情、隨時會來問的那顆，收起來等於每次都要重新叫醒，跟主力一樣不收。
//! * 分享用（受限）bot（`shared_bots` 有一列，SPEC §20）——2026-10-04 使用者：「let AGM 不清除這類 bot」。
//!   它是給外部 end user 隨時來用的，對方不知道它被收起來，也沒有人在旁邊按啟動。
//! * 它開的子 agent 還在跑——父 bot 分完工就結束回合等回報，看起來是閒著，但子 agent 做完要
//!   `herdr agent prompt` 回報給它、卡住時 `child_alerts` 要通知它；收掉就沒人收（issue #172）。

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use crate::db;
use crate::lifecycle::{LcError, StartOpts};
use sqlx::SqlitePool;
use serde_json::json;

/// 幾分鐘沒動作就收起來。使用者指定 90 分鐘。
pub const DEFAULT_IDLE_MINUTES: i64 = 90;
/// 兩次巡邏之間至少隔這麼久——控制迴圈每 10 秒跑一次，但這件事不必那麼勤。
const SWEEP_EVERY_SECS: i64 = 60;
/// 整輪閒置回收的上限：20 顆 bot、每顆最多 30 秒檢查加 10 秒停止，留 15 分鐘讓整批完成。
const SWEEP_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// 測試接縫（#1862 S3a）：`SWEEP_TIMEOUT` 在模組外不可見，daemon 的 `supervisor_tests` 只透過這個轉呼叫（不改行為）；`#[doc(hidden)]`，正式 build 不編。
#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub const SWEEP_TIMEOUT_FOR_TEST: Duration = SWEEP_TIMEOUT;

/// 門檻，`AM_IDLE_SLEEP_MINUTES` 可覆寫；**設成 0 就整個關掉**（要留一個不必改程式的退場方式：
/// 這個功能會在使用者沒看著的時候動他的 bot）。
pub fn idle_minutes() -> i64 {
    static V: OnceLock<i64> = OnceLock::new();
    *V.get_or_init(|| match std::env::var("AM_IDLE_SLEEP_MINUTES").ok().and_then(|s| s.trim().parse::<i64>().ok()) {
        Some(n) if n >= 0 => n,
        _ => DEFAULT_IDLE_MINUTES,
    })
}

/// 一顆 bot 的判斷素材（純資料，好寫測試）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cand {
    pub bot_id: String,
    pub name: String,
    /// `bots.managed_by`：`user` / `team` / `child`。
    pub managed_by: String,
    /// 這顆是不是總管自己。
    pub is_supervisor: bool,
    /// 主力 bot（`bots.is_primary`）。
    pub is_primary: bool,
    /// 分享用（受限）bot（`shared_bots` 有一列）：永遠不收。
    pub is_share_bot: bool,
    /// 專家模式的顧問（`bots.role = 'advisor'` 且所在專案 `mode = 'expert'`）：永遠不收（2026-11-10 使用者裁示：顧問常駐，用完不關閉）。
    pub is_expert_advisor: bool,
    /// `runs.state`。
    pub state: String,
    /// `runs.agent_status`：`idle` / `working` / `blocked` / `unknown`。
    pub agent_status: String,
    /// 這個 run 還有一回合沒收掉。
    pub turn_in_flight: bool,
    /// 還有一則排隊中的 web prompt 等著送進去。
    pub queued_turn: bool,
    /// AGM 還有沒結案的 assignment 指著它（**非終局**的都算，含 `blocked`／`awaiting_review`／
    /// `quota_blocked`——只看在途三態就是 2026-09-18 那次停擺的根因）。
    pub open_assignment: bool,
    /// 它開的子 agent（`managed_by = 'child'`、`parent_bot_id` 指著它）還有活著的 run：子 agent 做完要用
    /// `herdr agent prompt` 回報給它、卡住時 `child_alerts` 要通知它，收掉就沒人收（issue #172）。
    pub live_children: bool,
    /// pane 裡還有背景 shell／建置在跑（agent 自己已經結束回合，但它丟到背景的工作還沒完）。
    /// 便宜的檢查先做完才會去問這一項，所以 [`candidates`] 列出來的一律是 `false`。
    pub background_shell: bool,
    /// 收起來之後接得回來（有 native session、transcript 還在、kind 支援 `--resume`）。
    pub resumable: bool,
    /// 最後一次有動作到現在幾分鐘。
    pub idle_minutes: i64,
}

/// 為什麼這顆不收。`code` 給 API／log 比對，`label` 給人看。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    Supervisor,
    ShareBot,
    Primary,
    ExpertAdvisor,
    TeamMember,
    Child,
    NotRunning,
    Working,
    Blocked,
    UnknownStatus,
    TurnInFlight,
    QueuedTurn,
    OpenAssignment,
    LiveChildren,
    BackgroundShell,
    NoResume,
    StillWarm,
}

impl Skip {
    pub fn code(self) -> &'static str {
        match self {
            Skip::Supervisor => "supervisor",
            Skip::ShareBot => "share_bot",
            Skip::Primary => "primary",
            Skip::ExpertAdvisor => "expert_advisor",
            Skip::TeamMember => "team_member",
            Skip::Child => "child",
            Skip::NotRunning => "not_running",
            Skip::Working => "working",
            Skip::Blocked => "blocked",
            Skip::UnknownStatus => "unknown_status",
            Skip::TurnInFlight => "turn_in_flight",
            Skip::QueuedTurn => "queued_turn",
            Skip::OpenAssignment => "open_assignment",
            Skip::LiveChildren => "live_children",
            Skip::BackgroundShell => "background_shell",
            Skip::NoResume => "no_resume",
            Skip::StillWarm => "still_warm",
        }
    }

}

/// 收不收這一顆。`threshold` 是分鐘數；判斷順序就是回報的理由順序，第一個中的就是理由。
pub fn decide(c: &Cand, threshold: i64) -> Result<(), Skip> {
    if c.is_supervisor {
        return Err(Skip::Supervisor);
    }
    if c.is_share_bot {
        return Err(Skip::ShareBot);
    }
    if c.is_primary {
        return Err(Skip::Primary);
    }
    if c.is_expert_advisor {
        return Err(Skip::ExpertAdvisor);
    }
    match c.managed_by.as_str() {
        "team" => return Err(Skip::TeamMember),
        "child" => return Err(Skip::Child),
        _ => {}
    }
    if c.state != "running" {
        return Err(Skip::NotRunning);
    }
    match c.agent_status.as_str() {
        "working" => return Err(Skip::Working),
        "blocked" => return Err(Skip::Blocked),
        "idle" => {}
        _ => return Err(Skip::UnknownStatus),
    }
    if c.turn_in_flight {
        return Err(Skip::TurnInFlight);
    }
    if c.queued_turn {
        return Err(Skip::QueuedTurn);
    }
    if c.open_assignment {
        return Err(Skip::OpenAssignment);
    }
    if c.live_children {
        return Err(Skip::LiveChildren);
    }
    if c.background_shell {
        return Err(Skip::BackgroundShell);
    }
    if !c.resumable {
        return Err(Skip::NoResume);
    }
    if c.idle_minutes < threshold {
        return Err(Skip::StillWarm);
    }
    Ok(())
}

/// 收掉之後接得回來嗎。判斷跟 `lifecycle::start_bot_locked_with` 的續接前置一致：有 session id，
/// kind＋session 組得出續接參數（`lifecycle::can_resume_native`，跟 `resume_args_by_kind` 同一份判斷），
/// 而且本機上 hook 回報過的 transcript 檔還在（claude 對一個沒有 transcript 的 session `--resume`
/// 會印 `No conversation found` 立刻退出，§6.9）。
fn resumable(kind: &str, session: Option<&str>, transcript: Option<&str>, local: bool) -> bool {
    let Some(session) = session.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    if !crate::lifecycle::can_resume_native(kind, session) {
        return false;
    }
    match transcript.map(str::trim) {
        Some(t) if local && !t.is_empty() => std::path::Path::new(t).exists(),
        _ => true,
    }
}

/// 測試接縫（#1862 S3a）：`resumable` 在模組外不可見，daemon 的 `supervisor_tests` 只透過這個轉呼叫（不改行為）；`#[doc(hidden)]`，正式 build 不編。
#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub fn resumable_for_test(kind: &str, session: Option<&str>, transcript: Option<&str>, local: bool) -> bool {
    resumable(kind, session, transcript, local)
}

/// 這顆 bot 最後一次「有動作」是什麼時候（RFC3339）。
///
/// 三個來源取最大：它的 turn（含還沒收掉的）、對話裡的訊息（終端快照補進來的回覆也算），
/// 還有這個 run 自己的 `started_at`——剛起來的 bot 不能因為還沒人跟它講過話就被當成閒置 90 分鐘。
///
/// **讀不到就回 `Err`，不退回 `started_at`**（issue #123）：那個退路會讓一顆跑很久、剛剛才有訊息的 bot，
/// 只因為這一次查詢失敗就被算成閒置 90 分鐘以上。收機器是破壞性動作，資料讀不到＝不知道，不是「閒很久」。
async fn last_activity(db: &SqlitePool, bot_id: &str, run: &db::Run) -> anyhow::Result<String> {
    let conv = db::conversation_id(db, bot_id).await?;
    let latest: Option<String> = sqlx::query_scalar(
        "SELECT MAX(ts) FROM (
           SELECT MAX(COALESCE(completed_at, created_at)) AS ts FROM turns WHERE conversation_id = ?1
           UNION ALL SELECT MAX(created_at) FROM messages WHERE conversation_id = ?1
           UNION ALL SELECT ?2
         )",
    )
    .bind(&conv)
    .bind(&run.started_at)
    .fetch_one(db)
    .await?;
    // `SELECT ?2` 保證有值；真的拿到 NULL 代表查詢的形狀不對，一樣是「不知道」。
    latest.ok_or_else(|| anyhow::anyhow!("the activity query returned no timestamp"))
}

/// 測試接縫（#1862 S3a）：`last_activity` 在模組外不可見，daemon 的 `supervisor_tests` 只透過這個轉呼叫（不改行為）；`#[doc(hidden)]`，正式 build 不編。
#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub async fn last_activity_for_test(db: &SqlitePool, bot_id: &str, run: &db::Run) -> anyhow::Result<String> {
    last_activity(db, bot_id, run).await
}

fn minutes_since(iso: &str, now: chrono::DateTime<chrono::Utc>) -> i64 {
    match chrono::DateTime::parse_from_rfc3339(iso) {
        Ok(t) => (now - t.with_timezone(&chrono::Utc)).num_minutes(),
        // 讀不懂的時間戳不能變成「閒置很久」的理由。
        Err(_) => 0,
    }
}

/// 測試接縫（#1862 S3a）：`minutes_since` 在模組外不可見，daemon 的 `supervisor_tests` 只透過這個轉呼叫（不改行為）；`#[doc(hidden)]`，正式 build 不編。
#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub fn minutes_since_for_test(iso: &str, now: chrono::DateTime<chrono::Utc>) -> i64 {
    minutes_since(iso, now)
}

/// 總管自己那幾顆的 bot id：巡檢在 `supervisors`，回應者在 `supervisor_roles`（roles.rs）。
/// 直接讀 row，不用 `get_or_init`——那會在使用者沒設總管的機器上建一列出來。
///
/// 任何一張讀不到就回 `Err`（issue #123）：漏掉的那幾個 id 會被當成一般 worker，總管自己就被收掉了。
async fn supervisor_bot_ids(db: &SqlitePool) -> anyhow::Result<std::collections::HashSet<String>> {
    let mut out = std::collections::HashSet::new();
    for q in ["SELECT bot_id FROM supervisors", "SELECT bot_id FROM supervisor_roles"] {
        let rows = sqlx::query_scalar::<_, Option<String>>(q).fetch_all(db).await?;
        out.extend(rows.into_iter().flatten().filter(|s| !s.is_empty()));
    }
    Ok(out)
}

/// 測試接縫（#1862 S3a）：`supervisor_bot_ids` 在模組外不可見，daemon 的 `supervisor_tests` 只透過這個轉呼叫（不改行為）；`#[doc(hidden)]`，正式 build 不編。
#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub async fn supervisor_bot_ids_for_test(db: &SqlitePool) -> anyhow::Result<std::collections::HashSet<String>> {
    supervisor_bot_ids(db).await
}

/// 還沒結案的交辦＝**所有非終局狀態**，從 [`crate::supervisor::assignment_state`] 那張表算出來，
/// 不在這裡抄一份清單（抄的那份就是 2026-09-18 停擺的根因：只認在途三態，一筆 `blocked` 的交辦
/// 擋不住回收，建置 child 被收掉之後 kick 每輪跳過那張未結案，部署停了 3.5 小時）。
///
/// 查詢失敗回 `Err`，**不當成 0 筆**（issue #123）：資料庫讀不到不等於「沒有未結案交辦」。
async fn has_open_assignment(db: &SqlitePool, bot_id: &str) -> anyhow::Result<bool> {
    let open = open_assignment_statuses();
    let marks = vec!["?"; open.len()].join(",");
    let sql = format!("SELECT COUNT(*) FROM supervisor_assignments WHERE target_bot_id = ? AND status IN ({marks})");
    let mut q = sqlx::query_scalar::<_, i64>(&sql).bind(bot_id);
    for st in &open {
        q = q.bind(*st);
    }
    Ok(q.fetch_one(db).await? > 0)
}

/// 測試接縫（#1862 S3a）：`has_open_assignment` 在模組外不可見，daemon 的 `supervisor_tests` 只透過這個轉呼叫（不改行為）；`#[doc(hidden)]`，正式 build 不編。
#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub async fn has_open_assignment_for_test(db: &SqlitePool, bot_id: &str) -> anyhow::Result<bool> {
    has_open_assignment(db, bot_id).await
}

/// 它開的子 agent 還有沒有活著的 run（`db::active_run` 同一組狀態）。子 agent 的 pane 是 herdr 另開的，不在這顆 bot 的
/// 行程樹底下，背景工作那一項看不到它們（issue #172）。查詢失敗回 `Err`，不當成「沒有」（issue #123 同一個規矩）。
async fn has_live_children(db: &SqlitePool, bot_id: &str) -> anyhow::Result<bool> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM bots b JOIN runs r ON r.bot_id = b.id
          WHERE b.parent_bot_id = ? AND b.managed_by = 'child' AND b.deleted_at IS NULL
            AND r.state IN ('starting','running','stopping')",
    )
    .bind(bot_id)
    .fetch_one(db)
    .await?;
    Ok(n > 0)
}

/// 非終局的交辦狀態。
pub fn open_assignment_statuses() -> Vec<&'static str> {
    crate::supervisor::assignment_state::ALL.iter().copied().filter(|s| !crate::supervisor::assignment_state::is_terminal(s)).collect()
}

/// 一顆 bot 此刻的判斷素材。`sup` 是總管的 bot id。
/// 任何一項證據讀不到就回 `Err`（issue #123）——呼叫端一律跳過這顆、下一輪再看，不拿預設值頂替。
async fn cand_for(app: &(impl crate::capabilities::Db + crate::supervisor::ports::HostProbes), run: &db::Run, sup: &std::collections::HashSet<String>) -> anyhow::Result<Option<Cand>> {
    let Some(bot) = db::bot(app.db(), &run.bot_id).await? else { return Ok(None) };
    if bot.deleted_at.is_some() {
        return Ok(None);
    }
    let host = db::bot_host(app.db(), &bot.id).await?;
    let seen = last_activity(app.db(), &bot.id, run).await?;
    // 讀不到所在專案就回 Err（issue #123）：漏認的顧問會被當成一般 worker 收掉。
    let is_expert_advisor = bot.role.as_deref() == Some("advisor")
        && db::project(app.db(), &bot.project_id).await?.is_some_and(|p| p.mode == "expert");
    Ok(Some(Cand {
        bot_id: bot.id.clone(),
        name: bot.name.clone(),
        managed_by: bot.managed_by.clone(),
        is_supervisor: sup.contains(&bot.id),
        is_primary: bot.is_primary != 0,
        // 讀不到就回 Err（issue #123）：漏認的分享用 bot 會被當成一般 worker 收掉。
        is_share_bot: app.is_share_bot(&bot.id).await?,
        is_expert_advisor,
        state: run.state.clone(),
        agent_status: run.agent_status.clone(),
        turn_in_flight: db::in_flight_turn(app.db(), &run.id).await?.is_some(),
        queued_turn: db::queued_turn_for_bot(app.db(), &bot.id).await?.is_some(),
        open_assignment: has_open_assignment(app.db(), &bot.id).await?,
        live_children: has_live_children(app.db(), &bot.id).await?,
        // 要問 herdr 與 ps，太貴；只有真的要收的那一顆才查（見 [`sweep`]）。
        background_shell: false,
        resumable: resumable(
            &bot.kind,
            run.native_session_id.as_deref(),
            run.transcript_path.as_deref(),
            host == crate::config::LOCAL_HOST,
        ),
        idle_minutes: minutes_since(&seen, chrono::Utc::now()),
    }))
}

async fn candidates_with(app: &(impl crate::capabilities::Db + crate::supervisor::ports::HostProbes), sup: &std::collections::HashSet<String>) -> anyhow::Result<Vec<Cand>> {
    let mut out = Vec::new();
    for run in db::all_active_runs(app.db()).await? {
        match cand_for(app, &run, sup).await {
            Ok(Some(c)) => out.push(c),
            Ok(None) => {}
            // 這顆的證據讀不到：不進名單（＝這一輪不收），別的 bot 照常巡。
            Err(e) => tracing::warn!(bot = %run.bot_id, error = %format!("{e:#}"), "idle sweep: could not read this bot's state; keeping it warm this round"),
        }
    }
    Ok(out)
}

/// 測試接縫（#1862 S3a）：`candidates_with` 在模組外不可見，daemon 的 `supervisor_tests` 只透過這個轉呼叫（不改行為）；`#[doc(hidden)]`，正式 build 不編。
#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub async fn candidates_with_for_test(app: &(impl crate::capabilities::Db + crate::supervisor::ports::HostProbes), sup: &std::collections::HashSet<String>) -> anyhow::Result<Vec<Cand>> {
    candidates_with(app, sup).await
}

/// pane 的行程樹裡，除了那顆 shell 與 agent 自己以外還活著的**背景工作**（argv，去重後）。
///
/// 2026-09-18：建置 child 回了一句「正在背景建置測試」就結束回合，背景的 web build／cargo 還在跑。
/// 對 daemon 來說它就是一顆 `idle` 的 bot，90 分鐘後被收起來，工作跟著沒了，部署停擺 3.5 小時。
/// 回合結束不等於工作結束，所以收之前看一眼 pane 底下還有沒有活的背景工作。
///
/// 只認得出來的形狀才算，不是「有子行程就算」：claude 的 MCP server、外掛、statusline 也是子行程，
/// 把它們算進去等於把整個功能關掉。認的是 shell（背景 Bash 工作就長這樣）與建置工具。
///
/// `None` = 這顆 shell 根本不在行程樹裡（pane 剛好不在了、或 ps 與 herdr 講的不是同一台）：**證明不了**它底下
/// 沒有背景工作，跟「證明沒有」是兩回事，不能一律當空清單（issue #123）。`Some(vec![])` 才是證明過的沒有。
pub fn background_procs(ps_tree: &str, shell_pid: i32) -> Option<Vec<String>> {
    let procs = crate::memstat::parse_ps(ps_tree);
    let by_pid: std::collections::HashMap<i32, &crate::memstat::Proc> = procs.iter().map(|p| (p.pid, p)).collect();
    if !by_pid.contains_key(&shell_pid) {
        return None;
    }
    let children = crate::memstat::child_index(&procs);
    let mut order = vec![shell_pid];
    let mut seen: std::collections::HashSet<i32> = std::collections::HashSet::from([shell_pid]);
    let mut at = 0;
    while at < order.len() {
        if let Some(kids) = children.get(&order[at]) {
            let mut kids = kids.clone();
            kids.sort_unstable();
            order.extend(kids.into_iter().filter(|k| seen.insert(*k)));
        }
        at += 1;
    }
    let mut out: Vec<String> = Vec::new();
    for pid in order.into_iter().skip(1) {
        let Some(p) = by_pid.get(&pid) else { continue };
        let exe = crate::memstat::exe_name(&p.argv).trim_start_matches('-');
        if !(BACKGROUND_SHELLS.contains(&exe) || BACKGROUND_TOOLS.contains(&exe)) {
            continue;
        }
        if !out.contains(&p.argv) {
            out.push(p.argv.clone());
        }
    }
    Some(out)
}

/// 背景 Bash 工作跑起來就是這幾個。
const BACKGROUND_SHELLS: &[&str] = &["bash", "zsh", "sh", "dash", "ksh", "fish"];
/// 背景建置常見的長命行程（shell 自己先結束、工具還在跑的情形）。`node`／`python3` 刻意不列：
/// MCP server 與外掛就是那兩個，列進去等於所有 claude 都不收。
const BACKGROUND_TOOLS: &[&str] = &["cargo", "rustc", "bun", "bunx", "npm", "pnpm", "yarn", "tsc", "vite", "make", "pytest", "sccache"];

/// 這顆 bot 的 pane 底下現在有沒有背景工作——**三態**（issue #123）。
///
/// 收機器是破壞性動作，所以只有 affirmative 的「沒有」才放行；問不到不是「沒有」。以前 herdr 不在、ps 失敗、
/// pane id／shell pid 缺失一律回 `false`，暫時性的 herdr／ssh／ps 故障就成了停機許可（背景建置還在跑、bot 被回收，
/// 2026-09-18 的事故就是這條路，加了行程樹檢查之後不該又讓「看不到」變成 kill permission）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackgroundWork {
    /// 證明過：shell 在行程樹裡，底下沒有背景工作。
    None,
    /// 底下還有背景工作（argv，去重後）。
    Running(Vec<String>),
    /// 問不到、或證明不了：原因寫在裡面，給 log 看。這一輪不收，下一輪重試。
    Unknown(String),
}

/// 問 herdr 與 ps 最多等這麼久。巡邏是一顆一顆序列做的，某一顆的 ssh 卡住不能把整輪（以及之後每一輪，
/// `SWEEPING` 沒放掉就不會再開）一起拖死。
const INSPECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// ps 行程樹（或它讀不到的原因）＋pane 的 shell pid → 三態。純函式，好測。
fn judge_background(ps_tree: Result<String, String>, shell_pid: i32) -> BackgroundWork {
    let tree = match ps_tree {
        Ok(t) => t,
        Err(e) => return BackgroundWork::Unknown(format!("ps 行程樹讀不到：{e}")),
    };
    match background_procs(&tree, shell_pid) {
        None => BackgroundWork::Unknown(format!("pane 的 shell（pid {shell_pid}）不在 ps 行程樹裡，證明不了它底下沒有背景工作")),
        Some(found) if found.is_empty() => BackgroundWork::None,
        Some(found) => BackgroundWork::Running(found),
    }
}

/// 測試接縫（#1862 S3a）：`judge_background` 在模組外不可見，daemon 的 `supervisor_tests` 只透過這個轉呼叫（不改行為）；`#[doc(hidden)]`，正式 build 不編。
#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub fn judge_background_for_test(ps_tree: Result<String, String>, shell_pid: i32) -> BackgroundWork {
    judge_background(ps_tree, shell_pid)
}

async fn inspect_background(app: &(impl crate::capabilities::Db + crate::capabilities::HerdrRoutes + crate::supervisor::ports::HostProbes), run: &db::Run) -> BackgroundWork {
    match tokio::time::timeout(INSPECT_TIMEOUT, inspect_background_inner(app, run)).await {
        Ok(w) => w,
        Err(_) => BackgroundWork::Unknown(format!("問 herdr／ps 超過 {} 秒沒有回應", INSPECT_TIMEOUT.as_secs())),
    }
}

/// 測試接縫（#1862 S3a）：`inspect_background` 在模組外不可見，daemon 的 `supervisor_tests` 只透過這個轉呼叫（不改行為）；`#[doc(hidden)]`，正式 build 不編。
#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub async fn inspect_background_for_test(app: &(impl crate::capabilities::Db + crate::capabilities::HerdrRoutes + crate::supervisor::ports::HostProbes), run: &db::Run) -> BackgroundWork {
    inspect_background(app, run).await
}

async fn inspect_background_inner(app: &(impl crate::capabilities::Db + crate::capabilities::HerdrRoutes + crate::supervisor::ports::HostProbes), run: &db::Run) -> BackgroundWork {
    use BackgroundWork::Unknown;
    let Some(pane) = run.pane_id.clone().map(|p| p.trim().to_string()).filter(|p| !p.is_empty()) else {
        return Unknown("這個 run 沒有 pane id".into());
    };
    let Some(client) = app.herdr_for_run(run).await else { return Unknown("找不到這個 run 對應的 herdr client".into()) };
    let shell = match client.pane_shell(&pane).await {
        Ok(s) => s,
        Err(e) => return Unknown(format!("herdr pane.process_info 失敗：{e:#}")),
    };
    let Some(pid) = shell.shell_pid else { return Unknown(format!("herdr 沒有回 pane {pane} 的 shell_pid")) };
    let host = match db::bot_host(app.db(), &run.bot_id).await {
        Ok(h) => h,
        Err(e) => return Unknown(format!("讀不到這顆 bot 所在的主機：{e:#}")),
    };
    judge_background(ps_tree(app, &host, &run.bot_id).await, pid as i32)
}

/// 行程樹的來源。正式路徑就是 `memproc::dump`；測試可以照 bot 注入一份決定性的答案（見 `test_ps`），
/// 不然「證明沒有背景工作」要靠本機真的有一個 `sleep` 行程、而且 runner 上的 ps 讀得到它。
async fn ps_tree(app: &impl crate::supervisor::ports::HostProbes, host: &str, _bot_id: &str) -> Result<String, String> {
    #[cfg(any(test, feature = "test-hooks"))]
    if let Some(injected) = test_ps::get(_bot_id) {
        return injected;
    }
    app.process_dump(host).await.map_err(|e| format!("{e:#}"))
}

/// 測試用：按 bot id 注入 ps 行程樹（或讀不到的原因）。bot id 是每個測試自己的 ULID，平行測試互不干擾。
#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub mod test_ps {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    fn map() -> &'static Mutex<HashMap<String, Result<String, String>>> {
        static V: OnceLock<Mutex<HashMap<String, Result<String, String>>>> = OnceLock::new();
        V.get_or_init(Default::default)
    }
    pub fn get(bot_id: &str) -> Option<Result<String, String>> {
        map().lock().unwrap_or_else(|e| e.into_inner()).get(bot_id).cloned()
    }
    pub struct Guard(String);
    pub fn set(bot_id: &str, tree: Result<String, String>) -> Guard {
        map().lock().unwrap_or_else(|e| e.into_inner()).insert(bot_id.to_string(), tree);
        Guard(bot_id.to_string())
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            map().lock().unwrap_or_else(|e| e.into_inner()).remove(&self.0);
        }
    }
}

// ---------------------------------------------------------------- 睡著這件事本身

/// 把這顆標成「AGM 收起來的」。**先寫再停**：中間死掉的話留下的是一列「它應該是睡著的」，
/// 而 [`wake`] 對一顆其實還活著的 bot 只會把這列清掉，不會誤動它——反過來（停完才寫）
/// 死在中間就變成一顆沒人知道要 `--resume` 叫醒的 bot。
async fn mark_asleep(db: &SqlitePool, c: &Cand, session: Option<&str>) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO bot_sleeps (bot_id, native_session_id, idle_minutes, reason, slept_at)
         VALUES (?,?,?,'idle',?)
         ON CONFLICT(bot_id) DO UPDATE SET
           native_session_id=excluded.native_session_id,
           idle_minutes=excluded.idle_minutes,
           slept_at=excluded.slept_at",
    )
    .bind(&c.bot_id)
    .bind(session)
    .bind(c.idle_minutes)
    .bind(db::now())
    .execute(db)
    .await?;
    Ok(())
}

/// 測試接縫（#1862 S3a）：`mark_asleep` 在模組外不可見，daemon 的 `supervisor_tests` 只透過這個轉呼叫（不改行為）；`#[doc(hidden)]`，正式 build 不編。
#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub async fn mark_asleep_for_test(db: &SqlitePool, c: &Cand, session: Option<&str>) -> anyhow::Result<()> {
    mark_asleep(db, c, session).await
}

async fn clear_asleep(db: &SqlitePool, bot_id: &str) {
    let _ = sqlx::query("DELETE FROM bot_sleeps WHERE bot_id = ?").bind(bot_id).execute(db).await;
}

async fn read_asleep(db: &SqlitePool, bot_id: &str) -> anyhow::Result<Option<(String, i64)>> {
    Ok(sqlx::query_as::<_, (String, i64)>("SELECT slept_at, idle_minutes FROM bot_sleeps WHERE bot_id = ?")
        .bind(bot_id)
        .fetch_optional(db)
        .await?)
}

/// 測試接縫（#1862 S3a）：`read_asleep` 在模組外不可見，daemon 的 `supervisor_tests` 只透過這個轉呼叫（不改行為）；`#[doc(hidden)]`，正式 build 不編。
#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub async fn read_asleep_for_test(db: &SqlitePool, bot_id: &str) -> anyhow::Result<Option<(String, i64)>> {
    read_asleep(db, bot_id).await
}

/// 所有被收起來的 bot：`bot_id -> (slept_at, idle_minutes)`。狀態 JSON 一次讀完，不必每顆問一次。
pub async fn all_asleep(app: &impl crate::capabilities::Db) -> std::collections::HashMap<String, (String, i64)> {
    sqlx::query_as::<_, (String, String, i64)>("SELECT bot_id, slept_at, idle_minutes FROM bot_sleeps")
        .fetch_all(app.db())
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(id, at, mins)| (id, (at, mins)))
        .collect()
}

async fn say(app: &(impl crate::capabilities::Db + crate::supervisor::ports::TurnOps), bot_id: &str, text: &str) {
    let Ok(conv) = db::conversation_id(app.db(), bot_id).await else { return };
    let _ = app.insert_message(&conv, None, "system", text, "system", false, None).await;
}

/// 事件流最後一次說這個 run 在做什麼（`run_id -> (agent_status, 記下的時刻)`），`events::handle_status` 在寫 DB
/// **之前**記（issue #144）。
///
/// DB 的 `agent_status` 寫不進去（以前 `let _ =` 吞掉）時，DB 會一直說 idle——那不是「閒著」的證據。收機前拿這份
/// 對一下：事件流說它不是 idle 就不收。存在記憶體：daemon 重啟後是空的，那時照 DB 判（跟以前一樣）。
type Observed = std::collections::HashMap<String, (String, chrono::DateTime<chrono::Utc>)>;

fn observed() -> &'static std::sync::Mutex<Observed> {
    static V: OnceLock<std::sync::Mutex<Observed>> = OnceLock::new();
    V.get_or_init(Default::default)
}

/// `events::handle_status` 每收到一次狀態事件就記一次（在寫 DB 之前）。
pub fn observe_status(run_id: &str, status: &str) {
    observed().lock().unwrap_or_else(|e| e.into_inner()).insert(run_id.to_string(), (status.to_string(), chrono::Utc::now()));
}

/// 不在 `active` 裡的 run（結束了）不留帳：run id 每次都是新的，不清就只增不減。
pub fn retain_runs(active: &[String]) {
    prune(&mut observed().lock().unwrap_or_else(|e| e.into_inner()), active);
}

/// 只留 `active` 裡的 run；獨立成函式讓單元測試用自己的表（清全域表會讓同時跑的其他測試剛記的狀態不見）。
fn prune<V>(map: &mut std::collections::HashMap<String, V>, active: &[String]) {
    map.retain(|id, _| active.contains(id));
}

/// 測試接縫（#1862 S3a）：`prune` 在模組外不可見，daemon 的 `supervisor_tests` 只透過這個轉呼叫（不改行為）；`#[doc(hidden)]`，正式 build 不編。
#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub fn prune_for_test<V>(map: &mut std::collections::HashMap<String, V>, active: &[String]) {
    prune(map, active)
}

/// 事件流最後說它不是 idle（working／blocked／unknown…），而且那一則**不比 DB 最後一次改狀態舊**。沒看過就是 `false`，照 DB 判。
///
/// 寫 `agent_status` 的不只事件流：對帳（`reconcile`）、收編 default session、起 run 都會照 herdr 當下的答案寫，但不經過這裡。
/// herdr 的 idle 事件漏了（事件流斷線重連、daemon 忙）、由對帳把 DB 寫回 idle 時，記憶體裡那一則 working 已經過時——以前一直
/// 拿它擋，那顆 bot 永遠收不掉（issue #184）。所以比時間：DB 的 `agent_status_since`（trigger 只在值真的改變時蓋）比那一則新，
/// 就是之後有更新的說法，照 DB 判；那一則寫不進 DB（#144）時 DB 沒動，它還是最新的證據，照舊擋。DB 沒有這個時間或讀不懂，
/// 也照舊擋——不知道誰比較新，就不拿它當閒著的證據。
fn observed_busy(run: &db::Run) -> bool {
    let Some((status, at)) = observed().lock().unwrap_or_else(|e| e.into_inner()).get(&run.id).cloned() else { return false };
    if status == "idle" {
        return false;
    }
    let db_changed_after = run
        .agent_status_since
        .as_deref()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .is_some_and(|since| since.with_timezone(&chrono::Utc) > at);
    !db_changed_after
}

/// 鎖裡重讀證據時某一項讀不到：不收、不留標記，下一輪重試。
fn unreadable(c: &Cand, what: &str, e: &anyhow::Error) {
    tracing::warn!(bot = %c.name, what, error = %format!("{e:#}"), "idle sweep: could not re-read the evidence right before stopping; keeping the bot warm and retrying next round");
}

/// 收一顆：標記 → `stop_bot`（送 ctrl+c 收 agent、關 pane）→ 在它自己的對話裡說一聲為什麼。
///
/// 標記與停機前**在這顆 bot 的鎖裡再判斷一次**（issue #133）：`c` 是巡邏稍早湊的，之後還問過 herdr、跑過 ps，
/// 那段時間 AGM 可能剛好把工作派給它。`prompt` 建回合拿的是同一把鎖，所以鎖裡看到的就是停機那一刻的事實——
/// 以前拿舊的判斷直接停，剛送進去的回合被 `fail_in_flight` 標成「run stopped by user」。
///
/// 這把鎖從最後一次判斷一路握到 `stop_bot_locked` 結束（issue #123）：最後的 admission check、寫標記、停機是同一個
/// 序列化邊界，[`wake`] 與 `prompt` 的叫醒也在這把鎖裡，看不到「已標記、還沒停」的中間狀態。
/// 鎖裡重讀的證據只要有一項**讀不到**就不收——跟背景工作那一項同一個規矩：只有 affirmative 的安全證據才能收。
async fn sleep_one(app: &(impl crate::capabilities::BotLocks + crate::capabilities::Db + crate::capabilities::Emit + crate::supervisor::ports::HostProbes + crate::supervisor::ports::LocalAccountView + crate::supervisor::ports::TurnOps), c: &Cand, threshold: i64) {
    let lock = app.bot_lock(&c.bot_id).await;
    let _g = lock.lock().await;
    let run = match db::active_run(app.db(), &c.bot_id).await {
        Ok(Some(run)) => run,
        // 已經不在跑了（別人先停掉了它）：沒什麼好收的。
        Ok(None) => return,
        Err(e) => return unreadable(c, "active run", &e),
    };
    let sup = match supervisor_bot_ids(app.db()).await {
        Ok(sup) => sup,
        Err(e) => return unreadable(c, "supervisor ids", &e),
    };
    let fresh = match cand_for(app, &run, &sup).await {
        // 背景工作那一項剛才已經問過（貴），沿用；其餘都是鎖裡重讀的。
        //
        // 沿用是安全的：daemon 經手的新工作一定先建 turn／訊息（同一把鎖裡），鎖裡重讀的 in-flight、排隊、交辦、
        // 最後動作時間就看得到；一個剛開始又結束的回合，也會把「最後動作」推到現在而不再閒置。
        Ok(Some(f)) => Cand { background_shell: c.background_shell, ..f },
        Ok(None) => return,
        Err(e) => return unreadable(c, "bot state", &e),
    };
    if let Err(why) = decide(&fresh, threshold) {
        tracing::info!(bot = %c.name, why = why.code(), "idle sweep: the bot is no longer idle; leaving it running");
        return;
    }
    if observed_busy(&run) {
        tracing::info!(bot = %c.name, "idle sweep: the last status event said the bot is busy even though the DB says idle; leaving it running");
        return;
    }
    if let Some(n) = app.background_jobs_known(&run.id).filter(|n| *n > 0) {
        tracing::info!(bot = %c.name, background_jobs = n, "idle sweep: the background-work account changed before stopping; leaving the bot running");
        return;
    }
    if let Err(e) = mark_asleep(app.db(), &fresh, run.native_session_id.as_deref()).await {
        tracing::warn!(bot = %c.name, error = %e, "could not record the sleep; leaving the bot running");
        return;
    }
    // 測試專用的競態點：「已標記、還沒停」的那一瞬，另一條路（叫醒／prompt）剛好落在這裡。
    #[cfg(any(test, feature = "test-hooks"))]
    crate::race_point::hit("idle_sleep.marked", &c.bot_id).await;
    // 寫完標記到停機之間，事件流可能剛說它開始忙（handle_status 不拿鎖）：再問一次記憶體，再由停機那一步的許可
    // （`stop_bot_locked_if_idle`，DB 裡同一句 UPDATE）做最後的排序（issue #144）。
    if observed_busy(&run) {
        tracing::info!(bot = %c.name, "idle sweep: the bot started working right before the stop; leaving it running");
        clear_asleep(app.db(), &c.bot_id).await;
        return;
    }
    match app.stop_bot_locked_if_idle(&c.bot_id).await {
        Ok(_) => {
            tracing::info!(bot = %c.name, idle_minutes = c.idle_minutes, "idle bot put to sleep; only its resumable session is kept");
            say(
                app,
                &c.bot_id,
                &format!(
                    "AGM 巡到這顆已經 {} 分鐘沒有動作，先把它收起來省記憶體（只留下可以 `--resume` 接回來的 session）。下次送訊息或按啟動會自動把同一段對話叫醒。",
                    c.idle_minutes
                ),
            )
            .await;
            app.emit("bot_changed", json!({"bot_id": c.bot_id, "asleep": true})).await;
        }
        // agent 已經停了、pane 已經關了，只是 `stopped` 寫不進 DB（503 `stop_state_uncommitted`，run 停在 `stopping`、
        // 背景重試會補記）：它**就是**睡著的，標記要留著——收回去的話補記之後它只是一顆「使用者停掉」的 bot，
        // 下一則 prompt 不會用 `--resume` 叫醒它（issue #152）。
        Err(e @ LcError::Uncommitted(_)) => {
            tracing::warn!(bot = %c.name, error = ?e, "idle bot stopped but its run could not be recorded as stopped yet; keeping it marked asleep while the retry records it");
            app.emit("bot_changed", json!({"bot_id": c.bot_id, "asleep": true})).await;
        }
        // 其餘的錯誤：只有**確定它還在跑**（許可沒過、agent 對 ctrl+c 沒反應被放回 `running`、在飛的那一筆收不成）才收回
        // 標記。問不到 herdr 證實 agent 不在（`stop_not_confirmed`／`Unknown`：pane 已經關了）時 run 留在 `stopping` 交給
        // 對帳——收成 `exited` 的話它就是睡著的，標記收回去就叫不醒原本的對話（issue #170）；對帳看到 agent 放回
        // `running` 的話，`wake` 看到活著的 run 本來就會把標記清掉。讀不到 run 也不算「確定還在跑」。
        Err(e) => match db::active_run(app.db(), &c.bot_id).await {
            Ok(Some(r)) if r.state == "running" => {
                tracing::warn!(bot = %c.name, error = ?e, "could not stop the idle bot; it stays running");
                clear_asleep(app.db(), &c.bot_id).await;
            }
            now => {
                tracing::warn!(bot = %c.name, error = ?e, state = ?now.map(|r| r.map(|r| r.state)), "the idle stop could not be confirmed; keeping the bot marked asleep until the run settles");
                app.emit("bot_changed", json!({"bot_id": c.bot_id, "asleep": true})).await;
            }
        },
    }
}

/// 測試接縫（#1862 S3a）：`sleep_one` 在模組外不可見，daemon 的 `supervisor_tests` 只透過這個轉呼叫（不改行為）；`#[doc(hidden)]`，正式 build 不編。
#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub async fn sleep_one_for_test(app: &(impl crate::capabilities::BotLocks + crate::capabilities::Db + crate::capabilities::Emit + crate::supervisor::ports::HostProbes + crate::supervisor::ports::LocalAccountView + crate::supervisor::ports::TurnOps), c: &Cand, threshold: i64) {
    sleep_one(app, c, threshold).await
}

// ---------------------------------------------------------------- 叫醒

/// 被收起來的 bot 要用了：用 `--resume` 把同一段 session 接回來。
///
/// `Ok(false)` = 這顆本來就不是睡著的（絕大多數呼叫都是這一種，所以這條路必須便宜）。
/// 已經有 active run 的話只把標記清掉：它其實活著，不能拿一次 start 去撞它。
///
/// **整段在 bot 鎖裡**（issue #123）：巡邏的 [`sleep_one`] 從寫標記握到停機結束都持著這把鎖。以前這裡先不拿鎖
/// 讀標記，剛好落在「已標記、還沒停」的中間就會看到「標記在、run 還活著」，把標記清掉回 `false`——接著巡邏把
/// 它停掉，留下一顆停著、沒有標記、再也沒人知道要 `--resume` 叫醒的 bot。
pub async fn wake(app: &(impl crate::capabilities::BotLocks + crate::capabilities::Db + crate::capabilities::Emit + crate::supervisor::ports::TurnOps), bot_id: &str, why: &str) -> anyhow::Result<bool> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    wake_locked(app, bot_id, why).await
}

/// [`wake`]，呼叫端已經握著這顆 bot 的鎖。`prompt` 用這個：叫醒與後面建回合在同一次持鎖裡，巡邏不會夾在
/// 「叫醒檢查過了」與「拿到鎖」之間把剛檢查過的 bot 收掉（那樣 prompt 拿到鎖只會看到 409 `bot has no active run`）。
pub async fn wake_locked(app: &(impl crate::capabilities::Db + crate::capabilities::Emit + crate::supervisor::ports::TurnOps), bot_id: &str, why: &str) -> anyhow::Result<bool> {
    let Some((_, mins)) = read_asleep(app.db(), bot_id).await? else { return Ok(false) };
    if let Some(run) = db::active_run(app.db(), bot_id).await? {
        // 停在 `stopping`：巡邏停掉了它、`stopped` 還沒補記上（issue #152）。那是睡著的過程，不是「其實還活著」——
        // 標記留著，補記之後照 `--resume` 叫醒；現在拿一次 start 去撞它也只會撞上那筆還沒收掉的 run。
        if run.state == "stopping" {
            return Ok(false);
        }
        clear_asleep(app.db(), bot_id).await;
        return Ok(false);
    }
    // `resume_required`：接不回原本那段對話時**不要**默默開一段新的——「只留下 resume」是這個
    // 功能的全部前提，悄悄換成空白對話等於把使用者的脈絡弄丟還不說（上游 2026-09-17 的
    // `?resume=native` 用的是同一個旗標）。接不回就退回開新對話，但在那顆 bot 自己的對話裡講清楚。
    let resumed = StartOpts { resume_native: true, resume_required: true, ..Default::default() };
    match app.start_bot_locked_with(bot_id, resumed).await {
        Ok(run_id) => {
            clear_asleep(app.db(), bot_id).await;
            tracing::info!(bot = %bot_id, run = %run_id, why, "woke a sleeping bot with --resume");
            say(
                app,
                bot_id,
                &format!("閒置 {mins} 分鐘被收起來，現在因為「{why}」用 `--resume` 接回原本的 session 叫醒。"),
            )
            .await;
            app.emit("bot_changed", json!({"bot_id": bot_id, "asleep": false})).await;
            Ok(true)
        }
        // 409 `cannot_resume`：session 或 transcript 在這段睡眠裡不見了（使用者清掉、CLI 換身份）。
        // 這顆還是要回得來，所以改開新對話，但把「原本那段接不回來」寫進對話裡，不裝作沒事。
        Err(LcError::Conflict(v)) if v.get("reason").and_then(|r| r.as_str()) == Some("cannot_resume") => {
            let reason = v.get("resume_reason").and_then(|r| r.as_str()).unwrap_or("unknown").to_string();
            match app.start_bot_locked_with(bot_id, StartOpts::default()).await {
                Ok(run_id) => {
                    clear_asleep(app.db(), bot_id).await;
                    tracing::warn!(bot = %bot_id, run = %run_id, why, reason, "woke a sleeping bot, but its old session could not be resumed");
                    say(
                        app,
                        bot_id,
                        &format!(
                            "閒置 {mins} 分鐘被收起來，現在因為「{why}」叫醒——但原本那段對話接不回來（{reason}），這是一段**新的**對話。"
                        ),
                    )
                    .await;
                    app.emit("bot_changed", json!({"bot_id": bot_id, "asleep": false})).await;
                    Ok(true)
                }
                Err(e) => {
                    tracing::warn!(bot = %bot_id, why, error = ?e, "could not wake a sleeping bot");
                    Err(anyhow::anyhow!("{e:?}"))
                }
            }
        }
        Err(e) => {
            // 標記留著：這顆還是睡著的，下一次要用它時再試一次，也還看得出它是被收起來的。
            tracing::warn!(bot = %bot_id, why, error = ?e, "could not wake a sleeping bot");
            Err(anyhow::anyhow!("{e:?}"))
        }
    }
}

// ---------------------------------------------------------------- 巡邏

/// 巡邏的互斥旗標與它的 guard。`$vis` 讓同一份定義在 `test`／`test-hooks` 建置下是 `pub`（daemon 的 `supervisor_tests` 要直接讀寫 `SWEEPING`、
/// 呼叫 `SweepGuard::take()`），正式 build 維持私有；兩種可見性共用一份定義，不會只改一份而漂移（#1862 S3a）。
macro_rules! sweep_state {
    ($vis:vis,) => {
        #[doc(hidden)]
        $vis static SWEEPING: AtomicBool = AtomicBool::new(false);

        #[doc(hidden)]
        $vis struct SweepGuard;

        impl SweepGuard {
            $vis fn take() -> Option<Self> {
                (!SWEEPING.swap(true, Ordering::SeqCst)).then_some(Self)
            }
        }
    };
}
#[cfg(any(test, feature = "test-hooks"))]
sweep_state!(pub,);
#[cfg(not(any(test, feature = "test-hooks")))]
sweep_state!(,);
static LAST_SWEEP: OnceLock<std::sync::Mutex<Option<std::time::Instant>>> = OnceLock::new();

fn sweep_due(last: Option<std::time::Instant>, now: std::time::Instant) -> bool {
    match last {
        None => true,
        Some(last) => now
            .checked_duration_since(last)
            .is_none_or(|elapsed| elapsed >= Duration::from_secs(SWEEP_EVERY_SECS as u64)),
    }
}

/// 測試接縫（#1862 S3a）：`sweep_due` 在模組外不可見，daemon 的 `supervisor_tests` 只透過這個轉呼叫（不改行為）；`#[doc(hidden)]`，正式 build 不編。
#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub fn sweep_due_for_test(last: Option<std::time::Instant>, now: std::time::Instant) -> bool {
    sweep_due(last, now)
}

impl Drop for SweepGuard {
    fn drop(&mut self) {
        SWEEPING.store(false, Ordering::SeqCst);
    }
}

async fn run_sweep_task<F>(guard: SweepGuard, round: F, timeout: Duration, shutdown: tokio_util::sync::CancellationToken) -> bool
where
    F: Future<Output = ()>,
{
    let _guard = guard;
    tokio::select! {
        _ = shutdown.cancelled() => false,
        result = tokio::time::timeout(timeout, round) => match result {
            Ok(()) => false,
            Err(_) => {
                tracing::warn!(
                    timeout_seconds = timeout.as_secs(),
                    "idle sweep round timed out; cancelling this round and retrying later"
                );
                true
            }
        }
    }
}

/// 測試接縫（#1862 S3a）：`run_sweep_task` 在模組外不可見，daemon 的 `supervisor_tests` 只透過這個轉呼叫（不改行為）；`#[doc(hidden)]`，正式 build 不編。
#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub async fn run_sweep_task_for_test<F>(guard: SweepGuard, round: F, timeout: Duration, shutdown: tokio_util::sync::CancellationToken) -> bool
where
    F: Future<Output = ()>,
{
    run_sweep_task(guard, round, timeout, shutdown).await
}

/// 控制迴圈每一拍呼叫一次。真正的巡邏最多每分鐘一次，而且丟到背景跑——一顆 `stop_bot` 最久要等
/// agent 十秒，二十顆就是三分多鐘，同步做完會把 dispatch／notify 這些也一起卡住。
pub fn tick(app: &(impl crate::capabilities::BgTasks + crate::capabilities::Shutdown + crate::capabilities::BotLocks + crate::capabilities::Db + crate::capabilities::Emit + crate::capabilities::HerdrRoutes + crate::supervisor::ports::HostProbes + crate::supervisor::ports::LocalAccountView + crate::supervisor::ports::TurnOps + Clone + 'static)) {
    if cfg!(any(test, feature = "test-hooks")) || app.shutdown().is_cancelled() {
        return;
    }
    let threshold = idle_minutes();
    if threshold <= 0 {
        return;
    }
    let now = std::time::Instant::now();
    let last = LAST_SWEEP.get_or_init(|| std::sync::Mutex::new(None));
    let mut last = last.lock().unwrap_or_else(|e| e.into_inner());
    if !sweep_due(*last, now) {
        return;
    }
    let Some(guard) = SweepGuard::take() else {
        return;
    };
    *last = Some(now);
    drop(last);
    let app = app.clone();
    let tasks = app.background_tasks().clone();
    tasks.spawn(Box::pin(async move {
        let shutdown = app.shutdown().clone();
        run_sweep_task(
            guard,
            async move { sweep(&app, threshold).await },
            SWEEP_TIMEOUT,
            shutdown,
        ).await;
    }));
}

/// 巡一輪，一顆一顆收。序列而不是並行，理由跟 §6.9 的批次一樣：herdr 的 pane 版面與 per-bot 鎖
/// 都假設一次一顆。
async fn sweep(app: &(impl crate::capabilities::BotLocks + crate::capabilities::Db + crate::capabilities::Emit + crate::capabilities::HerdrRoutes + crate::capabilities::Shutdown + crate::supervisor::ports::HostProbes + crate::supervisor::ports::LocalAccountView + crate::supervisor::ports::TurnOps), threshold: i64) {
    if app.shutdown().is_cancelled() {
        return;
    }
    let sup = match supervisor_bot_ids(app.db()).await {
        Ok(s) => s,
        Err(e) => {
            // 認不出誰是總管就不能收任何一顆：漏掉的那幾個會被當成一般 worker。
            tracing::warn!(error = %format!("{e:#}"), "idle sweep could not read who the supervisors are; skipping this round");
            return;
        }
    };
    let cands = match candidates_with(app, &sup).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "idle sweep could not read the fleet");
            return;
        }
    };
    for c in cands.iter().filter(|c| decide(c, threshold).is_ok()) {
        if app.shutdown().is_cancelled() {
            return;
        }
        // 收之前再確認一次：湊完清單到輪到這顆，中間隔了前面每一顆的停機時間（一顆最久十秒），
        // 這段時間裡它可能已經被派了工作。
        let run = match db::active_run(app.db(), &c.bot_id).await {
            Ok(Some(run)) => run,
            Ok(None) => continue,
            Err(e) => {
                unreadable(c, "active run", &e);
                continue;
            }
        };
        let mut f = match cand_for(app, &run, &sup).await {
            Ok(Some(f)) => f,
            Ok(None) => continue,
            Err(e) => {
                unreadable(c, "bot state", &e);
                continue;
            }
        };
        // 便宜的判斷全過了才去問 pane：一次 ps dump 不該為了每顆閒著的 bot 每分鐘跑一遍。
        if decide(&f, threshold).is_err() {
            continue;
        }
        // 網頁顯示的「背景執行中」同一本帳（claude Stop hook 的 `background_tasks`，沒有就是畫面判斷）：
        // subagent／monitor／workflow 不是 pane 底下的行程，行程樹看不到它們。帳上有就不收，也不必再為它跑一次 ps。
        // 只往保守的方向用：帳上說「沒有」不能放寬行程樹（沒登記在 claude 的行程照樣會在收機器時被殺）。
        if let Some(n) = app.background_jobs_known(&run.id).filter(|n| *n > 0) {
            tracing::info!(bot = %run.bot_id, background_jobs = n, "idle sweep: the web shows background work for this bot; leaving it alone");
            continue;
        }
        match inspect_background(app, &run).await {
            BackgroundWork::None => {}
            BackgroundWork::Running(procs) => {
                tracing::info!(bot = %run.bot_id, procs = ?procs, "idle sweep: pane still has background work; leaving it alone");
                f.background_shell = true;
            }
            // 問不到跟「有背景工作」是兩種暫緩，log 分開寫：前者是觀測失敗（下一輪重試），後者是真的在忙。
            BackgroundWork::Unknown(reason) => {
                tracing::warn!(bot = %run.bot_id, reason, "idle sweep: could not tell whether the pane has background work; keeping the bot warm and retrying next round");
                continue;
            }
        }
        #[cfg(any(test, feature = "test-hooks"))]
        crate::race_point::hit("idle_sleep_before_sleep_one", &c.bot_id).await;
        if decide(&f, threshold).is_ok() {
            sleep_one(app, &f, threshold).await;
        }
    }
}

/// 測試接縫（#1862 S3a）：`sweep` 在模組外不可見，daemon 的 `supervisor_tests` 只透過這個轉呼叫（不改行為）；`#[doc(hidden)]`，正式 build 不編。
#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub async fn sweep_for_test(app: &(impl crate::capabilities::BotLocks + crate::capabilities::Db + crate::capabilities::Emit + crate::capabilities::HerdrRoutes + crate::capabilities::Shutdown + crate::supervisor::ports::HostProbes + crate::supervisor::ports::LocalAccountView + crate::supervisor::ports::TurnOps), threshold: i64) {
    sweep(app, threshold).await
}
