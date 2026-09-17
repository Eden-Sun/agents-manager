//! AGM — the persistent supervisor layer (docs/goals/agm-supervisor-environment-plan-2026-09-09.md).
//!
//! The manager is an ordinary claude bot with a persona; everything that must survive a
//! restart, a model switch or a lost session is here, in the daemon, not in its context.
//! The model understands and decides. The daemon persists, retries, watches and switches.

pub mod api;
pub mod assignment_state;
pub mod bot_requests;
pub mod cli_refresh;
pub mod controller;
pub mod digest_text;
pub mod health;
pub mod incidents;
pub mod maintenance;
pub mod persona;
pub mod policy;
pub mod remote;
pub mod responder;
pub mod responder_api;
pub mod roles;
pub mod setup;
pub mod store;
pub mod watchdog;

use crate::lifecycle::LcError;
use crate::state::App;
use serde_json::{json, Value};
use std::sync::Arc;
use std::sync::OnceLock;
use tokio::sync::Mutex;

/// One lock for every state-changing supervisor operation: setup, start, stop, fallback and
/// the dispatch half of an assignment. They all read-modify-write the same one row and the
/// same one bot, and two of them at once is how you get a second AGM or a double send.
fn op_lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

pub async fn lock() -> tokio::sync::MutexGuard<'static, ()> {
    op_lock().lock().await
}

const BOOTSTRAP_REQUEST_ID: &str = "agm-bootstrap-v1";
const BOOTSTRAP_PROMPT: &str = r#"這是 AGM 啟動握手，不是新的工作委派。

請先依你的恢復流程讀取 handoff.md、bin/agm assignments、bin/agm inbox，再用 bin/agm state 查即時狀態。確認你是 agents-manager 的總管，能協助使用者找回適合的既有 bot、提出有證據的分配建議，並在使用者明確交辦後透過 bin/agm 建立與追蹤 assignment。

請用繁體中文回覆一段簡短的「AGM 已就緒」訊息，說明使用者可以直接提出問題、要求尋找相關 bot，或說「交給你推薦的 bot」。不要自行建立工作，也不要把這段握手當成待辦。"#;

/// 使用者按下 start：**先把「要它跑」寫進去，寫成功才啟動**。呼叫端持有 [`lock`]。
///
/// `desired_running` 是看門狗唯一的憑據，而且要跨重啟活著，所以它的寫入是這條路徑的前置條件，
/// 不是順手做的副作用（issue #84）。寫不進去就整個失敗、一個 pane 都不開：呼叫端拿到的錯誤是真的，
/// 而且什麼都還沒動，原樣重試是安全的。反過來（先啟動再寫）失敗時會留下「跑著但沒人要它跑」——
/// 看門狗不管、健康說 healthy，重啟之後就不會照使用者期待回來。
///
/// 啟動本身失敗時意圖照樣留著：交給看門狗的有界重試，健康那格也會因為「要它跑卻停著」變成 degraded
/// （999480e 對協調者的決定，這裡跟它一致）。
pub async fn start_requested(app: &Arc<App>) -> Result<(), LcError> {
    // Choose the configured fallback before launching the CLI, so a low Fable bucket starts
    // directly on Opus instead of briefly opening the wrong session.
    if let Err(e) = controller::apply_quota_policy(app).await {
        tracing::warn!(error = ?e, "quota policy during start failed");
    }
    // `manager_bot` 建好那一列（`get_or_init`）之後才寫得進去，而且沒設定好就該回 not_configured，
    // 不是留下一個沒有 bot 的「要它跑」。
    if manager_bot(app).await?.is_none() {
        return Err(LcError::conflict("supervisor is not set up", json!({"reason": "not_configured"})));
    }
    store::set_desired_running(&app.db, true).await.map_err(persist_intent_failed)?;
    start_manager(app, None).await
}

