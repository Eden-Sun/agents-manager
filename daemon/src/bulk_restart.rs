//! 一鍵把「等著套用 claude 更新」的閒置 bot 全部 exit + resume（SPEC §6.9）。
//!
//! claude 把新版下載好之後只會在 pane 底下印 `Update installed · Restart to update`
//! （daemon 收在 `runs.update_notice`，見 API.md），真正套用的方式就是重啟。十顆 bot 就要點
//! 十次「重啟」，而且每點一次都要自己確認那顆現在有沒有在忙。
//!
//! 這支只做兩件事：**挑**（[`plan`]）與**跑**（[`spawn`]）。跑的部分完全疊在既有的單顆路徑上
//! ——`lifecycle::stop_bot` 之後 `lifecycle::start_bot_with(StartOpts { resume_native: true })`，
//! 也就是「結束目前的 agent，再用它自己剛結束那個 native session `--resume` 回來」。沒有另一套
//! 啟動流程，所以 hook 注入、身份、模型、pane 版面那些全部照舊。
//!
//! 挑的規則刻意保守：**只動 claude、只動真的帶著 update_notice 的、而且只動閒置的那些**。
//! 正在跑（`working`）、卡在提問（`blocked`）、狀態不明（`unknown`）、還有回合在飛的一律跳過並
//! 說出原因——批次操作最不能做的事就是把使用者正在等的那一回合砍掉。

use crate::db;
use crate::lifecycle::{self, LcError, StartOpts};
use crate::state::App;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

/// 一顆候選 bot 的判斷素材（純資料，好寫測試）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cand {
    pub bot_id: String,
    pub name: String,
    pub kind: String,
    /// `bots.managed_by`：`user` / `team` / `child`。只有 `user` 的歸使用者管。
    pub managed_by: String,
    /// `runs.state`：只有 `running` 能重啟，`starting` / `stopping` 都還在變。
    pub state: String,
    /// `runs.agent_status`：`idle` / `working` / `blocked` / `unknown`。
    pub agent_status: String,
    /// 這個 run 帶著 `update_notice`（也就是真的有更新等著套用）。
    pub has_update: bool,
    /// 這個 run 還有一回合沒收掉。
    pub turn_in_flight: bool,
}

/// 為什麼這顆沒被重啟。`code` 給 API / 前端比對，`label` 給人看。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    TeamMember,
    NotRunning,
    Working,
    Blocked,
    UnknownStatus,
    TurnInFlight,
}

impl Skip {
    pub fn code(self) -> &'static str {
        match self {
            Skip::TeamMember => "team_member",
            Skip::NotRunning => "not_running",
            Skip::Working => "working",
            Skip::Blocked => "blocked",
            Skip::UnknownStatus => "unknown_status",
            Skip::TurnInFlight => "turn_in_flight",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Skip::TeamMember => "是 team 的成員，由 team 排程管",
            Skip::NotRunning => "還在啟動或關閉中",
            Skip::Working => "正在跑，重啟會把這一回合砍掉",
            Skip::Blocked => "卡在提問，等人回答",
            Skip::UnknownStatus => "狀態不明，不確定它在不在忙",
            Skip::TurnInFlight => "還有一回合沒收掉",
        }
    }
}

/// 這顆算不算候選——不是 claude、或根本沒有更新在等，就連「跳過」都不必列出來（那不是使用者
/// 按這顆按鈕時心裡想的東西，列出來只會把真正要看的那幾行淹掉）。
pub fn is_candidate(c: &Cand) -> bool {
    c.kind == "claude" && c.has_update
}

