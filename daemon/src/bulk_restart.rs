//! 一鍵把「等著套用 claude 更新」的閒置 bot 全部 exit + resume（SPEC §6.9）。
//!
//! 跑的部分疊在既有單顆路徑（stop + `resume_native` start）上，沒有另一套啟動流程。
//! 挑的規則刻意保守：只動帶著 update_notice 的閒置 claude——批次最不能做的就是砍掉使用者正在等的回合。

use crate::db;
use crate::lifecycle::{self, LcError, StartOpts};
use crate::state::App;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

/// 純資料，好寫測試。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cand {
    pub bot_id: String,
    pub name: String,
    pub kind: String,
    pub managed_by: String,
    pub state: String,
    pub agent_status: String,
    pub has_update: bool,
    pub turn_in_flight: bool,
    /// 使用者自己的 herdr `default` session（SPEC §6.5.1）：daemon 只觀察，不開、不關它的 pane。
    pub default_session: bool,
}

/// `code` 給 API / 前端比對，`label` 給人看。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    DefaultSession,
    NotRunning,
    Working,
    Blocked,
    UnknownStatus,
    TurnInFlight,
    /// 排到它的時候已經不用重啟了：更新套用過、run 不在了、bot 被刪了。
    NoLongerPending,
}

impl Skip {
    pub fn code(self) -> &'static str {
        match self {
            Skip::DefaultSession => "default_session",
            Skip::NotRunning => "not_running",
            Skip::Working => "working",
            Skip::Blocked => "blocked",
            Skip::UnknownStatus => "unknown_status",
            Skip::TurnInFlight => "turn_in_flight",
            Skip::NoLongerPending => "no_longer_pending",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Skip::DefaultSession => "在你自己的 herdr default session 裡，daemon 不動它的 pane",
            Skip::NotRunning => "還在啟動或關閉中",
            Skip::Working => "正在跑，重啟會把這一回合砍掉",
            Skip::Blocked => "卡在提問，等人回答",
            Skip::UnknownStatus => "狀態不明，不確定它在不在忙",
            Skip::TurnInFlight => "還有一回合沒收掉",
            Skip::NoLongerPending => "排到它時已經不用重啟了（更新套用過或 run 不在了）",
        }
    }
}

/// 非候選連「跳過」都不列，免得淹掉真正要看的那幾行。
pub fn is_candidate(c: &Cand) -> bool {
    c.kind == "claude" && c.has_update
}

/// 這顆候選為什麼不能動；`None`＝可以重啟。順序即優先序，回報理由取第一個命中的（使用者最該先處理的那件）。
/// 計畫時用一次，**輪到它真的要重啟前再用一次**（[`run_batch`]）。
pub fn skip_reason(c: &Cand) -> Option<Skip> {
    if c.default_session {
        // SPEC §6.5.1：重啟會關掉使用者自己的 pane（2026-09-12 review #4）。
        Some(Skip::DefaultSession)
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
    }
}

/// 子 agent 也進來，走 [`crate::lifecycle::restart_child_in_pane`]（2026-09-12 使用者：三顆子 agent 全被跳過）。
pub fn plan(cands: &[Cand]) -> (Vec<&Cand>, Vec<(&Cand, Skip)>) {
    let mut go = Vec::new();
    let mut skip = Vec::new();
    for c in cands.iter().filter(|c| is_candidate(c)) {
        match skip_reason(c) {
            Some(w) => skip.push((c, w)),
            None => go.push(c),
        }
    }
    (go, skip)
}

async fn cand_of(app: &Arc<App>, run: &db::Run, bot: &db::Bot) -> anyhow::Result<Cand> {
    Ok(Cand {
        bot_id: bot.id.clone(),
        name: bot.name.clone(),
        kind: bot.kind.clone(),
        managed_by: bot.managed_by.clone(),
        state: run.state.clone(),
        agent_status: run.agent_status.clone(),
        has_update: run.update_notice.as_deref().is_some_and(|s| !s.trim().is_empty()),
        turn_in_flight: db::in_flight_turn(&app.db, &run.id).await?.is_some(),
        default_session: lifecycle::in_default_session(run) || bot.herdr_session.as_deref() == Some("default"),
    })
}

