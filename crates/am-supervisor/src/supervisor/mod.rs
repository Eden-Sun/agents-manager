//! AGM — the persistent supervisor layer (docs/goals/agm-supervisor-environment-plan-2026-09-09.md).
//!
//! The manager is an ordinary claude bot with a persona; everything that must survive a
//! restart, a model switch or a lost session is here, in the daemon, not in its context.
//! The model understands and decides. The daemon persists, retries, watches and switches.

/// #1862 D：crate-only 測試用的窄環境（取代 daemon `App`／`testing`）。
#[cfg(all(test, not(feature = "daemon-test-harness")))]
pub(crate) mod test_env;

pub mod api;
pub mod assignment_state;
pub mod bot_requests;
pub mod cli_refresh;
pub mod controller;
pub mod digest_text;
pub mod failover;
pub mod health;
pub mod idle_sleep;
pub mod incidents;
pub mod maintenance;
pub mod persona;
pub mod policy;
pub mod ports;
pub mod responder;
pub mod responder_api;
pub mod role_faults;
pub mod roles;
pub mod setup;
pub mod store;
pub mod timing;
pub mod watchdog;

/// 這一批通知的 `client_request_id`。同一組 id 與各自的 `notify_attempts` 重送要同一個 id
/// （當機重送不該再打一次）；換了成員或某筆的 attempts 就必須是另一個 id。
/// 只拿「最後一筆 id + 最大 attempts」會跟上一批撞號，冪等檢查回 409 `text_mismatch`（#642）。
pub fn inbox_notify_crid(prefix: &str, batch: &[(&str, i64)]) -> String {
    let mut rows: Vec<(&str, i64)> = batch.to_vec();
    rows.sort_by(|a, b| a.0.cmp(b.0).then(a.1.cmp(&b.1)));
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    for (id, attempts) in rows {
        hasher.update(id.as_bytes());
        hasher.update([0xff]);
        hasher.update(attempts.to_le_bytes());
    }
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("{prefix}{hex}")
}

/// Generate an opaque 32-character hexadecimal token.
pub fn new_token() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..32).map(|_| std::char::from_digit(rng.gen_range(0..16), 16).unwrap()).collect()
}

#[cfg(all(test, not(feature = "daemon-test-harness")))]
mod notify_crid_tests {
    use super::inbox_notify_crid;

    /// #642：上一批 [e1 attempts=1, e2 attempts=0] 與下一批只剩 [e2 attempts=1] 的「最後一筆＋最大 attempts」相同，crid 不能相同。
    #[test]
    fn a_later_batch_that_shares_the_last_id_and_max_attempts_gets_its_own_crid() {
        let first = inbox_notify_crid("agm-inbox-", &[("e1", 1), ("e2", 0)]);
        let second = inbox_notify_crid("agm-inbox-", &[("e2", 1)]);
        assert_ne!(first, second, "撞號會讓下一批吃到 409 text_mismatch");
        assert_eq!(inbox_notify_crid("agm-inbox-", &[("e2", 0), ("e1", 1)]), first, "同一批重送要同一個 id");
        assert_ne!(inbox_notify_crid("agm-responder-inbox-", &[("e2", 1)]), second);
    }
}

use crate::lifecycle::LcError;
use sqlx::SqlitePool;
use self::ports::{BotLamp, CandidateSwitch, ControllerRuntime, JudgeOps, QuotaOps, TurnOps};
use serde_json::{json, Value};
use std::sync::OnceLock;
use tokio::sync::Mutex;

/// One lock for every state-changing supervisor operation: setup, start, stop, fallback and
/// the dispatch half of an assignment. They all read-modify-write the same one row and the
/// same one bot, and two of them at once is how you get a second AGM or a double send.
fn op_lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

/// 等到鎖、拿到鎖，順便記下「等多久」與「握多久」（issue #473 的量測，只記錄不改行為）。
///
/// 刻意**不是** `async fn`：`#[track_caller]` 在 `async fn` 上抓不到呼叫端（`Location::caller()` 會
/// 變成 poll 它的人）。寫成「同步函式回一個 future」，位置就在呼叫的那一刻抓好——27 個拿鎖點
/// 一行都不用改，統計裡卻分得出是哪一個。
#[track_caller]
pub fn lock() -> impl std::future::Future<Output = OpGuard> {
    let at = std::panic::Location::caller();
    async move {
        let waited_since = std::time::Instant::now();
        let inner = op_lock().lock().await;
        timing::note_lock_wait(at, waited_since.elapsed());
        OpGuard { inner: Some(inner), at, held_since: std::time::Instant::now() }
    }
}