/// 候選裡誰重啟、誰跳過。
///
/// 順序即優先序：先看這顆歸不歸使用者管，再看 run 本身穩不穩（`state`），再看 agent 在不在忙，
/// 最後才看回合。回報的理由取第一個中的那個，因為那是使用者最該先處理的那件事。
///
/// `team` 不碰：成員的生死歸 team 排程管——批次重啟插手只會讓排程對不上自己記得的 run。
///
/// 子 agent（`child`）**進來**（2026-09-12 使用者：三顆子 agent 全被跳過，更新套不上去）。
/// 它們跟別人一樣是帶著更新的 claude，只是不能照一般路徑重開 pane，所以執行時改走
/// [`crate::lifecycle::restart_child_in_pane`]——在它自己那個 pane 裡 exit + resume。
pub fn plan(cands: &[Cand]) -> (Vec<&Cand>, Vec<(&Cand, Skip)>) {
    let mut go = Vec::new();
    let mut skip = Vec::new();
    for c in cands.iter().filter(|c| is_candidate(c)) {
        let why = if c.managed_by == "team" {
            Some(Skip::TeamMember)
        } else if c.state != "running" {
            Some(Skip::NotRunning)
        } else if c.agent_status == "working" {
            Some(Skip::Working)
        } else if c.agent_status == "blocked" {
            Some(Skip::Blocked)
        } else if c.agent_status != "idle" {
            Some(Skip::UnknownStatus)
        } else if c.turn_in_flight {
            Some(Skip::TurnInFlight)
        } else {
            None
        };
        match why {
            Some(w) => skip.push((c, w)),
            None => go.push(c),
        }
    }
    (go, skip)
}

/// 從資料庫湊出候選清單。只走有 active run 的 bot——沒在跑的本來就沒有更新要套用。
pub async fn candidates(app: &Arc<App>) -> anyhow::Result<Vec<Cand>> {
    let mut out = Vec::new();
    for run in db::all_active_runs(&app.db).await? {
        let Some(bot) = db::bot(&app.db, &run.bot_id).await? else { continue };
        if bot.deleted_at.is_some() {
            continue;
        }
        out.push(Cand {
            bot_id: bot.id.clone(),
            name: bot.name.clone(),
            kind: bot.kind.clone(),
            managed_by: bot.managed_by.clone(),
            state: run.state.clone(),
            agent_status: run.agent_status.clone(),
            has_update: run.update_notice.as_deref().is_some_and(|s| !s.trim().is_empty()),
            turn_in_flight: db::in_flight_turn(&app.db, &run.id).await?.is_some(),
        });
    }
    Ok(out)
}

fn skip_json(c: &Cand, w: Skip) -> serde_json::Value {
    json!({"bot_id": c.bot_id, "name": c.name, "reason": w.code(), "reason_label": w.label()})
}

/// 開一批重啟：回傳給 HTTP 呼叫端的計畫，實際的 exit + resume 在背景一顆一顆跑。
///
/// 為什麼不同步做完再回：一顆 `stop_bot` 最久要等 agent 十秒才放棄，五顆就一分鐘——那是會把
/// HTTP 連線拖爆的長度。所以這裡只回計畫，進度與結果走 WS（`bots_restart_progress` /
/// `bots_restart_done`），前端照著畫「第幾顆 / 共幾顆」與最後的摘要。
pub async fn spawn(app: &Arc<App>) -> anyhow::Result<serde_json::Value> {
    let cands = candidates(app).await?;
    let (go, skipped) = plan(&cands);
    let batch_id = db::ulid();
    let supervisor = supervisor_bot_id(app).await;
    let targets = supervisor_last(go.iter().map(|c| (c.bot_id.clone(), c.name.clone())).collect(), supervisor.as_deref());
    let planned: Vec<serde_json::Value> = targets.iter().map(|(id, name)| json!({"bot_id": id, "name": name})).collect();
    let skipped_json: Vec<serde_json::Value> = skipped.iter().map(|(c, w)| skip_json(c, *w)).collect();
    let total = targets.len();

    if total > 0 {
        let app2 = app.clone();
        let bid = batch_id.clone();
        let skipped_for_task = skipped_json.clone();
        tokio::spawn(async move { run_batch(&app2, &bid, targets, skipped_for_task, supervisor).await });
    } else {
        // 沒有可重啟的也要送一次 done：前端才不必自己分「送出去了但什麼都沒發生」這一種。
        app.emit(
            "bots_restart_done",
            json!({"batch_id": batch_id, "ok": [], "failed": [], "skipped": skipped_json}),
        )
        .await;
    }

    Ok(json!({
        "batch_id": batch_id,
        "total": total,
        "planned": planned,
        "skipped": skipped_json,
    }))
}