/// 使用者按下 stop：**先把「不要它跑」寫進去，寫成功才停**。呼叫端持有 [`lock`]。
///
/// 順序本來就對，但寫入失敗被吞掉，於是「停好了」會回 200，而看門狗讀到的還是「要它跑」，
/// 下一個 tick 就把它拉回來——使用者看到的是自己停過的東西自己活過來（issue #84）。
pub async fn stop_requested(app: &Arc<App>) -> Result<(), LcError> {
    let bot = manager_bot(app)
        .await?
        .ok_or_else(|| LcError::conflict("supervisor is not set up", json!({"reason": "not_configured"})))?;
    store::set_desired_running(&app.db, false).await.map_err(persist_intent_failed)?;
    crate::lifecycle::stop_bot(app, &bot.id).await?;
    // A stopped CLI takes its Remote Control session with it; claiming otherwise would send
    // the user to a dead URL on their phone.
    let _ = store::set_remote(&app.db, "unknown", None).await;
    app.emit("supervisor_changed", json!({"stopped": true})).await;
    Ok(())
}

/// 意圖寫不進去：講清楚是哪一步壞了，呼叫端才知道「什麼都沒發生、可以原樣重試」。
fn persist_intent_failed<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(format!("could not persist desired_running (nothing was started or stopped): {e}"))
}

/// Bring the manager up. The one start path, shared by [`start_requested`] and the
/// watchdog; the caller holds [`lock`]. `detail` is what `status_detail` should say afterwards
/// (the watchdog writes why it did this; the API clears it).
///
/// 這裡**不動** `desired_running`：看門狗與換模型重啟都走這條，寫意圖會把看門狗的重試次數歸零
/// （`set_desired_running` 的語意是「人做了新決定」），有界重試就變成永遠重試。
pub async fn start_manager(app: &Arc<App>, detail: Option<&str>) -> Result<(), LcError> {
    let bot = manager_bot(app)
        .await?
        .ok_or_else(|| LcError::conflict("supervisor is not set up", json!({"reason": "not_configured"})))?;
    if crate::db::active_run(&app.db, &bot.id).await.map_err(up)?.is_none() {
        crate::lifecycle::start_bot(app, &bot.id).await?;
    }
    // A newly spawned CLI is intentionally idle until it receives a first turn. Send one
    // idempotent handshake so the user can immediately see how to use AGM; the stable request
    // id prevents a restart from creating another greeting turn.
    // The handshake is the daemon's, not the user's: marked as such so AGM's own conversation
    // keeps showing which lines a person actually typed.
    // 控制面自己的送入：維護窗口不擋它，否則「把 AGM 重新起來」這件事會被自己開的窗口鎖在門外
    // （issue #86）。
    if let Err(e) = crate::lifecycle::prompt_control_plane(
        app,
        &bot.id,
        BOOTSTRAP_PROMPT,
        BOOTSTRAP_REQUEST_ID,
        Some(crate::agent_relay::DAEMON_SENDER),
    )
    .await
    {
        tracing::warn!(error = ?e, "AGM bootstrap prompt was not delivered");
    }
    // The bot's args carry `--remote-control AGM`, which is a *request*. Whether a remote
    // session actually came up is something only an observation can say, so the status stays
    // `requested` until something verifies it — and it is bound to *this* run, so a later
    // restart cannot inherit the claim (see `remote.rs`).
    let session = crate::db::active_run(&app.db, &bot.id).await.map_err(up)?.map(|r| r.id);
    let _ = store::set_remote_observed(&app.db, "requested", None, "argv", session.as_deref(), None, None).await;
    let _ = store::set_status(&app.db, "", detail).await;
    let sup = store::get_or_init(&app.db).await.map_err(up)?;
    let gen = if sup.generation == 0 {
        store::bump_generation(&app.db).await.map_err(up)?
    } else {
        sup.generation
    };
    controller::spawn(app.clone(), gen);
    controller::reconcile(app).await;
    app.emit("supervisor_changed", json!({"started": true})).await;
    Ok(())
}

fn up<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(e.to_string())
}