/// 全域鎖的守衛。除了照舊在 drop 時放鎖，多記一筆「握了多久」。
///
/// `inner` 是 `Option` 只為了一件事：**記帳一定要在放鎖之後**。欄位是晚於 `Drop::drop` 才 drop 的，
/// 所以直接在 `drop` 裡記帳等於「還握著全域鎖去搶 timing 那張表的 mutex」——而 [`timing::snapshot`]
/// 掛在 UI 高頻輪詢的 health 端點上，那一刻正好在算統計的話，放鎖就得排在它後面，等全域鎖的 API
/// 跟著一起慢。那既違反「不改鎖的範圍」，量出來的 hold 又剛好不含被自己拖長的那一段，
/// 對「握著全域鎖做 herdr 送出是不是太久」這個問題系統性偏樂觀（i263 審 #473）。
pub struct OpGuard {
    /// `take()` 之後就是 `None`；只有 [`Drop`] 會動它。
    inner: Option<tokio::sync::MutexGuard<'static, ()>>,
    at: &'static std::panic::Location<'static>,
    held_since: std::time::Instant,
}

impl Drop for OpGuard {
    fn drop(&mut self) {
        let held = self.held_since.elapsed();
        drop(self.inner.take());
        timing::note_lock_hold(self.at, held);
    }
}

