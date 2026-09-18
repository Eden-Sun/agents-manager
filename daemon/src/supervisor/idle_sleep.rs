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
//! * 沒有可續接的 session（沒 `native_session_id`、transcript 不在、或 kind 根本不支援 resume，
//!   例如 grok）——收起來等於把對話丟掉，那不是省 RAM，是刪資料；
//! * 子 agent（`managed_by = 'child'`）——它的 pane 是父 agent 開的，daemon 起不回來（§6.5a），
//!   收掉就真的沒了。子 agent 的去留歸父 agent 與 AGM 的清理規則管，不歸這裡；
//! * 總管自己那幾顆（`supervisors.bot_id` 與 `supervisor_roles` 的 patrol／responder）——巡邏的人
//!   不收自己，而且 watchdog 反正會把它們拉回來；
//! * 主力 bot（`bots.is_primary`，側欄打星號的那幾顆）——2026-09-18 使用者：「主力 bot 超時也不先 kill」。
//!   主力是使用者隨時會切回去的那幾顆，叫醒要等 `--resume` 起來，比省下的 RAM 更貴。

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;

use crate::db;
use crate::lifecycle::{self, LcError, StartOpts};
use crate::state::App;
use serde_json::json;

/// 幾分鐘沒動作就收起來。使用者指定 90 分鐘。
pub const DEFAULT_IDLE_MINUTES: i64 = 90;
/// 兩次巡邏之間至少隔這麼久——控制迴圈每 10 秒跑一次，但這件事不必那麼勤。
const SWEEP_EVERY_SECS: i64 = 60;

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
    Primary,
    TeamMember,
    Child,
    NotRunning,
    Working,
    Blocked,
    UnknownStatus,
    TurnInFlight,
    QueuedTurn,
    OpenAssignment,
    BackgroundShell,
    NoResume,
    StillWarm,
}

impl Skip {
    pub fn code(self) -> &'static str {
        match self {
            Skip::Supervisor => "supervisor",
            Skip::Primary => "primary",
            Skip::TeamMember => "team_member",
            Skip::Child => "child",
            Skip::NotRunning => "not_running",
            Skip::Working => "working",
            Skip::Blocked => "blocked",
            Skip::UnknownStatus => "unknown_status",
            Skip::TurnInFlight => "turn_in_flight",
            Skip::QueuedTurn => "queued_turn",
            Skip::OpenAssignment => "open_assignment",
            Skip::BackgroundShell => "background_shell",
            Skip::NoResume => "no_resume",
            Skip::StillWarm => "still_warm",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Skip::Supervisor => "總管自己，巡邏的人不收自己",
            Skip::Primary => "主力 bot，使用者指定閒置再久也不收",
            Skip::TeamMember => "是 team 的成員，由 team 排程管",
            Skip::Child => "是子 agent，pane 歸父 agent 管，daemon 起不回來",
            Skip::NotRunning => "還在啟動或關閉中",
            Skip::Working => "正在跑",
            Skip::Blocked => "卡在提問，等人回答",
            Skip::UnknownStatus => "狀態不明，不確定它在不在忙",
            Skip::TurnInFlight => "還有一回合沒收掉",
            Skip::QueuedTurn => "還有排隊中的訊息沒送進去",
            Skip::OpenAssignment => "AGM 還有沒結案的 assignment 指著它",
            Skip::BackgroundShell => "pane 裡還有背景 shell／建置在跑",
            Skip::NoResume => "沒有可續接的 session，收起來會把對話弄丟",
            Skip::StillWarm => "還沒閒置到門檻",
        }
    }
}

/// 收不收這一顆。`threshold` 是分鐘數；判斷順序就是回報的理由順序，第一個中的就是理由。
pub fn decide(c: &Cand, threshold: i64) -> Result<(), Skip> {
    if c.is_supervisor {
        return Err(Skip::Supervisor);
    }
    if c.is_primary {
        return Err(Skip::Primary);
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

/// `--resume` 這條路走得通的 kind。跟 `lifecycle::resume_args_by_kind` 是同一份名單：
/// grok 沒有支援的續接寫法，所以 grok 永遠不收。
fn kind_resumable(kind: &str) -> bool {
    matches!(kind, "claude" | "codex")
}

/// 收掉之後接得回來嗎。判斷跟 `lifecycle::start_bot_locked_with` 的續接前置一致：有 session id，
/// 而且本機上 hook 回報過的 transcript 檔還在（claude 對一個沒有 transcript 的 session `--resume`
/// 會印 `No conversation found` 立刻退出，§6.9）。
fn resumable(kind: &str, session: Option<&str>, transcript: Option<&str>, local: bool) -> bool {
    if !kind_resumable(kind) {
        return false;
    }
    if session.map(str::trim).filter(|s| !s.is_empty()).is_none() {
        return false;
    }
    match transcript.map(str::trim) {
        Some(t) if local && !t.is_empty() => std::path::Path::new(t).exists(),
        _ => true,
    }
}

/// 這顆 bot 最後一次「有動作」是什麼時候（RFC3339）。
///
/// 三個來源取最大：它的 turn（含還沒收掉的）、對話裡的訊息（終端快照補進來的回覆也算），
/// 還有這個 run 自己的 `started_at`——剛起來的 bot 不能因為還沒人跟它講過話就被當成閒置 90 分鐘。
///
/// **讀不到就回 `Err`，不退回 `started_at`**（issue #123）：那個退路會讓一顆跑很久、剛剛才有訊息的 bot，
/// 只因為這一次查詢失敗就被算成閒置 90 分鐘以上。收機器是破壞性動作，資料讀不到＝不知道，不是「閒很久」。
async fn last_activity(app: &Arc<App>, bot_id: &str, run: &db::Run) -> anyhow::Result<String> {
    let conv = db::conversation_id(&app.db, bot_id).await?;
    let latest: Option<String> = sqlx::query_scalar(
        "SELECT MAX(ts) FROM (
           SELECT MAX(COALESCE(completed_at, created_at)) AS ts FROM turns WHERE conversation_id = ?1
           UNION ALL SELECT MAX(created_at) FROM messages WHERE conversation_id = ?1
           UNION ALL SELECT ?2
         )",
    )
    .bind(&conv)
    .bind(&run.started_at)
    .fetch_one(&app.db)
    .await?;
    // `SELECT ?2` 保證有值；真的拿到 NULL 代表查詢的形狀不對，一樣是「不知道」。
    latest.ok_or_else(|| anyhow::anyhow!("the activity query returned no timestamp"))
}

fn minutes_since(iso: &str, now: chrono::DateTime<chrono::Utc>) -> i64 {
    match chrono::DateTime::parse_from_rfc3339(iso) {
        Ok(t) => (now - t.with_timezone(&chrono::Utc)).num_minutes(),
        // 讀不懂的時間戳不能變成「閒置很久」的理由。
        Err(_) => 0,
    }
}

/// 總管自己那幾顆的 bot id：巡檢在 `supervisors`，回應者在 `supervisor_roles`（roles.rs）。
/// 直接讀 row，不用 `get_or_init`——那會在使用者沒設總管的機器上建一列出來。
///
/// 任何一張讀不到就回 `Err`（issue #123）：漏掉的那幾個 id 會被當成一般 worker，總管自己就被收掉了。
async fn supervisor_bot_ids(app: &Arc<App>) -> anyhow::Result<std::collections::HashSet<String>> {
    let mut out = std::collections::HashSet::new();
    for q in ["SELECT bot_id FROM supervisors", "SELECT bot_id FROM supervisor_roles"] {
        let rows = sqlx::query_scalar::<_, Option<String>>(q).fetch_all(&app.db).await?;
        out.extend(rows.into_iter().flatten().filter(|s| !s.is_empty()));
    }
    Ok(out)
}

/// 還沒結案的交辦＝**所有非終局狀態**，從 [`crate::supervisor::assignment_state`] 那張表算出來，
/// 不在這裡抄一份清單（抄的那份就是 2026-09-18 停擺的根因：只認在途三態，一筆 `blocked` 的交辦
/// 擋不住回收，建置 child 被收掉之後 kick 每輪跳過那張未結案，部署停了 3.5 小時）。
///
/// 查詢失敗回 `Err`，**不當成 0 筆**（issue #123）：資料庫讀不到不等於「沒有未結案交辦」。
async fn has_open_assignment(app: &Arc<App>, bot_id: &str) -> anyhow::Result<bool> {
    let open = open_assignment_statuses();
    let marks = vec!["?"; open.len()].join(",");
    let sql = format!("SELECT COUNT(*) FROM supervisor_assignments WHERE target_bot_id = ? AND status IN ({marks})");
    let mut q = sqlx::query_scalar::<_, i64>(&sql).bind(bot_id);
    for st in &open {
        q = q.bind(*st);
    }
    Ok(q.fetch_one(&app.db).await? > 0)
}

/// 非終局的交辦狀態。
pub fn open_assignment_statuses() -> Vec<&'static str> {
    crate::supervisor::assignment_state::ALL.iter().copied().filter(|s| !crate::supervisor::assignment_state::is_terminal(s)).collect()
}