/// The manager's bot, if it is configured *and* still present (a user can delete it).
pub async fn manager_bot(app: &Arc<App>) -> Result<Option<crate::db::Bot>, LcError> {
    let sup = store::get_or_init(&app.db).await.map_err(up)?;
    let Some(id) = sup.bot_id else { return Ok(None) };
    Ok(crate::db::bot(&app.db, &id).await.map_err(up)?.filter(|b| b.deleted_at.is_none()))
}

/// `idle` | `busy`: whether a prompt would be accepted right now. Anything else is not a
/// state we may send work into.
pub async fn manager_liveness(app: &Arc<App>, bot_id: &str) -> Result<&'static str, LcError> {
    let Some(run) = crate::db::active_run(&app.db, bot_id).await.map_err(up)? else { return Ok("stopped") };
    if run.state == "starting" {
        return Ok("starting");
    }
    if run.state != "running" {
        return Ok("stopped");
    }
    if run.agent_status == "working" || run.agent_status == "blocked" {
        return Ok("busy");
    }
    if crate::db::in_flight_turn(&app.db, &run.id).await.map_err(up)?.is_some() {
        return Ok("busy");
    }
    Ok("idle")
}

/// The `GET /api/supervisor` payload — the one shape the web UI and the `agm` CLI both read.
pub async fn status_json(app: &Arc<App>) -> Result<Value, LcError> {
    let sup = store::get_or_init(&app.db).await.map_err(up)?;
    let bot = manager_bot(app).await?;
    let configured = bot.is_some();
    let status = if !configured {
        "not_configured".to_string()
    } else if !sup.status.is_empty() {
        // A sticky `waiting_quota` / `failed` outranks the run: the process may well be up
        // and simply unable to answer.
        sup.status.clone()
    } else {
        manager_liveness(app, bot.as_ref().map(|b| b.id.as_str()).unwrap_or_default()).await?.to_string()
    };
    let assignments = store::list_assignments(&app.db, 50).await.map_err(up)?;
    Ok(json!({
        "configured": configured,
        "bot_id": bot.as_ref().map(|b| b.id.clone()),
        "project_id": bot.as_ref().map(|b| b.project_id.clone()),
        "model": sup.active_model,
        "model_arg": setup::model_arg(&sup.active_model),
        "identity": sup.identity,
        "effort": sup.effort,
        "status": status,
        "status_detail": sup.status_detail,
        "generation": sup.generation,
        "cwd": sup.cwd,
        "quota_reset_at": sup.quota_reset_at,
        // Computed, not just read back: an observation that has expired or belongs to a session
        // that is gone reads as `unknown` here rather than as the word it was stored under.
        "remote": remote::status(app).await,
        "pending_count": store::pending_count(&app.db).await.map_err(up)?,
        "assignments": assignments.iter().map(store::Assignment::to_json).collect::<Vec<_>>(),
        // 雙角色（SPEC §18.15）。上面那些欄位一直是巡檢的，舊的呼叫端照讀不受影響。
        "role": roles::Role::Patrol.as_str(),
        "remote_provider": roles::Role::Patrol.as_str(),
        "stats": roles::get(&app.db, roles::Role::Patrol).await.map_err(up)?.stats_json(),
        "responder": responder::status_json(app).await?,
        // 上次成功上線：現在跑的這顆 binary 的 commit 與它起來的時間。前端用它排掉上線以前的
        // 舊申請，AGM 用它判斷「origin/main 動了」跟「已經上線了」是不是同一件事。
        "last_deploy": crate::build_info::last_deploy(),
    }))
}