/// 測試用：全域鎖現在是不是自由的。**不走 [`lock`]**——那一支自己要記帳（拿 timing 的表），
/// 拿它來探測會把「被鎖擋住」跟「被記帳擋住」混成同一件事。
#[cfg(test)]
pub(crate) fn op_lock_is_free() -> bool {
    op_lock().try_lock().is_ok()
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
pub async fn start_requested(app: &(impl crate::capabilities::Db + crate::capabilities::Emit + TurnOps + QuotaOps + JudgeOps + CandidateSwitch + ControllerRuntime + Clone + 'static)) -> Result<(), LcError> {
    if let Err(e) = Box::pin(controller::apply_quota_policy(app)).await {
        tracing::warn!(error = ?e, "quota policy during start failed");
    }
    if manager_bot(app.db()).await?.is_none() {
        return Err(LcError::conflict("supervisor is not set up", json!({"reason": "not_configured"})));
    }
    store::set_desired_running(app.db(), true).await.map_err(persist_intent_failed)?;
    Box::pin(start_manager(app, None)).await
}

/// 使用者按下 stop：**先把「不要它跑」寫進去，寫成功才停**。呼叫端持有 [`lock`]。
///
/// 順序本來就對，但寫入失敗被吞掉，於是「停好了」會回 200，而看門狗讀到的還是「要它跑」，
/// 下一個 tick 就把它拉回來——使用者看到的是自己停過的東西自己活過來（issue #84）。
pub async fn stop_requested(app: &(impl crate::capabilities::Db + crate::capabilities::Emit + crate::supervisor::ports::TurnOps)) -> Result<(), LcError> {
    let bot = manager_bot(app.db())
        .await?
        .ok_or_else(|| LcError::conflict("supervisor is not set up", json!({"reason": "not_configured"})))?;
    store::set_desired_running(app.db(), false).await.map_err(persist_intent_failed)?;
    app.stop_bot(&bot.id).await?;
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
pub async fn start_manager(app: &(impl crate::capabilities::Db + crate::capabilities::Emit + ports::TurnOps + ports::ControllerRuntime + ports::QuotaOps + ports::JudgeOps + Clone + 'static), detail: Option<&str>) -> Result<(), LcError> {
    let bot = manager_bot(app.db())
        .await?
        .ok_or_else(|| LcError::conflict("supervisor is not set up", json!({"reason": "not_configured"})))?;
    match crate::db::active_run(app.db(), &bot.id).await.map_err(up)? {
        None => {
            Box::pin(app.start_bot(&bot.id)).await?;
        }
        Some(run) if run.state == "stopping" => {
            return Err(LcError::conflict(
                "run_stopping",
                json!({"run_id": run.id, "retryable": true, "message": "上一個 run 還在停止中，等它收掉再啟動"}),
            ));
        }
        Some(_) => {}
    }
    if let Err(e) = Box::pin(app
        .prompt_control_plane(&bot.id, BOOTSTRAP_PROMPT, BOOTSTRAP_REQUEST_ID, Some(crate::agent_relay::DAEMON_SENDER)))
        .await
    {
        tracing::warn!(error = ?e, "AGM bootstrap prompt was not delivered");
    }
    let _ = store::set_status(app.db(), "", detail).await;
    let sup = store::get_or_init(app.db()).await.map_err(up)?;
    let gen = if sup.generation == 0 {
        store::bump_generation(app.db()).await.map_err(up)?
    } else {
        sup.generation
    };
    app.spawn_supervisor_controller(gen);
    Box::pin(controller::reconcile(app)).await;
    app.emit("supervisor_changed", json!({"started": true})).await;
    Ok(())
}

fn up<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(e.to_string())
}

/// The manager's bot, if it is configured *and* still present (a user can delete it).
pub async fn manager_bot(db: &SqlitePool) -> Result<Option<crate::db::Bot>, LcError> {
    let sup = store::get_or_init(db).await.map_err(up)?;
    let Some(id) = sup.bot_id else { return Ok(None) };
    Ok(crate::db::bot(db, &id).await.map_err(up)?.filter(|b| b.deleted_at.is_none()))
}

/// `idle` | `busy`: whether a prompt would be accepted right now. Anything else is not a
/// state we may send work into.
pub async fn manager_liveness(db: &SqlitePool, bot_id: &str) -> Result<&'static str, LcError> {
    let Some(run) = crate::db::active_run(db, bot_id).await.map_err(up)? else { return Ok("stopped") };
    if run.state == "starting" {
        return Ok("starting");
    }
    if run.state != "running" {
        return Ok("stopped");
    }
    if run.agent_status == "working" || run.agent_status == "blocked" {
        return Ok("busy");
    }
    if crate::db::in_flight_turn(db, &run.id).await.map_err(up)?.is_some() {
        return Ok("busy");
    }
    Ok("idle")
}

/// The `GET /api/supervisor` payload — the one shape the web UI and the `agm` CLI both read.
pub async fn status_json(app: &(impl crate::capabilities::Db + crate::supervisor::role_faults::RoleFaultTable)) -> Result<Value, LcError> {
    let sup = store::get_or_init(app.db()).await.map_err(up)?;
    let bot = manager_bot(app.db()).await?;
    let configured = bot.is_some();
    let status = if !configured {
        "not_configured".to_string()
    } else if !sup.status.is_empty() {
        sup.status.clone()
    } else {
        manager_liveness(app.db(), bot.as_ref().map(|b| b.id.as_str()).unwrap_or_default()).await?.to_string()
    };
    let assignments = store::list_assignments_with_open(app.db(), 50).await.map_err(up)?;
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
        "pending_count": store::pending_count(app.db()).await.map_err(up)?,
        "assignments": assignments.iter().map(store::Assignment::to_json).collect::<Vec<_>>(),
        "role": roles::Role::Patrol.as_str(),
        "stats": roles::get(app.db(), roles::Role::Patrol).await.map_err(up)?.stats_json(),
        "responder": responder::status_json(app).await?,
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
    app: &(impl crate::capabilities::Db + crate::capabilities::Emit + QuotaOps + TurnOps + JudgeOps),
    target_bot_id: &str,
    text: &str,
    client_request_id: &str,
    source_turn_id: Option<&str>,
    ownership: &[String],
    follow_up_of: Option<&str>,
    expects_review: bool,
    review_role: Option<roles::Role>,
    actor: Option<roles::Role>,
    reply: bot_requests::ReplyMark<'_>,
) -> Result<Value, LcError> {
    if client_request_id.trim().is_empty() {
        return Err(LcError::Bad("client_request_id must not be empty".into()));
    }
    if text.trim().is_empty() {
        return Err(LcError::Bad("text must not be empty".into()));
    }
    let chars = text.chars().count();
    if chars > crate::lifecycle::MAX_PROVABLE_CHARS {
        return Err(LcError::Unprocessable(json!({
            "error": "text_too_long", "max_chars": crate::lifecycle::MAX_PROVABLE_CHARS, "chars": chars, "sent": false,
        })));
    }
    let _g = lock().await;

    if let Some(a) = store::assignment_by_crid(app.db(), client_request_id).await.map_err(up)? {
        if a.target_bot_id != target_bot_id {
            return Err(LcError::conflict(
                "client_request_id already used for another bot",
                json!({"assignment_id": a.id, "target_bot_id": a.target_bot_id}),
            ));
        }
        if a.text != text {
            return Err(LcError::conflict(
                "client_request_id already used with different text",
                json!({"assignment_id": a.id, "reason": "text_mismatch"}),
            ));
        }
        let mut have_own = a.ownership();
        let mut want_own = ownership.to_vec();
        have_own.sort();
        want_own.sort();
        let mismatch = if (a.expects_review != 0) != expects_review {
            Some("expects_review")
        } else if have_own != want_own {
            Some("ownership")
        } else if a.review_role.as_deref().unwrap_or("responder") != review_role.map_or("responder", roles::Role::as_str) {
            Some("review_role")
        } else {
            None
        };
        if let Some(field) = mismatch {
            return Err(LcError::conflict(
                "client_request_id already used with different settings",
                json!({"assignment_id": a.id, "reason": "assignment_request_mismatch", "field": field}),
            ));
        }
        return Ok(a.to_json());
    }

    let sup = store::get_or_init(app.db()).await.map_err(up)?;
    let manager_id = sup.bot_id.clone().ok_or_else(|| {
        LcError::conflict("supervisor is not set up", json!({"reason": "not_configured"}))
    })?;
    if let Some(target_role) = roles::role_of_bot(app.db(), target_bot_id).await.map_err(up)? {
        if actor == Some(target_role) || target_bot_id == manager_id && actor.is_none() && target_role == roles::Role::Patrol {
            return Err(LcError::Bad("the supervisor cannot assign work to itself".into()));
        }
        let from = match actor {
            Some(r) => roles::bot_for(app.db(), r).await.map_err(up)?,
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
    if reply.is_set() {
        return Err(LcError::Bad("ack / reply_to only apply to a handover between AGM roles".into()));
    }
    crate::db::bot(app.db(), target_bot_id)
        .await
        .map_err(up)?
        .filter(|b| b.deleted_at.is_none())
        .ok_or_else(|| LcError::NotFound("bot".into()))?;

    let (source, source_key, request_text) = source_of(app.db(), &manager_id, source_turn_id, text, actor).await?;
    let request_id = store::upsert_request(app.db(), source, source_key.as_deref(), &request_text)
        .await
        .map_err(up)?;

    let conflicts = ownership_conflicts(app.db(), ownership, None).await?;

    let a = store::insert_assignment_linked(
        app.db(),
        Some(&request_id),
        target_bot_id,
        client_request_id,
        text,
        ownership,
        follow_up_of,
        expects_review,
        review_role.map(roles::Role::as_str),
    )
    .await
    .map_err(up)?;
    if let Some(parent) = follow_up_of {
        store::link_followup(app.db(), parent, &a.id).await.map_err(up)?;
    }
    controller::dispatch(app, &a.id).await;
    app.judge_schedule_assignment(&a.id).await;
    let a = store::assignment(app.db(), &a.id).await.map_err(up)?.unwrap_or(a);
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
    db: &SqlitePool,
    paths: &[String],
    ignore_assignment: Option<&str>,
) -> Result<Vec<Value>, LcError> {
    if paths.is_empty() {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    for other in store::unsettled_assignments(db).await.map_err(up)? {
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
pub async fn check_assignable(db: &SqlitePool, target_bot_id: &str) -> Result<(), LcError> {
    if roles::role_of_bot(db, target_bot_id).await.map_err(up)?.is_some() {
        return Err(LcError::Bad("an AGM role cannot be the target of an assignment; hand over through the role queue".into()));
    }
    crate::db::bot(db, target_bot_id)
        .await
        .map_err(up)?
        .filter(|b| b.deleted_at.is_none())
        .ok_or_else(|| LcError::NotFound("bot".into()))?;
    Ok(())
}

/// Where an assignment came from: `(source, source_key, text)`.
///
/// A native session's user echo reaches the daemon through the hook / transcript path, and
/// that path is **not** verified to carry every word. So: a caller-supplied `source_turn_id` must belong to the
/// manager (a worker must not be able to name someone else's turn as its authority); with no
/// id we fall back to whatever turn the manager is on right now; and if no user text can be
/// read back off that turn, the source is labelled `assignment_text_fallback` rather than
/// pretending the assignment text is the user's own words.
async fn source_of(
    db: &SqlitePool,
    manager_id: &str,
    source_turn_id: Option<&str>,
    text: &str,
    actor: Option<roles::Role>,
) -> Result<(&'static str, Option<String>, String), LcError> {
    let actor_bot = match actor {
        Some(r) => roles::bot_for(db, r).await.map_err(up)?,
        None => None,
    };
    let conv_of = |id: String| async move { crate::db::conversation_id(db, &id).await.map_err(up) };
    let turn_id = match source_turn_id.map(str::trim).filter(|s| !s.is_empty()) {
        Some(t) => {
            let mut convs = Vec::new();
            match actor_bot.clone() {
                Some(id) => convs.push(conv_of(id).await?),
                None => {
                    convs.push(crate::db::conversation_id(db, manager_id).await.map_err(up)?);
                    if let Some(id) = roles::bot_for(db, roles::Role::Responder).await.map_err(up)? {
                        convs.push(crate::db::conversation_id(db, &id).await.map_err(up)?);
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
            let owned: Option<String> = q.fetch_optional(db).await.map_err(up)?;
            if owned.is_none() {
                let whose = actor.map(roles::Role::as_str).unwrap_or("the supervisor");
                return Err(LcError::Bad(format!("source_turn_id is not a turn of {whose}")));
            }
            Some(t.to_string())
        }
        None => match actor_bot {
            Some(id) => match crate::db::active_run(db, &id).await.map_err(up)? {
                Some(r) => crate::db::in_flight_turn(db, &r.id).await.map_err(up)?.map(|t| t.id),
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
    .fetch_optional(db)
    .await
    .map_err(up)?;
    match user_text.filter(|s| !s.trim().is_empty()) {
        Some(t) => Ok(("manager_turn", Some(turn_id), t)),
        None => Ok(("assignment_text_fallback", Some(turn_id), text.to_string())),
    }
}

/// `GET /api/supervisor/state` — what the manager is allowed to see about the world.
///
/// Sanitized on purpose: no env, no hook tokens, no args (an identity's args can name config
/// directories), no persona text. Enough to pick a bot; nothing that helps exfiltrate an account.
pub async fn sanitized_state(app: &(impl crate::capabilities::Db + crate::capabilities::HerdrRoutes + BotLamp)) -> Result<Value, LcError> {
    let projects = crate::db::live_projects(app.db()).await.map_err(up)?;
    let bots = crate::db::live_bots(app.db()).await.map_err(up)?;
    let sup = store::get_or_init(app.db()).await.map_err(up)?;
    let responder_id = roles::get(app.db(), roles::Role::Responder).await.map_err(up)?.bot_id;
    let hosts: std::collections::HashMap<String, String> =
        projects.iter().map(|p| (p.id.clone(), p.host.clone())).collect();
    let mut connected: std::collections::HashSet<String> = Default::default();
    for h in hosts.values() {
        if app.host_connected(h).await {
            connected.insert(h.clone());
        }
    }
    let asleep = idle_sleep::all_asleep(app).await;
    let mut out = Vec::new();
    for b in &bots {
        let run = crate::db::active_run(app.db(), &b.id).await.map_err(up)?;
        let queued: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM turns WHERE conversation_id=(SELECT id FROM conversations WHERE bot_id=?)
               AND status='queued'",
        )
        .bind(&b.id)
        .fetch_one(app.db())
        .await
        .map_err(up)?;
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
            "lamp": app.bot_lamp(&b.id, run.as_ref()).await,
            "queued_turns": queued,
            "asleep": asleep.get(&b.id).map(|(at, mins)| json!({"since": at, "idle_minutes": mins})),
            "run": run.as_ref().map(|r| json!({
                "id": r.id,
                "state": r.state,
                "agent_status": r.agent_status,
                "agent_title": r.agent_title,
                "native_session_id": r.native_session_id,
                "adopted": r.adopted,
                "terminal_id": r.terminal_id,
                "identity_recovery_reason": ports::identity_recovery_reason(
                    r.adopted, r.native_session_id.as_deref(), r.terminal_id.as_deref()),
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
        "open_assignments": store::unsettled_assignments(app.db()).await.map_err(up)?
            .iter().map(store::Assignment::to_json).collect::<Vec<_>>(),
        "pending_inbox": store::pending_inbox(app.db()).await.map_err(up)?
            .iter().map(store::InboxEvent::to_json).collect::<Vec<_>>(),
        "open_incidents": store::open_incidents(app.db()).await.map_err(up)?
            .iter().map(store::Incident::to_json).collect::<Vec<_>>(),
    }))
}