/// 一顆 bot 此刻的判斷素材。`sup` 是總管的 bot id。
/// 任何一項證據讀不到就回 `Err`（issue #123）——呼叫端一律跳過這顆、下一輪再看，不拿預設值頂替。
async fn cand_for(app: &Arc<App>, run: &db::Run, sup: &std::collections::HashSet<String>) -> anyhow::Result<Option<Cand>> {
    let Some(bot) = db::bot(&app.db, &run.bot_id).await? else { return Ok(None) };
    if bot.deleted_at.is_some() {
        return Ok(None);
    }
    let host = db::bot_host(&app.db, &bot.id).await?;
    let seen = last_activity(app, &bot.id, run).await?;
    Ok(Some(Cand {
        bot_id: bot.id.clone(),
        name: bot.name.clone(),
        managed_by: bot.managed_by.clone(),
        is_supervisor: sup.contains(&bot.id),
        is_primary: bot.is_primary != 0,
        state: run.state.clone(),
        agent_status: run.agent_status.clone(),
        turn_in_flight: db::in_flight_turn(&app.db, &run.id).await?.is_some(),
        queued_turn: db::queued_turn_for_bot(&app.db, &bot.id).await?.is_some(),
        open_assignment: has_open_assignment(app, &bot.id).await?,
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

/// 從資料庫湊出候選清單。只走有 active run 的 bot——沒在跑的本來就沒有 RAM 要省。
///
/// [`sweep`] 自己先讀 supervisor id 再走 [`candidates_with`]（讀不到就整輪跳過），所以這個只剩測試在用。
#[cfg(test)]
pub async fn candidates(app: &Arc<App>) -> anyhow::Result<Vec<Cand>> {
    let sup = supervisor_bot_ids(app).await?;
    candidates_with(app, &sup).await
}

async fn candidates_with(app: &Arc<App>, sup: &std::collections::HashSet<String>) -> anyhow::Result<Vec<Cand>> {
    let mut out = Vec::new();
    for run in db::all_active_runs(&app.db).await? {
        match cand_for(app, &run, sup).await {
            Ok(Some(c)) => out.push(c),
            Ok(None) => {}
            // 這顆的證據讀不到：不進名單（＝這一輪不收），別的 bot 照常巡。
            Err(e) => tracing::warn!(bot = %run.bot_id, error = %format!("{e:#}"), "idle sweep: could not read this bot's state; keeping it warm this round"),
        }
    }
    Ok(out)
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

async fn inspect_background(app: &Arc<App>, run: &db::Run) -> BackgroundWork {
    match tokio::time::timeout(INSPECT_TIMEOUT, inspect_background_inner(app, run)).await {
        Ok(w) => w,
        Err(_) => BackgroundWork::Unknown(format!("問 herdr／ps 超過 {} 秒沒有回應", INSPECT_TIMEOUT.as_secs())),
    }
}

async fn inspect_background_inner(app: &Arc<App>, run: &db::Run) -> BackgroundWork {
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
    let host = match db::bot_host(&app.db, &run.bot_id).await {
        Ok(h) => h,
        Err(e) => return Unknown(format!("讀不到這顆 bot 所在的主機：{e:#}")),
    };
    judge_background(crate::memproc::dump(app, &host).await.map_err(|e| format!("{e:#}")), pid as i32)
}

// ---------------------------------------------------------------- 睡著這件事本身

/// 把這顆標成「AGM 收起來的」。**先寫再停**：中間死掉的話留下的是一列「它應該是睡著的」，
/// 而 [`wake`] 對一顆其實還活著的 bot 只會把這列清掉，不會誤動它——反過來（停完才寫）
/// 死在中間就變成一顆沒人知道要 `--resume` 叫醒的 bot。
async fn mark_asleep(app: &Arc<App>, c: &Cand, session: Option<&str>) -> anyhow::Result<()> {
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
    .execute(&app.db)
    .await?;
    Ok(())
}

async fn clear_asleep(app: &Arc<App>, bot_id: &str) {
    let _ = sqlx::query("DELETE FROM bot_sleeps WHERE bot_id = ?").bind(bot_id).execute(&app.db).await;
}

/// 這顆現在是被收起來的嗎（`(slept_at, idle_minutes)`）。讀不到當成「不是」，只給測試用；
/// 決定要不要叫醒的 [`wake_locked`] 走 [`read_asleep`]，讀不到不能默默當成沒睡。
#[cfg(test)]
pub async fn asleep(app: &Arc<App>, bot_id: &str) -> Option<(String, i64)> {
    read_asleep(app, bot_id).await.ok().flatten()
}

async fn read_asleep(app: &Arc<App>, bot_id: &str) -> anyhow::Result<Option<(String, i64)>> {
    Ok(sqlx::query_as::<_, (String, i64)>("SELECT slept_at, idle_minutes FROM bot_sleeps WHERE bot_id = ?")
        .bind(bot_id)
        .fetch_optional(&app.db)
        .await?)
}

/// 所有被收起來的 bot：`bot_id -> (slept_at, idle_minutes)`。狀態 JSON 一次讀完，不必每顆問一次。
pub async fn all_asleep(app: &Arc<App>) -> std::collections::HashMap<String, (String, i64)> {
    sqlx::query_as::<_, (String, String, i64)>("SELECT bot_id, slept_at, idle_minutes FROM bot_sleeps")
        .fetch_all(&app.db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(id, at, mins)| (id, (at, mins)))
        .collect()
}

async fn say(app: &Arc<App>, bot_id: &str, text: &str) {
    let Ok(conv) = db::conversation_id(&app.db, bot_id).await else { return };
    let _ = lifecycle::insert_message(app, &conv, None, "system", text, "system", false, None).await;
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
async fn sleep_one(app: &Arc<App>, c: &Cand, session: Option<String>, threshold: i64) {
    let lock = app.bot_lock(&c.bot_id).await;
    let _g = lock.lock().await;
    let run = match db::active_run(&app.db, &c.bot_id).await {
        Ok(Some(run)) => run,
        // 已經不在跑了（別人先停掉了它）：沒什麼好收的。
        Ok(None) => return,
        Err(e) => return unreadable(c, "active run", &e),
    };
    let sup = match supervisor_bot_ids(app).await {
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
    if let Err(e) = mark_asleep(app, &fresh, session.as_deref()).await {
        tracing::warn!(bot = %c.name, error = %e, "could not record the sleep; leaving the bot running");
        return;
    }
    // 測試專用的競態點：「已標記、還沒停」的那一瞬，另一條路（叫醒／prompt）剛好落在這裡。
    #[cfg(test)]
    crate::lifecycle::race_point::hit("idle_sleep.marked", &c.bot_id).await;
    match lifecycle::stop_bot_locked(app, &c.bot_id).await {
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
        Err(e) => {
            // 停不下來就不是睡著的：把標記收回去，下一輪再看。
            tracing::warn!(bot = %c.name, error = ?e, "could not stop the idle bot; it stays running");
            clear_asleep(app, &c.bot_id).await;
        }
    }
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
pub async fn wake(app: &Arc<App>, bot_id: &str, why: &str) -> anyhow::Result<bool> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    wake_locked(app, bot_id, why).await
}

/// [`wake`]，呼叫端已經握著這顆 bot 的鎖。`prompt` 用這個：叫醒與後面建回合在同一次持鎖裡，巡邏不會夾在
/// 「叫醒檢查過了」與「拿到鎖」之間把剛檢查過的 bot 收掉（那樣 prompt 拿到鎖只會看到 409 `bot has no active run`）。
pub async fn wake_locked(app: &Arc<App>, bot_id: &str, why: &str) -> anyhow::Result<bool> {
    let Some((_, mins)) = read_asleep(app, bot_id).await? else { return Ok(false) };
    if db::active_run(&app.db, bot_id).await?.is_some() {
        clear_asleep(app, bot_id).await;
        return Ok(false);
    }
    // `resume_required`：接不回原本那段對話時**不要**默默開一段新的——「只留下 resume」是這個
    // 功能的全部前提，悄悄換成空白對話等於把使用者的脈絡弄丟還不說（上游 2026-09-17 的
    // `?resume=native` 用的是同一個旗標）。接不回就退回開新對話，但在那顆 bot 自己的對話裡講清楚。
    let resumed = StartOpts { resume_native: true, resume_required: true, ..Default::default() };
    match lifecycle::start_bot_locked_with(app, bot_id, resumed).await {
        Ok(run_id) => {
            clear_asleep(app, bot_id).await;
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
        // 409 `cannot_resume`：session 或 transcript 在這段睡眠裡不見了（使用者清掉、CLI 換身分）。
        // 這顆還是要回得來，所以改開新對話，但把「原本那段接不回來」寫進對話裡，不裝作沒事。
        Err(LcError::Conflict(v)) if v.get("reason").and_then(|r| r.as_str()) == Some("cannot_resume") => {
            let reason = v.get("resume_reason").and_then(|r| r.as_str()).unwrap_or("unknown").to_string();
            match lifecycle::start_bot_locked_with(app, bot_id, StartOpts::default()).await {
                Ok(run_id) => {
                    clear_asleep(app, bot_id).await;
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

static SWEEPING: AtomicBool = AtomicBool::new(false);
static LAST_SWEEP: AtomicI64 = AtomicI64::new(0);

/// 控制迴圈每一拍呼叫一次。真正的巡邏最多每分鐘一次，而且丟到背景跑——一顆 `stop_bot` 最久要等
/// agent 十秒，二十顆就是三分多鐘，同步做完會把 dispatch／notify 這些也一起卡住。
pub fn tick(app: &Arc<App>) {
    if cfg!(test) {
        return;
    }
    let threshold = idle_minutes();
    if threshold <= 0 {
        return;
    }
    let now = chrono::Utc::now().timestamp();
    if now - LAST_SWEEP.load(Ordering::SeqCst) < SWEEP_EVERY_SECS {
        return;
    }
    if SWEEPING.swap(true, Ordering::SeqCst) {
        return;
    }
    LAST_SWEEP.store(now, Ordering::SeqCst);
    let app = app.clone();
    tokio::spawn(async move {
        sweep(&app, threshold).await;
        SWEEPING.store(false, Ordering::SeqCst);
    });
}

/// 巡一輪，一顆一顆收。序列而不是並行，理由跟 §6.9 的批次一樣：herdr 的 pane 版面與 per-bot 鎖
/// 都假設一次一顆。
async fn sweep(app: &Arc<App>, threshold: i64) {
    let sup = match supervisor_bot_ids(app).await {
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
        // 收之前再確認一次：湊完清單到輪到這顆，中間隔了前面每一顆的停機時間（一顆最久十秒），
        // 這段時間裡它可能已經被派了工作。
        let run = match db::active_run(&app.db, &c.bot_id).await {
            Ok(Some(run)) => run,
            Ok(None) => continue,
            Err(e) => {
                unreadable(c, "active run", &e);
                continue;
            }
        };
        let session = run.native_session_id.clone();
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
        if decide(&f, threshold).is_ok() {
            sleep_one(app, &f, session, threshold).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand() -> Cand {
        Cand {
            bot_id: "b1".into(),
            name: "worker".into(),
            managed_by: "user".into(),
            is_supervisor: false,
            is_primary: false,
            state: "running".into(),
            agent_status: "idle".into(),
            turn_in_flight: false,
            queued_turn: false,
            open_assignment: false,
            background_shell: false,
            resumable: true,
            idle_minutes: 120,
        }
    }

    /// 2026-09-18 停擺：建置 child 的交辦停在 `blocked`（AGM 還沒裁示），舊的
    /// `has_open_assignment` 只認在途三態，於是它被當成閒置收掉，kick 每輪跳過那張未結案的交辦，
    /// 部署停了 3.5 小時。未結案就是未結案——非終局的每一個狀態都要擋住回收。
    #[test]
    fn every_unsettled_assignment_status_keeps_the_bot_awake() {
        for st in open_assignment_statuses() {
            assert!(
                !crate::supervisor::assignment_state::is_terminal(st),
                "{st} 是終局，不該出現在未結案清單裡"
            );
        }
        for st in ["queued", "delivered", "unknown", "awaiting_review", "blocked", "quota_blocked"] {
            assert!(open_assignment_statuses().contains(&st), "{st} 沒被當成未結案，回收會把它的 bot 收掉");
        }
        for st in crate::supervisor::assignment_state::TERMINAL {
            assert!(!open_assignment_statuses().contains(&st), "{st} 已經結案，不該擋住回收");
        }
        let mut c = cand();
        c.open_assignment = true;
        assert_eq!(decide(&c, 90), Err(Skip::OpenAssignment));
    }

    /// 主力 bot 閒置再久也不收（2026-09-18 使用者：「主力 bot 超時也不先 kill」）；
    /// 反向：同一顆取消主力就照常收。
    #[test]
    fn a_primary_bot_is_never_put_to_sleep() {
        let mut c = cand();
        c.is_primary = true;
        c.idle_minutes = 10_000;
        assert_eq!(decide(&c, 90), Err(Skip::Primary));
        c.is_primary = false;
        assert_eq!(decide(&c, 90), Ok(()));
    }

    /// 回合結束不等於工作結束：pane 底下還有背景 shell／建置在跑就不收。
    #[test]
    fn a_pane_with_background_work_is_left_alone() {
        let mut c = cand();
        c.background_shell = true;
        assert_eq!(decide(&c, 90), Err(Skip::BackgroundShell));
    }

    /// 2026-09-18 那顆建置 child 當時的行程樹：claude 已經閒著，它丟到背景的 build 還在跑。
    const PS_WITH_BACKGROUND_BUILD: &str = "\
  900     1  4000 /opt/homebrew/bin/herdr --session w168
 1000   900  8000 -zsh
 1010  1000 900000 /Users/m4p/.local/bin/claude
 1020  1010  3000 /bin/bash -c cd /repo && bun run build && cargo build --release
 1030  1020 500000 /Users/m4p/.cargo/bin/cargo build --release
";

    /// 只剩 agent 自己（外加它的 MCP server／外掛）的 pane 是可以收的——把那些算成背景工作，
    /// 等於把整個省 RAM 的功能關掉。
    const PS_IDLE_WITH_MCP: &str = "\
  900     1  4000 /opt/homebrew/bin/herdr --session w168
 1000   900  8000 -zsh
 1010  1000 900000 /Users/m4p/.local/bin/claude
 1040  1010 120000 node /Users/m4p/.claude/mcp/docs-server.js
 1050  1010  90000 /usr/bin/python3 /Users/m4p/.claude/plugins/thing.py
";

    #[test]
    fn background_build_under_the_pane_is_seen_but_mcp_servers_are_not() {
        let found = background_procs(PS_WITH_BACKGROUND_BUILD, 1000).expect("shell 在樹裡");
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().any(|a| a.contains("bun run build")), "{found:?}");
        assert!(found.iter().any(|a| a.contains("cargo build --release")), "{found:?}");

        assert_eq!(background_procs(PS_IDLE_WITH_MCP, 1000), Some(vec![]), "MCP／外掛不是背景工作：證明過的沒有");
        // 樹裡沒有這顆 shell（pane 已經不在、或 ps 與 herdr 講的不是同一台）：證明不了底下沒有背景工作，
        // 不是「沒有」——三態的 Unknown，這一輪不收（issue #123）。
        assert_eq!(background_procs(PS_WITH_BACKGROUND_BUILD, 4242), None);
        assert_eq!(background_procs("", 1000), None);
    }

    /// 三態的判斷本身（純函式）：ps 讀不到、shell 不在樹裡都是 Unknown，只有「shell 在樹裡而且底下沒有」才是 None。
    /// 端到端（herdr／pane id／shell pid 缺失）在 `a_background_inspection_that_fails_never_becomes_permission_to_stop`。
    #[test]
    fn only_an_affirmative_no_background_work_counts_as_none() {
        assert!(matches!(judge_background(Err("ssh: timed out".into()), 1000), BackgroundWork::Unknown(r) if r.contains("ssh: timed out")));
        assert!(matches!(judge_background(Ok(PS_WITH_BACKGROUND_BUILD.into()), 4242), BackgroundWork::Unknown(_)));
        assert!(matches!(judge_background(Ok(String::new()), 1000), BackgroundWork::Unknown(_)));
        assert_eq!(judge_background(Ok(PS_IDLE_WITH_MCP.into()), 1000), BackgroundWork::None);
        assert!(matches!(judge_background(Ok(PS_WITH_BACKGROUND_BUILD.into()), 1000), BackgroundWork::Running(p) if p.len() == 2));
    }

    #[test]
    fn an_idle_resumable_bot_past_the_threshold_is_put_to_sleep() {
        assert_eq!(decide(&cand(), 90), Ok(()));
    }

    #[test]
    fn a_bot_that_is_still_warm_is_left_alone() {
        let mut c = cand();
        c.idle_minutes = 89;
        assert_eq!(decide(&c, 90), Err(Skip::StillWarm));
        c.idle_minutes = 90;
        assert_eq!(decide(&c, 90), Ok(()));
    }

    #[test]
    fn nothing_the_user_is_waiting_on_is_taken_down() {
        for (set, want) in [
            ((|c: &mut Cand| c.agent_status = "working".into()) as fn(&mut Cand), Skip::Working),
            (|c| c.agent_status = "blocked".into(), Skip::Blocked),
            (|c| c.agent_status = "unknown".into(), Skip::UnknownStatus),
            (|c| c.state = "starting".into(), Skip::NotRunning),
            (|c| c.turn_in_flight = true, Skip::TurnInFlight),
            (|c| c.queued_turn = true, Skip::QueuedTurn),
            (|c| c.open_assignment = true, Skip::OpenAssignment),
        ] {
            let mut c = cand();
            set(&mut c);
            assert_eq!(decide(&c, 90), Err(want), "{}", want.code());
        }
    }

    #[test]
    fn a_session_that_cannot_be_resumed_is_never_traded_for_ram() {
        let mut c = cand();
        c.resumable = false;
        assert_eq!(decide(&c, 90), Err(Skip::NoResume));
        // grok 沒有支援的續接寫法，所以它的 run 永遠不是 resumable。
        assert!(!kind_resumable("grok"));
        assert!(kind_resumable("claude") && kind_resumable("codex"));
    }

    #[test]
    fn the_supervisor_children_and_team_members_are_not_ours_to_take_down() {
        let mut c = cand();
        c.is_supervisor = true;
        assert_eq!(decide(&c, 90), Err(Skip::Supervisor));
        for (m, want) in [("team", Skip::TeamMember), ("child", Skip::Child)] {
            let mut c = cand();
            c.managed_by = m.into();
            assert_eq!(decide(&c, 90), Err(want));
        }
    }

    #[test]
    fn a_run_with_no_session_or_a_transcript_that_is_gone_is_not_resumable() {
        assert!(!resumable("claude", None, None, true));
        assert!(resumable("claude", Some("sid-1"), None, true));
        // hook 回報過 transcript，但檔案不在了：`--resume` 會立刻退出（§6.9）。
        assert!(!resumable("claude", Some("sid-1"), Some("/nope/never-written.jsonl"), true));
        // 遠端主機上的路徑不是本機的檔案，不能拿本機的 `exists()` 去判。
        assert!(resumable("claude", Some("sid-1"), Some("/nope/never-written.jsonl"), false));
    }

    /// 一顆 90 分鐘沒動作、可續接的 bot 進得了名單；同一顆一有新訊息就退出名單。
    /// 「閒置多久」不是看 run 開多久，是看最後一次有動作到現在。
    #[tokio::test]
    async fn the_idle_clock_starts_at_the_last_thing_that_happened() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'alfa','claude','[]',0,1,'tok',?)",
        )
        .bind(&bot)
        .bind(&env.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let run = crate::testing::fake_run(&app, &bot).await;
        let long_ago = (chrono::Utc::now() - chrono::Duration::minutes(120)).to_rfc3339();
        sqlx::query("UPDATE runs SET started_at=?, native_session_id='sid-1' WHERE id=?")
            .bind(&long_ago)
            .bind(&run)
            .execute(&app.db)
            .await
            .unwrap();

        let c = candidates(&app).await.unwrap().into_iter().find(|c| c.bot_id == bot).unwrap();
        assert!(c.idle_minutes >= 120 && c.resumable);
        assert_eq!(decide(&c, 90), Ok(()));

        // 剛剛才有一則訊息：時鐘從那裡重算，這顆就不該再被收。
        let conv = db::conversation_id(&app.db, &bot).await.unwrap();
        crate::lifecycle::insert_message(&app, &conv, None, "user", "還在嗎", "web", false, None).await.unwrap();
        let c = candidates(&app).await.unwrap().into_iter().find(|c| c.bot_id == bot).unwrap();
        assert!(c.idle_minutes < 1, "idle_minutes = {}", c.idle_minutes);
        assert_eq!(decide(&c, 90), Err(Skip::StillWarm));
    }

    /// 總管現在是兩顆（巡檢在 `supervisors`、回應者在 `supervisor_roles`，roles.rs）。兩顆都不收——
    /// 只看 `supervisors` 的話，回應者會在閒置 90 分鐘後被自己人收掉。
    #[tokio::test]
    async fn both_halves_of_the_manager_are_recognised_as_the_supervisor() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let mut ids = Vec::new();
        for name in ["patrol", "responder"] {
            let id = db::ulid();
            sqlx::query(
                "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
                 VALUES (?,?,?,'claude','[]',0,1,'tok',?)",
            )
            .bind(&id)
            .bind(&env.project_id)
            .bind(name)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
            let run = crate::testing::fake_run(&app, &id).await;
            sqlx::query("UPDATE runs SET started_at=?, native_session_id='sid-1' WHERE id=?")
                .bind((chrono::Utc::now() - chrono::Duration::minutes(180)).to_rfc3339())
                .bind(&run)
                .execute(&app.db)
                .await
                .unwrap();
            ids.push(id);
        }
        crate::supervisor::store::get_or_init(&app.db).await.unwrap();
        sqlx::query("UPDATE supervisors SET bot_id=?").bind(&ids[0]).execute(&app.db).await.unwrap();
        crate::supervisor::roles::get(&app.db, crate::supervisor::roles::Role::Responder).await.unwrap();
        sqlx::query("UPDATE supervisor_roles SET bot_id=? WHERE role='responder'")
            .bind(&ids[1])
            .execute(&app.db)
            .await
            .unwrap();

        let cands = candidates(&app).await.unwrap();
        for id in &ids {
            let c = cands.iter().find(|c| &c.bot_id == id).expect("在名單裡");
            assert!(c.is_supervisor, "{} 應該被認出是總管", c.name);
            assert_eq!(decide(c, 90), Err(Skip::Supervisor), "{}", c.name);
        }
    }

    /// 真的打到 DB：一張 `blocked` 的交辦指著這顆 bot，`has_open_assignment` 就要是 true。
    /// 純函式測 `open_assignment_statuses()` 證明不了 SQL 有用上它——2026-09-18 壞掉的正是那句 SQL。
    #[tokio::test]
    async fn a_blocked_assignment_is_still_open_in_the_query_itself() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'builder','claude','[]',0,1,'tok',?)",
        )
        .bind(&bot)
        .bind(&env.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        assert!(!has_open_assignment(&app, &bot).await.unwrap(), "還沒有交辦");

        let insert = |status: &'static str| {
            let app = app.clone();
            let bot = bot.clone();
            async move {
                sqlx::query(
                    "INSERT INTO supervisor_assignments
                       (id, supervisor_id, request_id, target_bot_id, client_request_id, text, status, attempts,
                        expects_review, created_at, updated_at)
                     VALUES (?, 'AGM', NULL, ?, ?, '建置並重啟', ?, 0, 1, ?, ?)",
                )
                .bind(db::ulid())
                .bind(&bot)
                .bind(db::ulid())
                .bind(status)
                .bind(db::now())
                .bind(db::now())
                .execute(&app.db)
                .await
                .unwrap();
            }
        };

        // 事故當天那一張：AGM 還沒裁示，狀態是 blocked——舊的 SQL 看不到它。
        insert("blocked").await;
        assert!(has_open_assignment(&app, &bot).await.unwrap(), "blocked 的交辦還沒結案，不能把它的 bot 收掉");

        sqlx::query("UPDATE supervisor_assignments SET status='completed' WHERE target_bot_id=?")
            .bind(&bot)
            .execute(&app.db)
            .await
            .unwrap();
        assert!(!has_open_assignment(&app, &bot).await.unwrap(), "結案了就不該再擋著");

        for st in ["awaiting_review", "quota_blocked", "unknown"] {
            insert(st).await;
            assert!(has_open_assignment(&app, &bot).await.unwrap(), "{st} 還沒結案");
            sqlx::query("UPDATE supervisor_assignments SET status='cancelled' WHERE status=?")
                .bind(st)
                .execute(&app.db)
                .await
                .unwrap();
        }
    }

    /// 巡邏最後一次判斷「閒著」之後、真的停機之前（中間還要問 herdr、跑一次 ps，遠端主機走 ssh），AGM 剛好把
    /// 工作派給這顆閒置 90 分鐘的 bot——正是它最常挑的那種。停機前要在這顆 bot 的鎖裡再看一次：以前拿舊的
    /// 判斷直接 `stop_bot`，剛送進去的回合被 `fail_in_flight` 標成「run stopped by user」，bot 也被收起來。
    #[tokio::test]
    async fn a_bot_that_got_work_after_the_last_check_is_not_put_to_sleep() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "charlie").await.id;
        let run = crate::testing::fake_run(&app, &bot).await;
        sqlx::query("UPDATE runs SET started_at=?, native_session_id='sid-1' WHERE id=?")
            .bind((chrono::Utc::now() - chrono::Duration::minutes(120)).to_rfc3339())
            .bind(&run)
            .execute(&app.db)
            .await
            .unwrap();
        let stale = candidates(&app).await.unwrap().into_iter().find(|c| c.bot_id == bot).unwrap();
        assert_eq!(decide(&stale, 90), Ok(()), "判斷當下它確實閒著");

        // 判斷完之後派工落地：prompt 在 bot 鎖裡建了一筆送出中的回合。
        let conv = db::conversation_id(&app.db, &bot).await.unwrap();
        let turn = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
            .bind(&turn)
            .bind(&conv)
            .bind(&run)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();

        sleep_one(&app, &stale, Some("sid-1".into()), 90).await;

        let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(&turn).fetch_one(&app.db).await.unwrap();
        assert_eq!(status, "in_flight", "剛派進去的回合不能被收起來的那一下砍掉");
        assert!(asleep(&app, &bot).await.is_none(), "它沒有被收起來，也不該留下睡著的標記");
        assert!(db::active_run(&app.db, &bot).await.unwrap().is_some(), "run 照常活著");

        // 那一回合早就結束、之後又閒置夠久：照常收（鎖裡的重看不會把該收的也擋掉）。
        let long_ago = (chrono::Utc::now() - chrono::Duration::minutes(120)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        sqlx::query("UPDATE turns SET status='completed', created_at=?, completed_at=? WHERE id=?")
            .bind(&long_ago)
            .bind(&long_ago)
            .bind(&turn)
            .execute(&app.db)
            .await
            .unwrap();
        sleep_one(&app, &stale, Some("sid-1".into()), 90).await;
        assert!(asleep(&app, &bot).await.is_some(), "閒著的照樣收起來");
        assert!(db::active_run(&app.db, &bot).await.unwrap().is_none());
    }

    /// 標成睡著、實際上還活著（stop 沒成功、或使用者自己又把它起回來）：`wake` 只把標記清掉，
    /// 不能拿一次 start 去撞一個正在跑的 run。
    #[tokio::test]
    async fn waking_a_bot_that_is_actually_running_only_clears_the_mark() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'bravo','claude','[]',0,1,'tok',?)",
        )
        .bind(&bot)
        .bind(&env.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        crate::testing::fake_run(&app, &bot).await;
        sqlx::query("INSERT INTO bot_sleeps (bot_id, idle_minutes, reason, slept_at) VALUES (?,?, 'idle', ?)")
            .bind(&bot)
            .bind(99_i64)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();

        assert!(!wake(&app, &bot, "測試").await.unwrap());
        assert!(asleep(&app, &bot).await.is_none(), "the stale mark is cleared");
        assert!(db::active_run(&app.db, &bot).await.unwrap().is_some(), "its run was left alone");
        // 沒有標記的 bot 走 `wake` 是一次便宜的查詢，什麼都不做。
        assert!(!wake(&app, &bot, "測試").await.unwrap());
    }

    #[test]
    fn an_unreadable_timestamp_never_counts_as_idle() {
        let now = chrono::Utc::now();
        assert_eq!(minutes_since("not a timestamp", now), 0);
        assert_eq!(minutes_since(&(now - chrono::Duration::minutes(91)).to_rfc3339(), now), 91);
    }

    // ------------------------------------------------------------ issue #123：讀不到不等於安全

    /// 一顆閒置 120 分鐘、可續接的 bot（run 是 `fake_run`：pane 在 mock herdr 裡有 id，但沒有 agent）。
    async fn idle_bot(env: &crate::testing::Env, name: &str) -> (String, String) {
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, name).await.id;
        let run = crate::testing::fake_run(&env.app, &bot).await;
        sqlx::query("UPDATE runs SET started_at=?, native_session_id='sid-1' WHERE id=?")
            .bind((chrono::Utc::now() - chrono::Duration::minutes(120)).to_rfc3339())
            .bind(&run)
            .execute(&env.app.db)
            .await
            .unwrap();
        (bot, run)
    }

    /// 判斷當下湊的候選（讀得到、閒著）。之後才把某個資料來源弄壞，模擬「最後一刻讀不到」。
    async fn stale_cand(app: &Arc<App>, bot: &str) -> Cand {
        let c = candidates(app).await.unwrap().into_iter().find(|c| c.bot_id == bot).unwrap();
        assert_eq!(decide(&c, 90), Ok(()), "判斷當下它確實閒著");
        c
    }

    async fn assert_left_running(app: &Arc<App>, bot: &str, why: &str) {
        assert!(asleep(app, bot).await.is_none(), "{why}: 不該留下睡著的標記");
        assert!(db::active_run(&app.db, bot).await.unwrap().is_some(), "{why}: run 照常活著");
    }

    /// 讓這顆 bot 的 pane **證明**沒有背景工作：mock herdr 回一個真的、底下沒有 shell／建置工具的行程（`sleep`）當
    /// shell pid，ps 行程樹裡看得到它。整輪巡邏的測試要靠它走到 DB 證據那幾道關卡——不然背景巡檢先回 `Unknown`
    /// 就把 bot 擋下來，DB 那條 fail-open 有沒有修好都看不出來。
    struct FakeShell(std::process::Child);
    impl Drop for FakeShell {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    fn provably_no_background_work(env: &crate::testing::Env, bot: &str) -> FakeShell {
        let child = std::process::Command::new("sleep").arg("300").spawn().expect("spawn sleep");
        env.herdr.set_shell_pid(&format!("pane-{bot}"), child.id() as i64);
        FakeShell(child)
    }

    /// `has_open_assignment` 的 DB 錯誤以前被 `.unwrap_or(0)` 當成「沒有未結案交辦」：AGM 明明有一張
    /// `blocked` 的交辦指著它，這顆卻被收掉（2026-09-18 停擺的同一類後果）。讀不到就是不知道，不收。
    #[tokio::test]
    async fn an_assignment_lookup_that_fails_is_not_read_as_no_assignment() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (bot, _) = idle_bot(&env, "delta").await;
        let stale = stale_cand(&app, &bot).await;
        let _shell = provably_no_background_work(&env, &bot);

        sqlx::query("DROP TABLE supervisor_assignments").execute(&app.db).await.unwrap();
        sleep_one(&app, &stale, Some("sid-1".into()), 90).await;
        assert_left_running(&app, &bot, "交辦查不到（鎖裡重讀）").await;
        sweep(&app, 90).await;
        assert_left_running(&app, &bot, "交辦查不到（整輪巡邏）").await;
    }

    /// `last_activity` 的查詢錯誤以前被 `.ok().flatten()` 退回 `run.started_at`：一顆跑很久的 bot 就算剛剛
    /// 才有訊息，只要那一次查詢失敗，就瞬間被算成閒置 90 分鐘以上。
    #[tokio::test]
    async fn an_activity_lookup_that_fails_does_not_fall_back_to_the_run_start() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (bot, _) = idle_bot(&env, "echo").await;
        let stale = stale_cand(&app, &bot).await;
        let _shell = provably_no_background_work(&env, &bot);

        // 判斷之後才有動作：一則剛剛的訊息。活動時鐘本來應該從這裡重算。
        let conv = db::conversation_id(&app.db, &bot).await.unwrap();
        crate::lifecycle::insert_message(&app, &conv, None, "user", "還在嗎", "web", false, None).await.unwrap();

        sqlx::query("DROP TABLE messages").execute(&app.db).await.unwrap();
        sleep_one(&app, &stale, Some("sid-1".into()), 90).await;
        assert_left_running(&app, &bot, "活動時間查不到（鎖裡重讀）").await;
        sweep(&app, 90).await;
        assert_left_running(&app, &bot, "活動時間查不到（整輪巡邏）").await;
    }

    /// 辨識不到「是不是總管」（`supervisors`／`supervisor_roles` 讀不到）以前當成「一般 worker」，總管自己
    /// 會被自己人收掉。兩張表各壞一次：只壞其中一張也不行。
    #[tokio::test]
    async fn a_supervisor_lookup_that_fails_is_not_read_as_no_supervisor() {
        for broken in ["supervisors", "supervisor_roles"] {
            let env = crate::testing::env().await;
            let app = env.app.clone();
            let (bot, _) = idle_bot(&env, "foxtrot").await;
            let _shell = provably_no_background_work(&env, &bot);
            crate::supervisor::store::get_or_init(&app.db).await.unwrap();
            crate::supervisor::roles::get(&app.db, crate::supervisor::roles::Role::Responder).await.unwrap();
            // 它只登記在**被弄壞的那一張**：讀不到那張表，就沒有別的地方能認出它是總管。
            match broken {
                "supervisors" => sqlx::query("UPDATE supervisors SET bot_id=?").bind(&bot),
                _ => sqlx::query("UPDATE supervisor_roles SET bot_id=? WHERE role='responder'").bind(&bot),
            }
            .execute(&app.db)
            .await
            .unwrap();
            // 湊清單時它是總管所以不在名單裡；拿一份「當成一般 worker」的舊判斷來收，鎖裡重讀才是關卡。
            let mut stale = Cand { bot_id: bot.clone(), name: "foxtrot".into(), ..cand() };
            stale.idle_minutes = 120;

            sqlx::query(&format!("DROP TABLE {broken}")).execute(&app.db).await.unwrap();
            assert!(candidates(&app).await.is_err(), "{broken} 讀不到：名單不能照樣湊出來（漏掉的總管會被當成 worker）");
            sleep_one(&app, &stale, Some("sid-1".into()), 90).await;
            assert_left_running(&app, &bot, &format!("{broken} 讀不到（鎖裡重讀）")).await;
            sweep(&app, 90).await;
            assert_left_running(&app, &bot, &format!("{broken} 讀不到（整輪巡邏）")).await;
        }
    }

    /// `pane_shell`／`memproc::dump`／pane id／shell pid 任何一項問不到，以前都回 `false`＝「沒有背景工作」，
    /// 於是 herdr／ssh／ps 的一次暫時失敗就成了停機許可。現在：affirmative 證據才能收，問不到就跳過這一輪。
    /// 每一種失敗各跑一次 sweep，都不能碰到 agent（沒有任何 ctrl+c）。
    #[tokio::test]
    async fn a_background_inspection_that_fails_never_becomes_permission_to_stop() {
        for what in ["no_shell_pid", "shell_not_in_tree", "no_pane_id", "unknown_session"] {
            let env = crate::testing::env().await;
            let (bot, run) = idle_bot(&env, "golf").await;
            match what {
                // mock 的 `pane.process_info` 沒設 shell_pid 就不回這個欄位，像讀不到的 herdr。
                "no_shell_pid" => {}
                "shell_not_in_tree" => env.herdr.set_shell_pid(&format!("pane-{bot}"), 2_147_000_000),
                "no_pane_id" => {
                    sqlx::query("UPDATE runs SET pane_id=NULL WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
                }
                _ => {
                    sqlx::query("UPDATE runs SET herdr_session='ghost' WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
                }
            }
            // 分類本身要是 `Unknown`——不能靠「後面停機剛好也做不成」（例如找不到 herdr client 時 stop 自己也會失敗）過關。
            let run = db::active_run(&env.app.db, &bot).await.unwrap().unwrap();
            assert!(matches!(inspect_background(&env.app, &run).await, BackgroundWork::Unknown(_)), "{what}: 問不到要回 Unknown");
            sweep(&env.app, 90).await;
            assert_left_running(&env.app, &bot, what).await;
            assert!(env.herdr.calls_to("agent.send_keys").is_empty(), "{what}: 不該對 agent 按任何鍵");
        }

        // herdr 那條線本身斷了（`pane_shell` 呼叫失敗）：換一個 client 指向不存在的 socket。
        let env = crate::testing::env().await;
        let (bot, _) = idle_bot(&env, "hotel").await;
        let dead = app_with_dead_herdr(&env).await;
        let run = db::active_run(&dead.db, &bot).await.unwrap().unwrap();
        assert!(matches!(inspect_background(&dead, &run).await, BackgroundWork::Unknown(_)), "herdr 連不上要回 Unknown");
        sweep(&dead, 90).await;
        assert_left_running(&dead, &bot, "herdr 連不上").await;
    }

    /// 同一個 app，但 herdr 那條 socket 不存在：任何 herdr 呼叫都會失敗。
    async fn app_with_dead_herdr(env: &crate::testing::Env) -> Arc<App> {
        let data = env.dir.join("data");
        let pool = db::open(&data.join("db.sqlite3")).await.unwrap();
        let cfg = crate::config::ConfigStore::load(data.join("config.toml")).await.unwrap();
        let client = crate::herdr::HerdrClient::new(data.join("no-such-herdr.sock"));
        let app = App::new(pool, client.clone(), client, cfg, data.clone(), data.join("agents-managerd"), 7799, "test-token".into(), "test".into(), false);
        app.connected.store(true, std::sync::atomic::Ordering::SeqCst);
        app
    }

    // ------------------------------------------------------------ issue #123：判斷、標記、停機同一把鎖

    /// `--resume <sid>` 有沒有出現在某一次 `agent.start` 的 argv 裡。
    fn started_with_resume(env: &crate::testing::Env, sid: &str) -> bool {
        env.herdr.calls_to("agent.start").iter().any(|p| {
            let args: Vec<&str> = p.get("args").and_then(|a| a.as_array()).map(|a| a.iter().filter_map(|v| v.as_str()).collect()).unwrap_or_default();
            args.windows(2).any(|w| w == ["--resume", sid])
        })
    }

    /// 在「已標記、還沒停」的那一瞬（`idle_sleep.marked`）放一條路進來，回報它有沒有在那一瞬就跑完、以及事後的結果。
    /// 那一瞬巡邏握著這顆 bot 的鎖：進來的人要是不必拿鎖，就會在這裡跑完（＝看到「標記在、run 還活著」）。
    struct Landing<T> {
        finished_inside_the_window: Arc<AtomicBool>,
        handle: Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<T>>>>,
    }

    fn land_in_the_window<T, F, Fut>(bot: &str, f: F) -> Landing<T>
    where
        T: Send + 'static,
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = T> + Send + 'static,
    {
        let landing = Landing { finished_inside_the_window: Arc::new(AtomicBool::new(false)), handle: Arc::new(std::sync::Mutex::new(None)) };
        let (finished, slot) = (landing.finished_inside_the_window.clone(), landing.handle.clone());
        crate::lifecycle::race_point::arm("idle_sleep.marked", bot, move || async move {
            let h = tokio::spawn(f());
            // 給它足夠多的機會跑：沒被鎖擋住的話，這 300ms 裡一定跑完。
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            finished.store(h.is_finished(), Ordering::SeqCst);
            *slot.lock().unwrap() = Some(h);
        });
        landing
    }

    impl<T> Landing<T> {
        async fn result(self) -> T {
            let h = self.handle.lock().unwrap().take().expect("the race point was never hit — sleep_one did not get to the stop");
            h.await.unwrap()
        }
    }

    /// 一顆已標記、還沒停的 bot 被叫醒（使用者按了啟動）：叫醒要等停機做完，再看到標記、走 `--resume`。
    /// 以前 `wake` 不拿鎖，落在中間就看到「標記在、run 還活著」，把標記清掉回 `false`；接著巡邏把它停掉，
    /// 留下一顆停著、沒有標記、沒有人知道要叫醒的 bot。
    #[tokio::test]
    async fn a_wake_between_the_mark_and_the_stop_waits_for_the_stop_and_then_resumes() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (bot, _) = idle_bot(&env, "juliet").await;
        let stale = stale_cand(&app, &bot).await;

        let (app2, bot2) = (app.clone(), bot.clone());
        let landing = land_in_the_window(&bot, move || async move { wake(&app2, &bot2, "測試").await.map_err(|e| e.to_string()) });
        sleep_one(&app, &stale, Some("sid-1".into()), 90).await;

        assert!(!landing.finished_inside_the_window.load(Ordering::SeqCst), "叫醒不能在巡邏握著鎖的時候就跑完");
        assert_eq!(landing.result().await, Ok(true), "停機做完之後，叫醒看到標記、把它 --resume 接回來");
        assert!(started_with_resume(&env, "sid-1"), "帶著同一段 session 起來");
        assert!(asleep(&app, &bot).await.is_none(), "叫醒成功之後標記才收掉");
        assert!(db::active_run(&app.db, &bot).await.unwrap().is_some());
    }

    /// 同一個窗口，這次落進來的是 `prompt`（使用者在網頁打了一句話）：以前它先在鎖外叫醒，看到「標記在、run 還活著」
    /// 就把標記清掉，拿到鎖時 run 已經被停掉，只得到 409 `bot has no active run`——而且標記沒了，之後的訊息也不會叫醒。
    /// 現在叫醒在 prompt 自己的鎖裡：等巡邏做完，看到標記，`--resume` 接回來再往下送。
    #[tokio::test]
    async fn a_prompt_that_arrives_while_the_bot_is_being_put_to_sleep_wakes_it_instead_of_failing() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (bot, _) = idle_bot(&env, "kilo").await;
        let stale = stale_cand(&app, &bot).await;

        let (app2, bot2) = (app.clone(), bot.clone());
        let landing = land_in_the_window(&bot, move || async move {
            tokio::time::timeout(std::time::Duration::from_secs(30), crate::lifecycle::prompt(&app2, &bot2, "還在嗎", "req-during-sleep"))
                .await
                .map(|r| r.map(|_| ()).map_err(|e| format!("{e:?}")))
        });
        sleep_one(&app, &stale, Some("sid-1".into()), 90).await;

        assert!(!landing.finished_inside_the_window.load(Ordering::SeqCst), "prompt 不能在巡邏握著鎖的時候就跑完");
        let out = landing.result().await.expect("prompt 沒有卡住");
        assert!(!matches!(&out, Err(e) if e.contains("no active run")), "叫醒之後才送，不是 409 沒有 active run：{out:?}");
        assert!(started_with_resume(&env, "sid-1"), "prompt 把睡著的 bot 用 --resume 叫醒");
        assert!(asleep(&app, &bot).await.is_none());
        assert!(db::active_run(&app.db, &bot).await.unwrap().is_some());
    }

    /// 最容易漏的那一個縫：prompt 已經到了、還沒拿到鎖，巡邏整個做完（判斷、標記、停機）。prompt 這時還沒留下任何
    /// DB 證據，巡邏沒有理由不收。拿到鎖之後的 prompt 必須自己看到標記、把它叫醒——**叫醒得在 prompt 的鎖裡**；
    /// 放在鎖外（先叫醒、再拿鎖）的話，叫醒檢查早就過了（沒睡），拿到鎖只剩 409 `bot has no active run`，標記也留著沒人理。
    #[tokio::test]
    async fn a_prompt_already_on_its_way_when_the_bot_goes_to_sleep_wakes_it_under_its_own_lock() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (bot, _) = idle_bot(&env, "november").await;
        let stale = stale_cand(&app, &bot).await;

        let (app2, stale2) = (app.clone(), stale.clone());
        crate::lifecycle::race_point::arm("prompt_before_bot_lock", &bot, move || async move {
            sleep_one(&app2, &stale2, Some("sid-1".into()), 90).await;
        });
        let out = tokio::time::timeout(std::time::Duration::from_secs(30), crate::lifecycle::prompt(&app, &bot, "還在嗎", "req-on-its-way"))
            .await
            .expect("prompt 沒有卡住")
            .map(|_| ())
            .map_err(|e| format!("{e:?}"));

        assert!(!matches!(&out, Err(e) if e.contains("no active run")), "拿到鎖之後先叫醒，不是 409 沒有 active run：{out:?}");
        assert!(started_with_resume(&env, "sid-1"), "巡邏收掉的 bot 被這則 prompt 用 --resume 叫醒");
        assert!(asleep(&app, &bot).await.is_none(), "叫醒成功，標記收掉");
        assert!(db::active_run(&app.db, &bot).await.unwrap().is_some());
    }

    /// 巡邏最後一次判斷「閒著」之後，AGM 才把一張交辦指給它（交辦列先落地、prompt 還沒進來）：鎖裡重讀看得到那張，不收。
    #[tokio::test]
    async fn an_assignment_that_appears_after_the_last_check_keeps_the_bot_awake() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (bot, _) = idle_bot(&env, "lima").await;
        let stale = stale_cand(&app, &bot).await;

        sqlx::query(
            "INSERT INTO supervisor_assignments
               (id, supervisor_id, request_id, target_bot_id, client_request_id, text, status, attempts, expects_review, created_at, updated_at)
             VALUES (?, 'AGM', NULL, ?, ?, '接手這張', 'queued', 0, 1, ?, ?)",
        )
        .bind(db::ulid())
        .bind(&bot)
        .bind(db::ulid())
        .bind(db::now())
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        sleep_one(&app, &stale, Some("sid-1".into()), 90).await;
        assert_left_running(&app, &bot, "交辦在最後一次判斷之後才出現").await;
    }

    /// 背景工作那一項是鎖外問的（貴）、沿用到鎖裡，所以要證明沿用是安全的：判斷之後一個回合來了又走了
    /// （這正是「丟一個背景建置就結束回合」的形狀），「最後動作」被推到現在，這顆不再閒置，不收。
    #[tokio::test]
    async fn a_turn_that_started_and_finished_after_the_background_check_keeps_the_bot_awake() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (bot, run) = idle_bot(&env, "mike").await;
        let stale = stale_cand(&app, &bot).await;

        let conv = db::conversation_id(&app.db, &bot).await.unwrap();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at, completed_at) VALUES (?,?,?,'web','completed','ok',?,?)")
            .bind(db::ulid())
            .bind(&conv)
            .bind(&run)
            .bind(db::now())
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        sleep_one(&app, &stale, Some("sid-1".into()), 90).await;
        assert_left_running(&app, &bot, "判斷之後有一個回合來了又走了").await;
    }

    /// 反向：問得到、而且證明沒有背景工作，這顆就照常收——「不知道就不收」不能變成永遠不收。
    /// shell pid 用一個真的、底下沒有 shell／建置工具的行程（`sleep`），ps 行程樹裡看得到它。
    #[tokio::test]
    async fn once_the_inspection_proves_there_is_no_background_work_the_bot_sleeps() {
        let env = crate::testing::env().await;
        let (bot, _) = idle_bot(&env, "india").await;
        let _shell = provably_no_background_work(&env, &bot);

        sweep(&env.app, 90).await;
        assert!(asleep(&env.app, &bot).await.is_some(), "證明沒有背景工作，照常收");
        assert!(db::active_run(&env.app.db, &bot).await.unwrap().is_none());
    }
}