/// Hand a piece of work to a worker bot.
///
/// Order matters and is the whole point: the assignment row is committed *first*, then the
/// prompt goes out under that row's `client_request_id`. A crash in between leaves a `queued`
/// row the controller retries with the same id, so the worker never gets the job twice.
/// `expects_review = false` 是**通知**：AGM 只是要把話說給 bot 聽（「收到」「進 idle」
/// 「看完即可」），送到、回合結束就結案，不進 awaiting_review、不會被當成卡住的工作。
#[allow(clippy::too_many_arguments)]
pub async fn assign(
    app: &Arc<App>,
    target_bot_id: &str,
    text: &str,
    client_request_id: &str,
    source_turn_id: Option<&str>,
    ownership: &[String],
    follow_up_of: Option<&str>,
    expects_review: bool,
    // 群組任務：(mission_id, role)。呼叫端已驗證過。
    mission: Option<(&str, &str)>,
    // 回報給哪個 AGM 角色驗收；`None` = 協調者（roles.rs 的預設）。
    review_role: Option<roles::Role>,
    // 呼叫端自己是哪個角色（bot token 驗過的）。`None` = UI／腳本。
    actor: Option<roles::Role>,
    // 角色之間的交接明講是回覆（`--ack`／`--reply-to`）；只對交接有意義。
    reply: bot_requests::ReplyMark<'_>,
) -> Result<Value, LcError> {
    if client_request_id.trim().is_empty() {
        return Err(LcError::Bad("client_request_id must not be empty".into()));
    }
    if text.trim().is_empty() {
        return Err(LcError::Bad("text must not be empty".into()));
    }
    let _g = lock().await;

    // A retry of the same request is the same assignment, never a second one.
    if let Some(a) = store::assignment_by_crid(&app.db, client_request_id).await.map_err(up)? {
        if a.target_bot_id != target_bot_id {
            return Err(LcError::conflict(
                "client_request_id already used for another bot",
                json!({"assignment_id": a.id, "target_bot_id": a.target_bot_id}),
            ));
        }
        // Same id, different words: the model would read a 200 here as "my new instruction
        // went out", and it did not. Only a byte-identical retry is idempotent.
        if a.text != text {
            return Err(LcError::conflict(
                "client_request_id already used with different text",
                json!({"assignment_id": a.id, "reason": "text_mismatch"}),
            ));
        }
        return Ok(a.to_json());
    }

    let sup = store::get_or_init(&app.db).await.map_err(up)?;
    let manager_id = sup.bot_id.clone().ok_or_else(|| {
        LcError::conflict("supervisor is not set up", json!({"reason": "not_configured"}))
    })?;
    // 目標是另一個 AGM 角色：這不是交辦，是**交接**。走跟 bot 申請同一條佇列（批次、節流、
    // 「回覆不再叫醒對方」的規則只有一份），不直接打進對方的 pane（SPEC §18.15）。
    if let Some(target_role) = roles::role_of_bot(&app.db, target_bot_id).await.map_err(up)? {
        if actor == Some(target_role) || target_bot_id == manager_id && actor.is_none() && target_role == roles::Role::Patrol {
            return Err(LcError::Bad("the supervisor cannot assign work to itself".into()));
        }
        let from = match actor {
            Some(r) => roles::bot_for(&app.db, r).await.map_err(up)?,
            None => Some(manager_id.clone()),
        };
        let Some(from) = from else {
            return Err(LcError::conflict("that AGM role has no bot", json!({"reason": "role_not_configured"})));
        };
        let mut out = bot_requests::queue(
            app,
            target_role,
            &from,
            target_bot_id,
            text,
            Some(client_request_id),
            &[],
            actor.is_some(),
            "assignment",
            reply,
        )
        .await?;
        out["kind"] = json!("handover");
        out["expects_review"] = json!(expects_review);
        return Ok(out);
    }
    // 「這是回覆」只有交接走的佇列看得懂；對一般 bot 下的交辦帶它，呼叫端會以為對方不會被打擾。
    if reply.is_set() {
        return Err(LcError::Bad("ack / reply_to only apply to a handover between AGM roles".into()));
    }
    crate::db::bot(&app.db, target_bot_id)
        .await
        .map_err(up)?
        .filter(|b| b.deleted_at.is_none())
        .ok_or_else(|| LcError::NotFound("bot".into()))?;

    // The user's own words behind this assignment, with a stable id to dedupe on. A text hash
    // would not be one — two identical asks are two asks.
    let (source, source_key, request_text) = source_of(app, &manager_id, source_turn_id, text, actor).await?;
    let request_id = store::upsert_request(&app.db, source, source_key.as_deref(), &request_text)
        .await
        .map_err(up)?;

    // 一個任務同時只有一件開著的交辦（SPEC §18.14，issue #74）。跟下面的 ownership 衝突不同：
    // 那個是猜的（字串比對），這個是查得到的事實，所以這個擋、那個只回報。
    if let Some((mission_id, role)) = mission {
        crate::mission::workflow::ensure_can_assign(app, mission_id, role).await?;
    }

    // Who else is already holding these files. Reported, never enforced: the daemon cannot
    // know that two modules are really independent, so this goes to AGM to arbitrate rather
    // than refusing work on a string match (SPEC §18.4).
    let conflicts = ownership_conflicts(app, ownership, None).await?;

    let a = store::insert_assignment(
        &app.db,
        Some(&request_id),
        target_bot_id,
        client_request_id,
        text,
        ownership,
        follow_up_of,
        expects_review,
    )
    .await
    .map_err(up)?;
    if let Some(parent) = follow_up_of {
        let _ = store::link_followup(&app.db, parent, &a.id).await;
    }
    // 派送之前就要掛上任務：派送當下撞到額度時，controller 要看得到這件屬於哪個任務、什麼角色。
    if let Some((mission_id, role)) = mission {
        store::set_mission_link(&app.db, &a.id, mission_id, role).await.map_err(up)?;
    }
    // 同樣要在派送之前：派工訊息標成哪個角色送的，看的就是這一欄。
    if let Some(r) = review_role {
        store::set_review_role(&app.db, &a.id, r.as_str()).await.map_err(up)?;
    }
    // Best effort: a failure here leaves the row queued, which is the recoverable state.
    controller::dispatch(app, &a.id).await;
    let a = store::assignment(&app.db, &a.id).await.map_err(up)?.unwrap_or(a);
    app.emit("supervisor_changed", json!({"assignment_id": a.id, "status": a.status})).await;
    let mut out = a.to_json();
    out["ownership_conflicts"] = json!(conflicts);
    Ok(out)
}

