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
//!   不收自己，而且 watchdog 反正會把它們拉回來。

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
    /// `runs.state`。
    pub state: String,
    /// `runs.agent_status`：`idle` / `working` / `blocked` / `unknown`。
    pub agent_status: String,
    /// 這個 run 還有一回合沒收掉。
    pub turn_in_flight: bool,
    /// 還有一則排隊中的 web prompt 等著送進去。
    pub queued_turn: bool,
    /// AGM 還有沒結案的 assignment 指著它。
    pub open_assignment: bool,
    /// 收起來之後接得回來（有 native session、transcript 還在、kind 支援 `--resume`）。
    pub resumable: bool,
    /// 最後一次有動作到現在幾分鐘。
    pub idle_minutes: i64,
}

/// 為什麼這顆不收。`code` 給 API／log 比對，`label` 給人看。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    Supervisor,
    TeamMember,
    Child,
    NotRunning,
    Working,
    Blocked,
    UnknownStatus,
    TurnInFlight,
    QueuedTurn,
    OpenAssignment,
    NoResume,
    StillWarm,
}

impl Skip {
    pub fn code(self) -> &'static str {
        match self {
            Skip::Supervisor => "supervisor",
            Skip::TeamMember => "team_member",
            Skip::Child => "child",
            Skip::NotRunning => "not_running",
            Skip::Working => "working",
            Skip::Blocked => "blocked",
            Skip::UnknownStatus => "unknown_status",
            Skip::TurnInFlight => "turn_in_flight",
            Skip::QueuedTurn => "queued_turn",
            Skip::OpenAssignment => "open_assignment",
            Skip::NoResume => "no_resume",
            Skip::StillWarm => "still_warm",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Skip::Supervisor => "總管自己，巡邏的人不收自己",
            Skip::TeamMember => "是 team 的成員，由 team 排程管",
            Skip::Child => "是子 agent，pane 歸父 agent 管，daemon 起不回來",
            Skip::NotRunning => "還在啟動或關閉中",
            Skip::Working => "正在跑",
            Skip::Blocked => "卡在提問，等人回答",
            Skip::UnknownStatus => "狀態不明，不確定它在不在忙",
            Skip::TurnInFlight => "還有一回合沒收掉",
            Skip::QueuedTurn => "還有排隊中的訊息沒送進去",
            Skip::OpenAssignment => "AGM 還有沒結案的 assignment 指著它",
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
async fn last_activity(app: &Arc<App>, bot_id: &str, run: &db::Run) -> String {
    let conv = db::conversation_id(&app.db, bot_id).await.unwrap_or_default();
    let latest: Option<String> = sqlx::query_scalar(
        "SELECT MAX(ts) FROM (
           SELECT MAX(COALESCE(completed_at, created_at)) AS ts FROM turns WHERE conversation_id = ?1
           UNION ALL SELECT MAX(created_at) FROM messages WHERE conversation_id = ?1
           UNION ALL SELECT ?2
         )",
    )
    .bind(&conv)
    .bind(&run.started_at)
    .fetch_optional(&app.db)
    .await
    .ok()
    .flatten();
    latest.unwrap_or_else(|| run.started_at.clone())
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
async fn supervisor_bot_ids(app: &Arc<App>) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    for q in ["SELECT bot_id FROM supervisors", "SELECT bot_id FROM supervisor_roles"] {
        if let Ok(rows) = sqlx::query_scalar::<_, Option<String>>(q).fetch_all(&app.db).await {
            out.extend(rows.into_iter().flatten().filter(|s| !s.is_empty()));
        }
    }
    out
}

async fn has_open_assignment(app: &Arc<App>, bot_id: &str) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM supervisor_assignments
          WHERE target_bot_id = ? AND status IN ('queued','delivered','unknown')",
    )
    .bind(bot_id)
    .fetch_one(&app.db)
    .await
    .unwrap_or(0)
        > 0
}