/// 一顆一顆跑。**一顆失敗不中斷整批**——批次的價值就在於不用一顆一顆顧，中途停下等於白做。
async fn run_batch(
    app: &Arc<App>,
    batch_id: &str,
    targets: Vec<(String, String)>,
    skipped: Vec<serde_json::Value>,
    supervisor: Option<String>,
) {
    let total = targets.len();
    let mut ok: Vec<serde_json::Value> = Vec::new();
    let mut failed: Vec<serde_json::Value> = Vec::new();
    for (i, (bot_id, name)) in targets.into_iter().enumerate() {
        let index = i + 1;
        app.emit(
            "bots_restart_progress",
            json!({"batch_id": batch_id, "index": index, "total": total,
                   "bot_id": bot_id, "name": name, "status": "restarting"}),
        )
        .await;
        let res = restart_resuming(app, &bot_id).await;
        match res {
            Ok(run_id) => {
                tracing::info!(bot = %name, run = %run_id, "restarted for the claude update (resumed)");
                ok.push(json!({"bot_id": bot_id, "name": name, "run_id": run_id}));
                app.emit(
                    "bots_restart_progress",
                    json!({"batch_id": batch_id, "index": index, "total": total,
                           "bot_id": bot_id, "name": name, "status": "ok"}),
                )
                .await;
            }
            Err(e) => {
                let msg = format!("{e:#}");
                tracing::warn!(bot = %name, error = %msg, "restart for the claude update failed");
                settle_failed_restart(app, &bot_id).await;
                // Its own kind, not `health_changed`: the supervisor has to be able to tell "a bot
                // the batch took down did not come back" from background noise.
                let _ = crate::supervisor::store::push_inbox(
                    &app.db,
                    &format!("bot_restart_failed:{batch_id}:{bot_id}"),
                    "bot_restart_failed",
                    None,
                    Some(&bot_id),
                    None,
                    &json!({"batch_id": batch_id, "bot_id": bot_id, "name": name, "error": msg}),
                )
                .await;
                failed.push(json!({"bot_id": bot_id, "name": name, "error": msg}));
                app.emit(
                    "bots_restart_progress",
                    json!({"batch_id": batch_id, "index": index, "total": total,
                           "bot_id": bot_id, "name": name, "status": "failed", "error": msg}),
                )
                .await;
            }
        }
        app.emit("bot_changed", json!({"bot_id": bot_id})).await;
        if supervisor.as_deref() == Some(bot_id.as_str()) {
            tokio::spawn(verify_supervisor_back(app.clone(), bot_id.clone(), name.clone(), batch_id.to_string()));
        }
    }
    app.emit(
        "bots_restart_done",
        json!({"batch_id": batch_id, "ok": ok, "failed": failed, "skipped": skipped}),
    )
    .await;
}

/// 單顆的 exit + resume：`stop_bot` 把 run 收掉（`ended_at` 一寫上去，剛剛那個
/// `native_session_id` 就成了 [`db::last_native_session_id`] 找得到的「上一個 session」），
/// 接著用 `resume_native` 起回來，claude 拿到的就是 `--resume <session>`。
///
/// 跟 `lifecycle::restart_bot` 的差別只有這個旗標——那條路是「重新開始」，這條是「接著跑」。
async fn restart_resuming(app: &Arc<App>, bot_id: &str) -> anyhow::Result<String> {
    // 子 agent 的 pane 是父 agent 開的：關掉再開一個新的等於把它搬家，所以走原地重啟那條路。
    if db::bot(&app.db, bot_id).await.ok().flatten().is_some_and(|b| b.managed_by == "child") {
        return lifecycle::restart_child_in_pane(app, bot_id).await.map_err(why);
    }
    // One hold of the bot's lock for both halves — see `lifecycle::restart_bot_with` for the
    // 2026-09-10 23:02 race this closes.
    lifecycle::restart_bot_with(app, bot_id, StartOpts { resume_native: true }).await.map_err(why)
}