/// Open assignments whose declared ownership overlaps `paths`.
///
/// A plain prefix match on the declared strings: `daemon/src/supervisor` overlaps
/// `daemon/src/supervisor/store.rs` and itself, and nothing else. Cheap, explainable, and wrong
/// only in the safe direction — it can report an overlap that is not one, which costs AGM a
/// glance; it cannot quietly hand two bots the same file.
pub async fn ownership_conflicts(
    app: &Arc<App>,
    paths: &[String],
    ignore_assignment: Option<&str>,
) -> Result<Vec<Value>, LcError> {
    if paths.is_empty() {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    for other in store::unsettled_assignments(&app.db).await.map_err(up)? {
        if Some(other.id.as_str()) == ignore_assignment {
            continue;
        }
        let held = other.ownership();
        let overlap: Vec<String> = paths
            .iter()
            .filter(|p| held.iter().any(|h| overlaps(p, h)))
            .cloned()
            .collect();
        if !overlap.is_empty() {
            out.push(json!({
                "assignment_id": other.id,
                "target_bot_id": other.target_bot_id,
                "status": other.status,
                "paths": overlap,
            }));
        }
    }
    Ok(out)
}

fn overlaps(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim_end_matches('/'), b.trim_end_matches('/'));
    a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
}

/// Can this bot be handed work at all? A read-only check, so a caller can validate a target
/// before opening a transaction (and get a plain 400/409 instead of a rolled-back write).
///
/// Shares its rules with [`assign`]: the manager may not assign to itself, the bot has to exist
/// and not be deleted.
pub async fn check_assignable(app: &Arc<App>, target_bot_id: &str) -> Result<(), LcError> {
    // 角色 bot 不是工人：要跟另一個角色說話走 `assign` 的交接路徑（佇列），不是 followup。
    if roles::role_of_bot(&app.db, target_bot_id).await.map_err(up)?.is_some() {
        return Err(LcError::Bad("an AGM role cannot be the target of an assignment; hand over through the role queue".into()));
    }
    crate::db::bot(&app.db, target_bot_id)
        .await
        .map_err(up)?
        .filter(|b| b.deleted_at.is_none())
        .ok_or_else(|| LcError::NotFound("bot".into()))?;
    Ok(())
}

