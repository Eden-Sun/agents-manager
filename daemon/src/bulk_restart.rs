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
        }
    }
}

/// 非候選連「跳過」都不列，免得淹掉真正要看的那幾行。
pub fn is_candidate(c: &Cand) -> bool {
    c.kind == "claude" && c.has_update
}

/// 順序即優先序，回報理由取第一個命中的（使用者最該先處理的那件）。
/// 子 agent 也進來，走 [`crate::lifecycle::restart_child_in_pane`]（2026-09-12 使用者：三顆子 agent 全被跳過）。
pub fn plan(cands: &[Cand]) -> (Vec<&Cand>, Vec<(&Cand, Skip)>) {
    let mut go = Vec::new();
    let mut skip = Vec::new();
    for c in cands.iter().filter(|c| is_candidate(c)) {
        let why = if c.default_session {
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
        };
        match why {
            Some(w) => skip.push((c, w)),
            None => go.push(c),
        }
    }
    (go, skip)
}

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
            default_session: lifecycle::in_default_session(&run) || bot.herdr_session.as_deref() == Some("default"),
        });
    }
    Ok(out)
}

fn skip_json(c: &Cand, w: Skip) -> serde_json::Value {
    json!({"bot_id": c.bot_id, "name": c.name, "reason": w.code(), "reason_label": w.label()})
}

/// 只回計畫、背景執行：一顆 `stop_bot` 最久等十秒，同步做會拖爆 HTTP；進度走 WS。
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

async fn restart_resuming(app: &Arc<App>, bot_id: &str) -> anyhow::Result<String> {
    // 子 agent 的 pane 是父 agent 開的：關掉再開等於搬家，所以原地重啟。
    if db::bot(&app.db, bot_id).await.ok().flatten().is_some_and(|b| b.managed_by == "child") {
        return lifecycle::restart_child_in_pane(app, bot_id).await.map_err(why);
    }
    // One lock hold for both halves — closes the 2026-09-10 23:02 race (see `restart_bot_with`).
    lifecycle::restart_bot_with(app, bot_id, StartOpts { resume_native: true }).await.map_err(why)
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

const SUPERVISOR_WINDOW: Duration = Duration::from_secs(60);
const SUPERVISOR_POLL: Duration = Duration::from_secs(5);

/// `LcError` 沒有 `Display`，批次失敗要給人看，攤成一句話。
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
        crate::testing::fake_run(&app, &bot).await;

        run_batch(&app, "batch-1", vec![(bot.clone(), "alfa".into())], vec![], None).await;

        assert!(db::active_run(&app.db, &bot).await.unwrap().is_none(), "no run is left behind for a bot that did not come back");
        let inbox = crate::supervisor::store::inbox(&app.db, 50).await.unwrap();
        let ev = inbox.iter().find(|e| e.kind == "bot_restart_failed").expect("the supervisor is told");
        assert_eq!(ev.bot_id.as_deref(), Some(bot.as_str()));
    }

    #[test]
    fn nothing_to_do_is_an_empty_plan() {
        let (go, skip) = plan(&[]);
        assert!(go.is_empty() && skip.is_empty());
    }
}