/// 一顆 bot 此刻的判斷素材。`sup` 是總管的 bot id。
async fn cand_for(app: &Arc<App>, run: &db::Run, sup: &std::collections::HashSet<String>) -> anyhow::Result<Option<Cand>> {
    let Some(bot) = db::bot(&app.db, &run.bot_id).await? else { return Ok(None) };
    if bot.deleted_at.is_some() {
        return Ok(None);
    }
    let host = db::bot_host(&app.db, &bot.id).await.unwrap_or_else(|_| crate::config::LOCAL_HOST.to_string());
    let seen = last_activity(app, &bot.id, run).await;
    Ok(Some(Cand {
        bot_id: bot.id.clone(),
        name: bot.name.clone(),
        managed_by: bot.managed_by.clone(),
        is_supervisor: sup.contains(&bot.id),
        state: run.state.clone(),
        agent_status: run.agent_status.clone(),
        turn_in_flight: db::in_flight_turn(&app.db, &run.id).await?.is_some(),
        queued_turn: db::queued_turn_for_bot(&app.db, &bot.id).await?.is_some(),
        open_assignment: has_open_assignment(app, &bot.id).await,
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
pub async fn candidates(app: &Arc<App>) -> anyhow::Result<Vec<Cand>> {
    let sup = supervisor_bot_ids(app).await;
    let mut out = Vec::new();
    for run in db::all_active_runs(&app.db).await? {
        if let Some(c) = cand_for(app, &run, &sup).await? {
            out.push(c);
        }
    }
    Ok(out)
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

/// 這顆現在是被收起來的嗎（`(slept_at, idle_minutes)`）。
pub async fn asleep(app: &Arc<App>, bot_id: &str) -> Option<(String, i64)> {
    sqlx::query_as::<_, (String, i64)>("SELECT slept_at, idle_minutes FROM bot_sleeps WHERE bot_id = ?")
        .bind(bot_id)
        .fetch_optional(&app.db)
        .await
        .ok()
        .flatten()
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

/// 收一顆：標記 → `stop_bot`（送 ctrl+c 收 agent、關 pane）→ 在它自己的對話裡說一聲為什麼。
async fn sleep_one(app: &Arc<App>, c: &Cand, session: Option<String>) {
    if let Err(e) = mark_asleep(app, c, session.as_deref()).await {
        tracing::warn!(bot = %c.name, error = %e, "could not record the sleep; leaving the bot running");
        return;
    }
    match lifecycle::stop_bot(app, &c.bot_id).await {
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
pub async fn wake(app: &Arc<App>, bot_id: &str, why: &str) -> anyhow::Result<bool> {
    let Some((_, mins)) = asleep(app, bot_id).await else { return Ok(false) };
    if db::active_run(&app.db, bot_id).await?.is_some() {
        clear_asleep(app, bot_id).await;
        return Ok(false);
    }
    // `resume_required`：接不回原本那段對話時**不要**默默開一段新的——「只留下 resume」是這個
    // 功能的全部前提，悄悄換成空白對話等於把使用者的脈絡弄丟還不說（上游 2026-09-17 的
    // `?resume=native` 用的是同一個旗標）。接不回就退回開新對話，但在那顆 bot 自己的對話裡講清楚。
    let resumed = StartOpts { resume_native: true, resume_required: true, ..Default::default() };
    match lifecycle::start_bot_with(app, bot_id, resumed).await {
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
            match lifecycle::start_bot(app, bot_id).await {
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
    let cands = match candidates(app).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "idle sweep could not read the fleet");
            return;
        }
    };
    let sup = supervisor_bot_ids(app).await;
    for c in cands.iter().filter(|c| decide(c, threshold).is_ok()) {
        // 收之前再確認一次：湊完清單到輪到這顆，中間隔了前面每一顆的停機時間（一顆最久十秒），
        // 這段時間裡它可能已經被派了工作。
        let Ok(Some(run)) = db::active_run(&app.db, &c.bot_id).await else { continue };
        let session = run.native_session_id.clone();
        match cand_for(app, &run, &sup).await {
            Ok(Some(f)) if decide(&f, threshold).is_ok() => sleep_one(app, &f, session).await,
            _ => continue,
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
            state: "running".into(),
            agent_status: "idle".into(),
            turn_in_flight: false,
            queued_turn: false,
            open_assignment: false,
            resumable: true,
            idle_minutes: 120,
        }
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
}