/// Where an assignment came from: `(source, source_key, text)`.
///
/// The phone talks to the manager over Remote Control, which is a native session — its user
/// echo reaches the daemon through the hook / transcript path, and that path is **not**
/// verified to carry every word. So: a caller-supplied `source_turn_id` must belong to the
/// manager (a worker must not be able to name someone else's turn as its authority); with no
/// id we fall back to whatever turn the manager is on right now; and if no user text can be
/// read back off that turn, the source is labelled `assignment_text_fallback` rather than
/// pretending the assignment text is the user's own words.
async fn source_of(
    app: &Arc<App>,
    manager_id: &str,
    source_turn_id: Option<&str>,
    text: &str,
    actor: Option<roles::Role>,
) -> Result<(&'static str, Option<String>, String), LcError> {
    // 來源綁**呼叫者自己**（bot token 驗過的角色）。兩個角色同時在回合中時，若照固定順序先撿巡檢
    // 的回合，協調者派的工就會被記成「使用者對巡檢說的另一句話」——授權與稽核從此對不上人。
    let actor_bot = match actor {
        Some(r) => roles::bot_for(&app.db, r).await.map_err(up)?,
        None => None,
    };
    let conv_of = |id: String| async move { crate::db::conversation_id(&app.db, &id).await.map_err(up) };
    let turn_id = match source_turn_id.map(str::trim).filter(|s| !s.is_empty()) {
        // 明講了是哪個回合：那是呼叫端拿得出來的證據。驗過的角色只能指自己的回合；沒驗過的
        // 呼叫端（UI、腳本）仍只能指兩個角色其中之一的回合。
        Some(t) => {
            let mut convs = Vec::new();
            match actor_bot.clone() {
                Some(id) => convs.push(conv_of(id).await?),
                None => {
                    convs.push(crate::db::conversation_id(&app.db, manager_id).await.map_err(up)?);
                    if let Some(id) = roles::bot_for(&app.db, roles::Role::Responder).await.map_err(up)? {
                        convs.push(crate::db::conversation_id(&app.db, &id).await.map_err(up)?);
                    }
                }
            }
            let sql = format!(
                "SELECT id FROM turns WHERE id=? AND conversation_id IN ({})",
                convs.iter().map(|_| "?").collect::<Vec<_>>().join(",")
            );
            let mut q = sqlx::query_scalar::<_, String>(&sql).bind(t);
            for c in &convs {
                q = q.bind(c);
            }
            let owned: Option<String> = q.fetch_optional(&app.db).await.map_err(up)?;
            if owned.is_none() {
                let whose = actor.map(roles::Role::as_str).unwrap_or("the supervisor");
                return Err(LcError::Bad(format!("source_turn_id is not a turn of {whose}")));
            }
            Some(t.to_string())
        }
        // 沒講：只認呼叫者自己現在跑的那一回合。認不出呼叫者就**不猜**別人的回合——
        // 記成 `assignment_text_fallback`（交辦文字就是交辦文字），不要替某個人編一句話。
        None => match actor_bot {
            Some(id) => match crate::db::active_run(&app.db, &id).await.map_err(up)? {
                Some(r) => crate::db::in_flight_turn(&app.db, &r.id).await.map_err(up)?.map(|t| t.id),
                None => None,
            },
            None => None,
        },
    };
    let Some(turn_id) = turn_id else {
        return Ok(("assignment_text_fallback", None, text.to_string()));
    };
    let user_text: Option<String> = sqlx::query_scalar(
        "SELECT content FROM messages WHERE turn_id=? AND role='user' ORDER BY created_at ASC LIMIT 1",
    )
    .bind(&turn_id)
    .fetch_optional(&app.db)
    .await
    .map_err(up)?;
    match user_text.filter(|s| !s.trim().is_empty()) {
        Some(t) => Ok(("manager_turn", Some(turn_id), t)),
        // The turn is real, so it is still the right dedupe key; we just do not have the
        // user's wording, and the record says so.
        None => Ok(("assignment_text_fallback", Some(turn_id), text.to_string())),
    }
}

