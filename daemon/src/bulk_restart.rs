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
    SpawnedChild,
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
            Skip::SpawnedChild => "spawned_child",
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
            Skip::SpawnedChild => "是別的 agent 開的子 agent，由它的父 agent 管",
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
/// `child` / `team` 一律不碰：子 agent 是父 agent 開的 pane（`start_bot` 本來就會拒絕），
/// team 成員的生死歸 team 排程管——批次重啟插手只會讓排程對不上自己記得的 run。
pub fn plan(cands: &[Cand]) -> (Vec<&Cand>, Vec<(&Cand, Skip)>) {
    let mut go = Vec::new();
    let mut skip = Vec::new();
    for c in cands.iter().filter(|c| is_candidate(c)) {
        let why = if c.managed_by == "child" {
            Some(Skip::SpawnedChild)
        } else if c.managed_by == "team" {
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
    let planned: Vec<serde_json::Value> = go.iter().map(|c| json!({"bot_id": c.bot_id, "name": c.name})).collect();
    let skipped_json: Vec<serde_json::Value> = skipped.iter().map(|(c, w)| skip_json(c, *w)).collect();
    let targets: Vec<(String, String)> = go.iter().map(|c| (c.bot_id.clone(), c.name.clone())).collect();
    let total = targets.len();

    if total > 0 {
        let app2 = app.clone();
        let bid = batch_id.clone();
        let skipped_for_task = skipped_json.clone();
        tokio::spawn(async move { run_batch(&app2, &bid, targets, skipped_for_task).await });
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
    lifecycle::stop_bot(app, bot_id).await.map_err(why)?;
    lifecycle::start_bot_with(app, bot_id, StartOpts { resume_native: true }).await.map_err(why)
}

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

    /// 子 agent 與 team 成員不歸這顆按鈕管——前者 `start_bot` 本來就會拒絕（放進去只會變成
    /// 一則看不懂的失敗），後者的 run 由 team 排程記著。
    #[test]
    fn children_and_team_members_are_skipped() {
        let kid = Cand { managed_by: "child".into(), ..cand("kid", "claude", "running", "idle", true, false) };
        let member = Cand { managed_by: "team".into(), ..cand("dev-1", "claude", "running", "idle", true, false) };
        let mine = cand("mine", "claude", "running", "idle", true, false);
        let cands = [kid, member, mine];
        let (go, skip) = plan(&cands);
        assert_eq!(go.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["mine"]);
        assert_eq!(
            skip.iter().map(|(_, w)| w.code()).collect::<Vec<_>>(),
            ["spawned_child", "team_member"]
        );
    }

    /// 什麼都沒有時是空計畫，不是錯誤。
    #[test]
    fn nothing_to_do_is_an_empty_plan() {
        let (go, skip) = plan(&[]);
        assert!(go.is_empty() && skip.is_empty());
    }
}