/// The supervisor's bot (AGM), if one is set up. Read straight off the row: `get_or_init` would
/// create a supervisor where the user never asked for one.
async fn supervisor_bot_id(app: &Arc<App>) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>("SELECT bot_id FROM supervisors LIMIT 1")
        .fetch_optional(&app.db)
        .await
        .ok()
        .flatten()
        .flatten()
        .filter(|s| !s.is_empty())
}

/// The supervisor goes **last**: it is the one bot that notices and repairs the others, so it
/// should not be down while they restart — and if the batch goes wrong, it should still have
/// been up to see it.
pub fn supervisor_last(mut targets: Vec<(String, String)>, supervisor: Option<&str>) -> Vec<(String, String)> {
    if let Some(sid) = supervisor {
        if let Some(i) = targets.iter().position(|(id, _)| id == sid) {
            let t = targets.remove(i);
            targets.push(t);
        }
    }
    targets
}

/// A restart that failed must not leave a run behind that points at a closed pane: the UI would
/// show the bot as running and nothing would ever start it again. End such a run, so the bot reads
/// as stopped and can be started.
async fn settle_failed_restart(app: &Arc<App>, bot_id: &str) {
    let (Ok(Some(run)), Ok(Some(bot))) = (db::active_run(&app.db, bot_id).await, db::bot(&app.db, bot_id).await) else {
        return;
    };
    if !lifecycle::run_alive(app, &run, &bot).await {
        tracing::warn!(bot = %bot.name, run = %run.id, "restart failed and left a run with no live pane; ending it");
        lifecycle::mark_run_exited(app, &run.id, "restart for the claude update failed").await;
        app.emit_bot_status(bot_id).await;
    }
}

/// After the supervisor's own restart, check for [`SUPERVISOR_WINDOW`] that it is really back
/// (running, pane open, agent listed). If it is not, start it once more and put the outcome in
/// the inbox (`supervisor_restart_retry`) — the one bot that would otherwise notice is this one.
async fn verify_supervisor_back(app: Arc<App>, bot_id: String, name: String, batch_id: String) {
    let mut waited = Duration::ZERO;
    while waited < SUPERVISOR_WINDOW {
        tokio::time::sleep(SUPERVISOR_POLL).await;
        waited += SUPERVISOR_POLL;
        if supervisor_is_back(&app, &bot_id).await {
            tracing::info!(bot = %name, secs = waited.as_secs(), "supervisor is back after the update restart");
            return;
        }
    }
    tracing::warn!(bot = %name, "supervisor did not come back within 60s of the update restart; starting it once more");
    settle_failed_restart(&app, &bot_id).await;
    let res = match db::active_run(&app.db, &bot_id).await {
        Ok(Some(run)) => Ok(run.id),
        _ => lifecycle::start_bot_with(&app, &bot_id, StartOpts { resume_native: true }).await.map_err(why),
    };
    let (ok, error) = match &res {
        Ok(_) => (true, None),
        Err(e) => (false, Some(format!("{e:#}"))),
    };
    tracing::info!(bot = %name, ok, ?error, "supervisor restart retry finished");
    let _ = crate::supervisor::store::push_inbox(
        &app.db,
        &format!("supervisor_restart_retry:{batch_id}"),
        "supervisor_restart_retry",
        None,
        Some(&bot_id),
        None,
        &json!({"batch_id": batch_id, "bot_id": bot_id, "name": name, "ok": ok, "error": error}),
    )
    .await;
    app.emit("bot_changed", json!({"bot_id": bot_id})).await;
}