/// `GET /api/supervisor/state` — what the manager is allowed to see about the world.
///
/// Sanitized on purpose: no env, no hook tokens, no args (an identity's args can name config
/// directories), no persona text. Enough to pick a bot; nothing that helps exfiltrate an account.
pub async fn sanitized_state(app: &Arc<App>) -> Result<Value, LcError> {
    let projects = crate::db::live_projects(&app.db).await.map_err(up)?;
    let bots = crate::db::live_bots(&app.db).await.map_err(up)?;
    let sup = store::get_or_init(&app.db).await.map_err(up)?;
    let responder_id = roles::get(&app.db, roles::Role::Responder).await.map_err(up)?.bot_id;
    let hosts: std::collections::HashMap<String, String> =
        projects.iter().map(|p| (p.id.clone(), p.host.clone())).collect();
    let mut connected: std::collections::HashSet<String> = Default::default();
    for h in hosts.values() {
        if app.host_connected(h).await {
            connected.insert(h.clone());
        }
    }
    let mut out = Vec::new();
    for b in &bots {
        let run = crate::db::active_run(&app.db, &b.id).await.map_err(up)?;
        // Work already waiting on this bot: a bot with a queue is available, but not free.
        let queued: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM turns WHERE conversation_id=(SELECT id FROM conversations WHERE bot_id=?)
               AND status='queued'",
        )
        .bind(&b.id)
        .fetch_one(&app.db)
        .await
        .unwrap_or(0);
        out.push(json!({
            "id": b.id,
            "project_id": b.project_id,
            "name": b.name,
            "kind": b.kind,
            "model": b.model,
            "effort": b.effort,
            "identity": b.identity,
            "managed_by": b.managed_by,
            "parent_bot_id": b.parent_bot_id,
            "is_supervisor": Some(&b.id) == sup.bot_id.as_ref() || Some(&b.id) == responder_id.as_ref(),
            "supervisor_role": if Some(&b.id) == sup.bot_id.as_ref() { Some("patrol") }
                else if Some(&b.id) == responder_id.as_ref() { Some("responder") } else { None },
            "cwd": b.cwd,
            "host": hosts.get(&b.project_id).cloned(),
            "host_connected": hosts.get(&b.project_id).map(|h| connected.contains(h)),
            "queued_turns": queued,
            "run": run.as_ref().map(|r| json!({
                "id": r.id,
                "state": r.state,
                // The lamp the sidebar shows: idle / working / blocked / unknown.
                "agent_status": r.agent_status,
                "agent_title": r.agent_title,
                // Whether the original conversation can be resumed at all, and on what the
                // CLI is *actually* running — which is not always what the bot is configured with.
                "native_session_id": r.native_session_id,
                "runtime_model": r.runtime_model,
                "runtime_effort": r.runtime_effort,
                "pane_id": r.pane_id,
                "started_at": r.started_at,
            })),
        }));
    }
    Ok(json!({
        "supervisor_id": store::SUPERVISOR_ID,
        "projects": projects.iter().map(|p| json!({
            "id": p.id, "label": p.label, "path": p.path, "host": p.host,
        })).collect::<Vec<_>>(),
        "bots": out,
        // Everything still owed, `awaiting_review` included: the point of the acceptance state
        // is that a finished turn stays in front of the manager until it decides.
        "open_assignments": store::unsettled_assignments(&app.db).await.map_err(up)?
            .iter().map(store::Assignment::to_json).collect::<Vec<_>>(),
        "pending_inbox": store::pending_inbox(&app.db).await.map_err(up)?
            .iter().map(store::InboxEvent::to_json).collect::<Vec<_>>(),
        "open_incidents": store::open_incidents(&app.db).await.map_err(up)?
            .iter().map(store::Incident::to_json).collect::<Vec<_>>(),
    }))
}