pub async fn candidates(app: &Arc<App>) -> anyhow::Result<Vec<Cand>> {
    let mut out = Vec::new();
    for run in db::all_active_runs(&app.db).await? {
        let Some(bot) = db::bot(&app.db, &run.bot_id).await? else { continue };
        if bot.deleted_at.is_some() {
            continue;
        }
        out.push(cand_of(app, &run, &bot).await?);
    }
    Ok(out)
}

/// 輪到這顆真的要重啟前再看一次（計畫是按下去那一刻的快照）。`None`＝還是可以重啟。
async fn recheck(app: &Arc<App>, bot_id: &str) -> Option<Skip> {
    let (Ok(Some(run)), Ok(Some(bot))) = (db::active_run(&app.db, bot_id).await, db::bot(&app.db, bot_id).await) else {
        return Some(Skip::NoLongerPending);
    };
    if bot.deleted_at.is_some() {
        return Some(Skip::NoLongerPending);
    }
    let Ok(c) = cand_of(app, &run, &bot).await else { return Some(Skip::UnknownStatus) };
    if !is_candidate(&c) {
        return Some(Skip::NoLongerPending);
    }
    skip_reason(&c)
}

/// 這個 daemon（以 `data_dir` 分）正在跑的那一批。同時只准一批：連按兩次、或使用者按一次 AGM 也叫一次，
/// 以前會生出兩份重疊的清單，同一顆 bot 被重啟兩次（review 2026-09-16）。
fn running_batches() -> &'static std::sync::Mutex<std::collections::HashMap<String, String>> {
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, String>>> = std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

/// 批次結束（含 panic）就放掉。
struct BatchSlot(String);

impl Drop for BatchSlot {
    fn drop(&mut self) {
        running_batches().lock().unwrap().remove(&self.0);
    }
}

fn skip_json(c: &Cand, w: Skip) -> serde_json::Value {
    json!({"bot_id": c.bot_id, "name": c.name, "reason": w.code(), "reason_label": w.label()})
}