async fn supervisor_is_back(app: &Arc<App>, bot_id: &str) -> bool {
    let Ok(Some(run)) = db::active_run(&app.db, bot_id).await else { return false };
    if run.state != "running" {
        return false;
    }
    let Ok(Some(bot)) = db::bot(&app.db, bot_id).await else { return false };
    lifecycle::run_alive(app, &run, &bot).await
}

/// How long the supervisor gets to come back after its own update restart.
const SUPERVISOR_WINDOW: Duration = Duration::from_secs(60);
const SUPERVISOR_POLL: Duration = Duration::from_secs(5);

/// `LcError` 沒有 `Display`（它是拿來變成 HTTP body 的）。批次的失敗要進 WS 事件、最後印在
/// 摘要裡給人看，所以這裡把它攤成一句話——衝突就取 daemon 自己寫的 `reason`。
fn why(e: LcError) -> anyhow::Error {
    let s = match e {
        LcError::NotFound(what) => format!("找不到 {what}"),
        LcError::Upstream(m) | LcError::Bad(m) => m,
        LcError::Conflict(v) | LcError::BadValue(v) => v
            .get("reason")
            .or_else(|| v.get("message"))
            .and_then(|x| x.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| v.to_string()),
    };
    anyhow::anyhow!(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(name: &str, kind: &str, state: &str, status: &str, has_update: bool, in_flight: bool) -> Cand {
        Cand {
            bot_id: format!("b-{name}"),
            name: name.into(),
            kind: kind.into(),
            managed_by: "user".into(),
            state: state.into(),
            agent_status: status.into(),
            has_update,
            turn_in_flight: in_flight,
        }
    }

    /// 閒置的 claude 帶著更新 → 重啟；working / blocked → 跳過並說出原因。
    #[test]
    fn working_and_blocked_are_skipped() {
        let cands = vec![
            cand("idle-1", "claude", "running", "idle", true, false),
            cand("busy", "claude", "running", "working", true, false),
            cand("asking", "claude", "running", "blocked", true, false),
            cand("idle-2", "claude", "running", "idle", true, false),
        ];
        let (go, skip) = plan(&cands);
        assert_eq!(go.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["idle-1", "idle-2"]);
        assert_eq!(
            skip.iter().map(|(c, w)| (c.name.as_str(), w.code())).collect::<Vec<_>>(),
            [("busy", "working"), ("asking", "blocked")]
        );
    }

    /// 回合還在飛的不能動——即使 herdr 已經把 agent 報成 idle（§4.3 的備援還沒收掉那一回合）。
    #[test]
    fn an_in_flight_turn_is_skipped_even_when_idle() {
        let cands = vec![cand("mid-turn", "claude", "running", "idle", true, true)];
        let (go, skip) = plan(&cands);
        assert!(go.is_empty());
        assert_eq!(skip[0].1, Skip::TurnInFlight);
    }

    /// 狀態不明與還在啟動 / 關閉中的一律不碰。
    #[test]
    fn unstable_runs_are_skipped() {
        let cands = vec![
            cand("booting", "claude", "starting", "idle", true, false),
            cand("dying", "claude", "stopping", "idle", true, false),
            cand("dunno", "claude", "running", "unknown", true, false),
        ];
        let (go, skip) = plan(&cands);
        assert!(go.is_empty());
        assert_eq!(
            skip.iter().map(|(_, w)| w.code()).collect::<Vec<_>>(),
            ["not_running", "not_running", "unknown_status"]
        );
    }

    /// 不是 claude、或根本沒有更新在等的，連「跳過」都不列——那不是這顆按鈕在講的事。
    #[test]
    fn non_candidates_are_not_reported_at_all() {
        let cands = vec![
            cand("cx", "codex", "running", "idle", true, false),
            cand("gk", "grok", "running", "working", true, false),
            cand("cl-no-update", "claude", "running", "idle", false, false),
            cand("cl-yes", "claude", "running", "idle", true, false),
        ];
        let (go, skip) = plan(&cands);
        assert_eq!(go.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["cl-yes"]);
        assert!(skip.is_empty(), "{skip:?}");
        assert!(!is_candidate(&cands[0]));
        assert!(!is_candidate(&cands[2]));
    }

    /// team 成員不歸這顆按鈕管（run 由 team 排程記著）；子 agent 歸——它在自己的 pane 裡重啟
    /// （2026-09-12 使用者：ns2 / race / sup 三顆全被跳過，更新永遠套不上去）。
    #[test]
    fn children_join_the_batch_and_team_members_do_not() {
        let kid = Cand { managed_by: "child".into(), ..cand("kid", "claude", "running", "idle", true, false) };
        let member = Cand { managed_by: "team".into(), ..cand("dev-1", "claude", "running", "idle", true, false) };
        let mine = cand("mine", "claude", "running", "idle", true, false);
        let cands = [kid, member, mine];
        let (go, skip) = plan(&cands);
        assert_eq!(go.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["kid", "mine"]);
        assert_eq!(skip.iter().map(|(_, w)| w.code()).collect::<Vec<_>>(), ["team_member"]);
    }

    /// 忙碌判斷對子 agent 一樣成立——歸誰管不影響「現在能不能動它」。
    #[test]
    fn a_busy_child_is_still_skipped_for_being_busy() {
        let kid = Cand { managed_by: "child".into(), ..cand("kid", "claude", "running", "working", true, false) };
        let (go, skip) = plan(std::slice::from_ref(&kid));
        assert!(go.is_empty());
        assert_eq!(skip[0].1, Skip::Working);
    }

    /// 總管（AGM）排在最後重啟，其他順序不動；沒有總管或總管不在這批裡時原樣不變。
    #[test]
    fn the_supervisor_is_restarted_last() {
        let t = |ids: &[&str]| ids.iter().map(|i| (i.to_string(), format!("n-{i}"))).collect::<Vec<_>>();
        let order = |v: Vec<(String, String)>| v.into_iter().map(|(id, _)| id).collect::<Vec<_>>();
        assert_eq!(order(supervisor_last(t(&["a", "agm", "b", "c"]), Some("agm"))), ["a", "b", "c", "agm"]);
        assert_eq!(order(supervisor_last(t(&["a", "b"]), Some("agm"))), ["a", "b"]);
        assert_eq!(order(supervisor_last(t(&["agm", "a"]), None)), ["agm", "a"]);
    }

    /// A restart that fails leaves no run pointing at a dead pane, and says so in the supervisor's
    /// inbox under its own kind — not `health_changed`.
    #[tokio::test]
    async fn a_failed_restart_leaves_no_dead_run_and_tells_the_supervisor() {
        let env = crate::team::testing::env().await;
        let app = env.app.clone();
        let bot = db::ulid();
        // An identity this host does not know: `start` refuses before spawning anything.
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, identity, created_at)
             VALUES (?,?,'alfa','claude','[]',0,1,'tok','nope',?)",
        )
        .bind(&bot)
        .bind(&env.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        crate::team::testing::fake_run(&app, &bot).await;

        run_batch(&app, "batch-1", vec![(bot.clone(), "alfa".into())], vec![], None).await;

        assert!(db::active_run(&app.db, &bot).await.unwrap().is_none(), "no run is left behind for a bot that did not come back");
        let inbox = crate::supervisor::store::inbox(&app.db, 50).await.unwrap();
        let ev = inbox.iter().find(|e| e.kind == "bot_restart_failed").expect("the supervisor is told");
        assert_eq!(ev.bot_id.as_deref(), Some(bot.as_str()));
    }

    /// 什麼都沒有時是空計畫，不是錯誤。
    #[test]
    fn nothing_to_do_is_an_empty_plan() {
        let (go, skip) = plan(&[]);
        assert!(go.is_empty() && skip.is_empty());
    }
}