/// 只回計畫、背景執行：一顆 `stop_bot` 最久等十秒，同步做會拖爆 HTTP；進度走 WS。
/// 已經有一批在跑：不另開，回那一批的 `batch_id`（`already_running: true`），進度與結果照舊從那一批的事件來。
pub async fn spawn(app: &Arc<App>) -> anyhow::Result<serde_json::Value> {
    let slot_key = app.data_dir.display().to_string();
    let batch_id = db::ulid();
    {
        let mut running = running_batches().lock().unwrap();
        if let Some(existing) = running.get(&slot_key) {
            return Ok(json!({"batch_id": existing, "total": 0, "planned": [], "skipped": [], "already_running": true}));
        }
        running.insert(slot_key.clone(), batch_id.clone());
    }
    let slot = BatchSlot(slot_key);
    let cands = candidates(app).await?;
    let (go, skipped) = plan(&cands);
    let supervisor = supervisor_bot_id(app).await;
    let targets = supervisor_last(go.iter().map(|c| (c.bot_id.clone(), c.name.clone())).collect(), supervisor.as_deref());
    let planned: Vec<serde_json::Value> = targets.iter().map(|(id, name)| json!({"bot_id": id, "name": name})).collect();
    let skipped_json: Vec<serde_json::Value> = skipped.iter().map(|(c, w)| skip_json(c, *w)).collect();
    let total = targets.len();

    if total > 0 {
        let app2 = app.clone();
        let bid = batch_id.clone();
        let skipped_for_task = skipped_json.clone();
        tokio::spawn(async move {
            let _slot = slot;
            run_batch(&app2, &bid, targets, skipped_for_task, supervisor).await
        });
    } else {
        drop(slot);
        // 空批次也送 done，前端不必另外處理「什麼都沒發生」。
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

/// 一顆失敗不中斷整批。
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
    let mut skipped = skipped;
    for (i, (bot_id, name)) in targets.into_iter().enumerate() {
        let index = i + 1;
        // 計畫是按下去那一刻的快照，一顆 `stop_bot` 最久十秒，排在後面的要等上一兩分鐘；這段時間裡 AGM 派了工、
        // 使用者打了字，它就在回合中了。硬重啟會把回合連同 in-flight turn 一起砍掉（review 2026-09-16）。
        let skip_now = |why: Skip| {
            let row = json!({"bot_id": bot_id, "name": name, "reason": why.code(), "reason_label": why.label()});
            let ev = json!({"batch_id": batch_id, "index": index, "total": total, "bot_id": bot_id, "name": name,
                            "status": "skipped", "reason": why.code(), "reason_label": why.label()});
            (row, ev)
        };
        if let Some(why) = recheck(app, &bot_id).await {
            tracing::info!(bot = %name, reason = why.code(), "skipped at restart time: its state changed after the plan");
            let (row, ev) = skip_now(why);
            app.emit("bots_restart_progress", ev).await;
            skipped.push(row);
            continue;
        }
        app.emit(
            "bots_restart_progress",
            json!({"batch_id": batch_id, "index": index, "total": total,
                   "bot_id": bot_id, "name": name, "status": "restarting"}),
        )
        .await;
        let res = restart_resuming(app, &bot_id).await;
        match res {
            // recheck 之後、拿到 bot 鎖之前它忙起來了：跟上面一樣是跳過，不是失敗——不推 bot_restart_failed、
            // 也不做失敗收尾（它的 run 沒被動過）。
            Restarted::Busy(why) => {
                tracing::info!(bot = %name, reason = why.code(), "skipped under the bot lock: it got busy after the recheck");
                let (row, ev) = skip_now(why);
                app.emit("bots_restart_progress", ev).await;
                skipped.push(row);
            }
            Restarted::Ok(run_id) => {
                tracing::info!(bot = %name, run = %run_id, "restarted for the claude update (resumed)");
                ok.push(json!({"bot_id": bot_id, "name": name, "run_id": run_id}));
                app.emit(
                    "bots_restart_progress",
                    json!({"batch_id": batch_id, "index": index, "total": total,
                           "bot_id": bot_id, "name": name, "status": "ok"}),
                )
                .await;
            }
            Restarted::Failed(e) => {
                let msg = format!("{e:#}");
                tracing::warn!(bot = %name, error = %msg, "restart for the claude update failed");
                settle_failed_restart(app, &bot_id).await;
                // Own kind, not `health_changed`, so the supervisor can tell it from background noise.
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

/// 重啟的結果分三種：鎖內再判一次發現它忙起來了（34d24f0 的 409 `not_idle`）不是失敗，是跳過。
enum Restarted {
    Ok(String),
    Busy(Skip),
    Failed(anyhow::Error),
}

async fn restart_resuming(app: &Arc<App>, bot_id: &str) -> Restarted {
    // 子 agent 的 pane 是父 agent 開的：關掉再開等於搬家，所以原地重啟。
    let res = if db::bot(&app.db, bot_id).await.ok().flatten().is_some_and(|b| b.managed_by == "child") {
        lifecycle::restart_child_in_pane_with(app, bot_id, true).await
    } else {
        // One lock hold for both halves — closes the 2026-09-10 23:02 race (see `restart_bot_with`).
        lifecycle::restart_bot_with(app, bot_id, StartOpts { resume_native: true, require_idle: true, ..Default::default() }).await
    };
    match res {
        Ok(run_id) => Restarted::Ok(run_id),
        Err(e) => match busy_skip(&e) {
            Some(skip) => Restarted::Busy(skip),
            None => Restarted::Failed(why(e)),
        },
    }
}

/// 鎖內那一次閒置判斷擋下來的（409 `not_idle`），照 `busy` 對回跳過的理由。其他錯誤不是「忙」。
fn busy_skip(e: &LcError) -> Option<Skip> {
    let LcError::Conflict(v) = e else { return None };
    if v.get("reason").and_then(|x| x.as_str()) != Some("not_idle") {
        return None;
    }
    Some(match v.get("busy").and_then(|x| x.as_str()).unwrap_or_default() {
        "working" => Skip::Working,
        "blocked" => Skip::Blocked,
        "turn_in_flight" => Skip::TurnInFlight,
        "not_running" => Skip::NotRunning,
        _ => Skip::UnknownStatus,
    })
}

/// Read straight off the row: `get_or_init` would create a supervisor the user never asked for.
async fn supervisor_bot_id(app: &Arc<App>) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>("SELECT bot_id FROM supervisors LIMIT 1")
        .fetch_optional(&app.db)
        .await
        .ok()
        .flatten()
        .flatten()
        .filter(|s| !s.is_empty())
}

/// Supervisor goes last: it repairs the others, so it must be up while they restart.
pub fn supervisor_last(mut targets: Vec<(String, String)>, supervisor: Option<&str>) -> Vec<(String, String)> {
    if let Some(sid) = supervisor {
        if let Some(i) = targets.iter().position(|(id, _)| id == sid) {
            let t = targets.remove(i);
            targets.push(t);
        }
    }
    targets
}

/// A run left pointing at a closed pane would read as running and never be started again.
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

/// Retry once and record it in the inbox: the bot that would otherwise notice is this one.
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
        _ => lifecycle::start_bot_with(&app, &bot_id, StartOpts { resume_native: true, ..Default::default() }).await.map_err(why),
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

const SUPERVISOR_WINDOW: Duration = Duration::from_secs(60);
const SUPERVISOR_POLL: Duration = Duration::from_secs(5);

/// `LcError` 沒有 `Display`，批次失敗要給人看，攤成一句話。
fn why(e: LcError) -> anyhow::Error {
    let s = match e {
        LcError::NotFound(what) => format!("找不到 {what}"),
        LcError::Upstream(m) | LcError::Bad(m) => m,
        LcError::Conflict(v) | LcError::BadValue(v) | LcError::Unprocessable(v) | LcError::Forbidden(v) | LcError::Unavailable(v) => v
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
            default_session: false,
        }
    }

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

    /// herdr 報 idle 但 §4.3 備援還沒收掉回合。
    #[test]
    fn an_in_flight_turn_is_skipped_even_when_idle() {
        let cands = vec![cand("mid-turn", "claude", "running", "idle", true, true)];
        let (go, skip) = plan(&cands);
        assert!(go.is_empty());
        assert_eq!(skip[0].1, Skip::TurnInFlight);
    }

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

    /// 2026-09-12 使用者：三顆子 agent 全被跳過，更新永遠套不上去。
    #[test]
    fn children_join_the_batch() {
        let kid = Cand { managed_by: "child".into(), ..cand("kid", "claude", "running", "idle", true, false) };
        let mine = cand("mine", "claude", "running", "idle", true, false);
        let cands = [kid, mine];
        let (go, skip) = plan(&cands);
        assert_eq!(go.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["kid", "mine"]);
        assert!(skip.is_empty(), "{skip:?}");
    }

    /// SPEC §6.5.1：重啟會關掉使用者的終端（2026-09-12 review #4）。
    #[test]
    fn a_bot_in_the_users_default_session_is_skipped() {
        let mine = Cand { default_session: true, ..cand("mine", "claude", "running", "idle", true, false) };
        let (go, skip) = plan(std::slice::from_ref(&mine));
        assert!(go.is_empty());
        assert_eq!(skip[0].1, Skip::DefaultSession);
        assert_eq!(skip[0].1.code(), "default_session");
    }

    #[test]
    fn a_busy_child_is_still_skipped_for_being_busy() {
        let kid = Cand { managed_by: "child".into(), ..cand("kid", "claude", "running", "working", true, false) };
        let (go, skip) = plan(std::slice::from_ref(&kid));
        assert!(go.is_empty());
        assert_eq!(skip[0].1, Skip::Working);
    }

    #[test]
    fn the_supervisor_is_restarted_last() {
        let t = |ids: &[&str]| ids.iter().map(|i| (i.to_string(), format!("n-{i}"))).collect::<Vec<_>>();
        let order = |v: Vec<(String, String)>| v.into_iter().map(|(id, _)| id).collect::<Vec<_>>();
        assert_eq!(order(supervisor_last(t(&["a", "agm", "b", "c"]), Some("agm"))), ["a", "b", "c", "agm"]);
        assert_eq!(order(supervisor_last(t(&["a", "b"]), Some("agm"))), ["a", "b"]);
        assert_eq!(order(supervisor_last(t(&["agm", "a"]), None)), ["agm", "a"]);
    }

    #[tokio::test]
    async fn a_failed_restart_leaves_no_dead_run_and_tells_the_supervisor() {
        let env = crate::testing::env().await;
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
        let run = crate::testing::fake_run(&app, &bot).await;
        // 真的候選才會被排進批次：帶著更新通知、閒置（輪到它時會再看一次）。
        sqlx::query("UPDATE runs SET update_notice='Update installed · Restart to update' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();

        run_batch(&app, "batch-1", vec![(bot.clone(), "alfa".into())], vec![], None).await;

        assert!(db::active_run(&app.db, &bot).await.unwrap().is_none(), "no run is left behind for a bot that did not come back");
        let inbox = crate::supervisor::store::inbox(&app.db, 50).await.unwrap();
        let ev = inbox.iter().find(|e| e.kind == "bot_restart_failed").expect("the supervisor is told");
        assert_eq!(ev.bot_id.as_deref(), Some(bot.as_str()));
    }

    /// review 2026-09-16（上一輪 #5）：計畫之後、輪到它之前，AGM 派了工或使用者打了字，它就在回合中了。
    /// 以前 `run_batch` 照計畫硬重啟，把回合連同 in-flight turn 一起砍掉；現在輪到它時重看一次，改記成跳過。
    #[tokio::test]
    async fn a_bot_that_got_busy_after_the_plan_is_skipped_not_restarted() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "alfa").await;
        let run = crate::testing::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE runs SET update_notice='Update installed · Restart to update', agent_status='working' WHERE id=?")
            .bind(&run)
            .execute(&app.db)
            .await
            .unwrap();
        let mut events = app.subscribe();

        run_batch(&app, "batch-busy", vec![(bot.id.clone(), "alfa".into())], vec![], None).await;

        assert_eq!(db::active_run(&app.db, &bot.id).await.unwrap().map(|r| r.id), Some(run), "回合中的 bot 不能被重啟");
        let mut done = None;
        while let Ok(ev) = events.try_recv() {
            if ev.kind == "bots_restart_done" {
                done = Some(ev.data);
            }
        }
        let done = done.expect("done 一定會送");
        assert_eq!(done["ok"].as_array().unwrap().len(), 0);
        assert_eq!(done["skipped"][0]["reason"], "working", "{done}");
        // 更新已經套用過（沒有 update_notice）的也不重啟。
        assert_eq!(recheck(&app, &bot.id).await, Some(Skip::Working));
        sqlx::query("UPDATE runs SET update_notice=NULL, agent_status='idle' WHERE bot_id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        assert_eq!(recheck(&app, &bot.id).await, Some(Skip::NoLongerPending));
    }

    /// 34d24f0 讓 restart 在 bot 鎖內再判一次閒置、不閒置回 409 `not_idle`。那是「輪到它時忙起來了」，
    /// 要記成跳過（理由照 `busy`），不能落到 `failed`、推 `bot_restart_failed` 去叫醒人。
    #[test]
    fn a_not_idle_refusal_under_the_lock_is_a_skip_not_a_failure() {
        let conflict = |busy: &str| LcError::conflict("not_idle", json!({"bot_id": "b", "busy": busy}));
        assert_eq!(busy_skip(&conflict("working")), Some(Skip::Working));
        assert_eq!(busy_skip(&conflict("blocked")), Some(Skip::Blocked));
        assert_eq!(busy_skip(&conflict("turn_in_flight")), Some(Skip::TurnInFlight));
        assert_eq!(busy_skip(&conflict("not_running")), Some(Skip::NotRunning));
        assert_eq!(busy_skip(&conflict("???")), Some(Skip::UnknownStatus));
        // 其他 409 與其他錯誤照舊是失敗。
        assert_eq!(busy_skip(&LcError::conflict("default_session", json!({}))), None);
        assert_eq!(busy_skip(&LcError::Upstream("herdr down".into())), None);
    }

    /// 同時只准一批：第二次按下去拿到正在跑的那一批，不另開一份重疊的清單。
    #[tokio::test]
    async fn a_second_batch_while_one_is_running_joins_the_first() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let key = app.data_dir.display().to_string();
        running_batches().lock().unwrap().insert(key.clone(), "batch-first".into());
        let second = spawn(&app).await.unwrap();
        assert_eq!(second["batch_id"], "batch-first");
        assert_eq!(second["already_running"], true);
        assert_eq!(second["total"], 0);
        drop(BatchSlot(key.clone()));
        assert!(!running_batches().lock().unwrap().contains_key(&key), "批次結束就放掉");
        let third = spawn(&app).await.unwrap();
        assert_ne!(third["batch_id"], "batch-first");
        assert!(third.get("already_running").is_none());
        assert!(!running_batches().lock().unwrap().contains_key(&key), "空批次不佔著");
    }

    #[test]
    fn nothing_to_do_is_an_empty_plan() {
        let (go, skip) = plan(&[]);
        assert!(go.is_empty() && skip.is_empty());
    }
}
