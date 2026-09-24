//! `/api/supervisor/*` — the contract the web UI and the `agm` CLI both code against.
//!
//! Every route sits inside the existing `X-AM-Token` auth layer; the supervisor has no
//! privilege of its own, it simply reuses the daemon's local management authority.

use crate::lifecycle::LcError;
use crate::state::App;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use super::{controller, health, setup, store};

fn up<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(e.to_string())
}

pub async fn get_supervisor(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    Ok(Json(super::status_json(&app).await?))
}

pub async fn get_health(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    Ok(Json(health::snapshot(&app).await?))
}

/// Build the environment. Idempotent, and deliberately does **not** start anything: a manager
/// that comes up by itself before a human has looked at it is how you get a Remote Control
/// session nobody verified.
pub async fn post_setup(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let _g = super::lock().await;
    let (_project_id, bot_id, deployed) = setup::ensure_env(&app).await?;
    let sup = store::get_or_init(&app.db).await.map_err(up)?;
    // Bringing the environment back up is a new generation: an old controller (from a config
    // that pointed at a bot which no longer exists) must not keep sending notifications.
    if sup.generation == 0 {
        controller::respawn(&app).await;
    }
    controller::reconcile(&app).await;
    app.emit("supervisor_changed", json!({"bot_id": bot_id})).await;
    let mut out = super::status_json(&app).await?;
    out["deployed"] = serde_json::to_value(&deployed).unwrap_or(Value::Null);
    Ok(Json(out))
}

/// 啟停的順序（意圖先寫、寫失敗就不做副作用）住在 `super::start_requested` /
/// `super::stop_requested`，不由每個呼叫端各自維護（issue #84）。
pub async fn post_start(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let _g = super::lock().await;
    super::start_requested(&app).await?;
    Ok(Json(super::status_json(&app).await?))
}

pub async fn post_stop(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let _g = super::lock().await;
    super::stop_requested(&app).await?;
    Ok(Json(super::status_json(&app).await?))
}

/// Switch to the other fixed candidate. Bounded: see `controller::switch_candidate`.
pub async fn post_fallback(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let _g = super::lock().await;
    let sup = store::get_or_init(&app.db).await.map_err(up)?;
    let next = setup::other_candidate(&sup.active_model);
    let switched = controller::switch_candidate(&app, next, "requested", None).await?;
    let mut out = super::status_json(&app).await?;
    out["switched"] = json!(switched);
    Ok(Json(out))
}

#[derive(Deserialize, Default)]
pub struct AssignmentsQuery {
    /// 上一頁的 `next_cursor`（`["<created_at>","<id>"]` 的 JSON），原樣放回來。
    #[serde(default)]
    pub before: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

/// 交辦清單，新的在前。**有分頁**：`before` 沒帶就是第一頁。
///
/// 以前只回最新 200 筆而且沒有任何「還有更多」的訊號，`bin/agm` 又是在客戶端過濾 `--open`，
/// 於是頁外那些未結案的交辦在 CLI 裡完全看不到，而 `blocked`（＝還在等，保持未結案）天生
/// 就活得比一頁久（issue #515）。
pub async fn get_assignments(
    State(app): State<Arc<App>>,
    Query(q): Query<AssignmentsQuery>,
) -> Result<Json<Value>, LcError> {
    let cursor: Option<(String, String)> = q
        .before
        .as_deref()
        .map(|s| {
            if s.len() > 512 {
                return Err(LcError::Bad("invalid assignment cursor".into()));
            }
            serde_json::from_str(s).map_err(|_| LcError::Bad("invalid assignment cursor".into()))
        })
        .transpose()?;
    let limit = q.limit.unwrap_or(200).clamp(1, 500);
    // 多撈一筆才知道後面還有沒有；`has_more` 是「這不是全部」唯一可靠的訊號。
    let mut rows = store::list_assignments_page(
        &app.db,
        cursor.as_ref().map(|c| (c.0.as_str(), c.1.as_str())),
        limit + 1,
    )
    .await
    .map_err(up)?;
    let has_more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let next_cursor = if has_more {
        rows.last().map(|r| serde_json::to_string(&(&r.created_at, &r.id)).expect("string tuple"))
    } else {
        None
    };
    Ok(Json(json!({
        "assignments": rows.iter().map(store::Assignment::to_json).collect::<Vec<_>>(),
        "has_more": has_more,
        "next_cursor": next_cursor,
        "limit": limit,
    })))
}

#[derive(Deserialize)]
pub struct AssignIn {
    pub target_bot_id: String,
    pub text: String,
    pub client_request_id: String,
    #[serde(default)]
    pub source_turn_id: Option<String>,
    /// Files / modules this assignment is being handed, for conflict reporting (§18.4).
    #[serde(default)]
    pub ownership: Vec<String>,
    /// `"notice"` = 只是把話說給 bot 聽，不等回覆也不驗收（§18.8）。省略 = `"task"`。
    #[serde(default)]
    pub kind: Option<String>,
    /// `kind` 的等價寫法，給既有的呼叫端用；兩個都給時以 `expects_review` 為準。
    #[serde(default)]
    pub expects_review: Option<bool>,
    /// 回報給哪個 AGM 角色驗收：`patrol` | `responder`。省略 = 呼叫的角色自己（驗證過的 bot
    /// token），UI／腳本呼叫則是協調者。
    #[serde(default)]
    pub review_role: Option<String>,
    /// 群組任務：這件交辦屬於哪個任務（`/api/missions/{id}`），以及擔任的角色
    /// `executor | reviewer | verifier`。兩個一起給或都不給。
    #[serde(default)]
    pub mission_id: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    /// 交接給另一個 AGM 角色時明講「這是回覆」：`ack`（純告知）或 `reply_to`（回哪一則事件）。
    /// 都沒帶就叫醒對方（SPEC §18.15）；對一般 bot 的交辦帶它是 400。
    #[serde(default)]
    pub ack: bool,
    #[serde(default)]
    pub reply_to: Option<String>,
}

impl AssignIn {
    /// 要不要驗收。舊的呼叫端兩個欄位都不帶 → `true`，行為跟以前一模一樣。
    fn expects_review(&self) -> bool {
        self.expects_review.unwrap_or_else(|| self.kind.as_deref() != Some("notice"))
    }
}

/// 這筆任務的暫停是**人**設的（web 的暫停鈕、`agm mission pause`）嗎？
///
/// daemon 自己設的暫停有固定幾種 reason，它們的意思是「等 AGM 處理」，不是「停手」：輪數用完
/// （`max_rounds`）、驗證者沒有 Fable（`no_fable_for_verifier`）、交付失敗（`push_main_failed`／`pr_failed`）、
/// 要澄清（`clarify`）。其餘的 reason 只會從 `POST /api/missions/{id}/pause` 進來，那就是人按的。
pub(crate) fn user_pause_reason(paused_reason: Option<&str>) -> Option<&str> {
    const DAEMON_SET: [&str; 5] = ["max_rounds", "no_fable_for_verifier", "push_main_failed", "pr_failed", "clarify"];
    paused_reason.map(str::trim).filter(|r| !r.is_empty() && !DAEMON_SET.contains(r))
}

/// 這個任務現在收不收新交辦：已結案（完成／取消）或被使用者暫停就不收。
///
/// `post_assignment` 在鎖外先擋一次（錯的 mission_id 早點回），**算數的是**
/// `mission::workflow::ensure_can_assign` 在 supervisor 鎖裡的那一次：鎖外查完到拿到鎖之間，任務可能已經被
/// 取消或結案（issue #119）。
pub(crate) fn mission_gate(m: &crate::mission::store::Mission) -> Result<(), LcError> {
    if m.completed_at.is_some() || m.cancelled_at.is_some() {
        return Err(LcError::conflict("mission is closed", json!({"reason": "mission_closed", "mission_id": m.id})));
    }
    // 使用者按了暫停就是要它停下來：再派一棒等於當作沒看到（mission3 2026-09-17 轉來的一條）。
    // daemon 自己設的那幾種暫停（輪數用完、驗證者沒 Fable、交付失敗）不擋——runbook 要 AGM
    // 在那些狀態下繼續處理（例如請執行者 rebase 再交付）。
    if let Some(why) = user_pause_reason(m.paused_reason.as_deref()) {
        return Err(LcError::conflict(
            "mission is paused by the user; resume it before handing out more work",
            json!({"reason": "mission_paused", "mission_id": m.id, "paused_reason": why, "hint": "使用者決定之後用 `agm mission resume` 再派"}),
        ));
    }
    Ok(())
}

pub async fn post_assignment(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(b): Json<AssignIn>,
) -> Result<Json<Value>, LcError> {
    let actor = super::bot_requests::actor_role(&app, &headers).await?;
    let review_role = match b.review_role.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(r) => Some(super::roles::Role::parse(r).ok_or_else(|| LcError::Bad("review_role must be patrol | responder".into()))?),
        None => actor,
    };
    // 任務連結先驗證再建交辦：錯的 mission_id 不該留下一件已派出去、卻掛不回任務的工作。
    let mission = match (b.mission_id.as_deref().map(str::trim).filter(|s| !s.is_empty()), b.role.as_deref().map(str::trim)) {
        (None, None | Some("")) => None,
        (Some(mid), Some(role)) if crate::mission::pick::Role::parse(role).is_some() => {
            let m = crate::mission::store::get(&app.db, mid).await.map_err(up)?.ok_or_else(|| LcError::NotFound("mission".into()))?;
            mission_gate(&m)?;
            Some((mid.to_string(), role.to_string()))
        }
        _ => return Err(LcError::Bad("mission_id and role (executor | reviewer | verifier) go together".into())),
    };
    let a = super::assign(
        &app,
        &b.target_bot_id,
        &b.text,
        &b.client_request_id,
        b.source_turn_id.as_deref(),
        &b.ownership,
        None,
        b.expects_review(),
        mission.as_ref().map(|(m, r)| (m.as_str(), r.as_str())),
        review_role,
        actor,
        super::bot_requests::ReplyMark { ack: b.ack, reply_to: b.reply_to.as_deref() },
    )
    .await?;
    Ok(Json(a))
}

/// One assignment, with its decision history.
pub async fn get_assignment(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Json<Value>, LcError> {
    let a = store::assignment(&app.db, &id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("assignment".into()))?;
    let mut out = a.to_json();
    out["reviews"] = json!(store::reviews(&app.db, &a.id).await.map_err(up)?);
    Ok(Json(out))
}

/// 裁示紀錄裡的「是誰」。**只有**驗過的 `X-AM-Bot-Id` 能寫出 `AGM:<role>`；沒驗過的呼叫端
/// 一律記成 `user`，body 裡自稱的名字只當未驗證的備註括進去（issue #414）。
///
/// 以前是 `verified` 為 `None` 時直接採信 body 的 `actor`，而且預設填 `AGM`——UI token 本機
/// 任何行程都拿得到（`relay_auth.rs` 的說明），等於任何一顆 bot 都能把自己的裁示掛成
/// `AGM:responder`，事後從 DB 與 supervisor_notes 都分不出來。
fn decision_actor(verified: Option<super::roles::Role>, claimed: Option<&str>) -> String {
    if let Some(r) = verified {
        return format!("{}:{}", store::SUPERVISOR_ID, r.as_str());
    }
    match claimed.map(str::trim).filter(|c| !c.is_empty()) {
        // 保留呼叫端自稱的名字（`bin/agm … --actor`），但永遠包在 `user(…)` 裡：讀的人一眼看得出
        // 這串字沒有被驗證過，而且它再怎麼填都變不成 `AGM` 開頭。
        Some(c) => format!("user({})", c.chars().take(64).collect::<String>()),
        None => "user".to_string(),
    }
}

#[derive(Deserialize)]
pub struct ReviewIn {
    /// accept | block | followup | fail | cancel
    pub decision: String,
    /// Who decided. AGM names itself; a human acting through the UI says so.
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    /// What the decision rests on — a turn id, a commit, a build log line.
    #[serde(default)]
    pub evidence: Option<String>,
    /// `followup` only: the continuation's text and its own stable request id.
    #[serde(default)]
    pub followup_text: Option<String>,
    #[serde(default)]
    pub followup_request_id: Option<String>,
    #[serde(default)]
    pub followup_bot_id: Option<String>,
    #[serde(default)]
    pub ownership: Vec<String>,
}

/// Accept, block, continue, fail or cancel an assignment.
///
/// This is the only way an assignment is ever called done. A turn ending puts it on
/// `awaiting_review`; what happened to the *work* is a judgement, and a judgement has an author,
/// a reason and evidence attached to it (SPEC §18.3).
///
/// Idempotent: repeating a decision that is already recorded returns the same row without
/// writing a second audit entry, and a `followup` reuses its `followup_request_id`, so a retry
/// after a timeout cannot fan out into two continuations.
/// 裁示當下撤不成排著的那一則時，多久之後叫 flush 再判斷一次（同 flush 讀不到交辦那一條）。
const REVOKE_RECHECK: std::time::Duration = std::time::Duration::from_secs(super::maintenance::UNREADABLE_RETRY_SECS as u64);

pub async fn post_review(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(b): Json<ReviewIn>,
) -> Result<Json<Value>, LcError> {
    let verified = super::bot_requests::actor_role(&app, &headers).await?;
    let _g = super::lock().await;
    let to_status = store::decision_status(&b.decision).ok_or_else(|| {
        LcError::Bad("decision must be one of accept | block | followup | fail | cancel".into())
    })?;
    let a = store::assignment(&app.db, &id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("assignment".into()))?;

    // Already decided this way: hand back what is on file. A retry after a lost response must
    // not look like a second decision.
    if a.status == to_status && a.review_decision.as_deref() == Some(b.decision.as_str()) {
        if b.decision == "followup" {
            let f = match a.followup_assignment_id.as_deref() {
                Some(fid) => store::assignment(&app.db, fid).await.map_err(up)?,
                None => None,
            };
            let identical = f.as_ref().is_some_and(|f| {
                b.followup_request_id.as_deref().map(str::trim) == Some(f.client_request_id.as_str())
                    && b.followup_text.as_deref().map(str::trim) == Some(f.text.as_str())
                    && b.followup_bot_id.as_deref().unwrap_or(&a.target_bot_id) == f.target_bot_id
            });
            if !identical {
                return Err(LcError::conflict("this assignment already has a different continuation",
                    json!({"reason": "followup_mismatch", "followup_assignment_id": a.followup_assignment_id})));
            }
        }
        let mut out = a.to_json();
        out["reviews"] = json!(store::reviews(&app.db, &a.id).await.map_err(up)?);
        out["idempotent"] = json!(true);
        return Ok(Json(out));
    }
    // A decision is only meaningful on something that is still open. Re-deciding a closed
    // assignment would rewrite history; the follow-up mechanism exists for that.
    if !a.is_open() {
        return Err(LcError::conflict(
            "assignment is already closed; create a follow-up assignment instead of re-deciding this one",
            json!({"assignment_id": a.id, "status": a.status, "reason": "already_closed"}),
        ));
    }
    // The turn is still running: there is nothing to accept yet, and accepting it would be a
    // claim about work we can still watch happening.
    //
    // `cancel` is the one decision that applies mid-flight, and it has a cost worth stating:
    // the turn is *not* aborted, so whatever the bot says next will not be recorded against
    // this assignment. `block` is refused here on purpose — it would take the row out of the
    // executing set while its turn is still open, and the result would land nowhere. Wait for
    // the turn to end (it will park on `awaiting_review`) and block it then.
    if a.is_executing() && b.decision != "cancel" {
        return Err(LcError::conflict(
            "assignment has not finished executing; only cancel applies while it is in flight",
            json!({"assignment_id": a.id, "status": a.status, "reason": "still_executing"}),
        ));
    }

    // 驗證過的角色以 token 為準，不信 body 裡自稱的名字（issue #414）。
    let actor = decision_actor(verified, b.actor.as_deref());
    let source = b.source.clone().unwrap_or_else(|| "api".to_string());

    // Cancelling work that is (or may be) already running stops the *tracking*, not the bot.
    // Spelled out here and returned to the caller, because the tempting reading of a cancelled
    // row — "it never went out" — is wrong in two different ways: a `delivered` assignment is
    // running right now, and an `unknown` one may or may not be. The daemon does not abort
    // turns, and it must not let a status imply that it did.
    let still_running = if b.decision != "cancel" {
        None
    } else {
        match a.status.as_str() {
            "delivered" => Some("the turn is still running; cancelling stops tracking it, it does not stop the bot"),
            "unknown" => Some(
                "delivery was never confirmed: this work may or may not be running. Cancelling records your \
                 decision; it does not un-send the prompt and does not abort a turn.",
            ),
            _ => None,
        }
    };

    // A follow-up is a *new* assignment carrying the unfinished part forward, never an edit of
    // the one already sent: rewriting delivered text is how a bot ends up working from words
    // nobody sent it.
    //
    // Order is the whole point here. The decision, the continuation row and the audit entry are
    // written in one guarded transaction *before* anything is dispatched, because a prompt that
    // has gone out cannot be rolled back. Creating the work first and recording it afterwards
    // (the shape this had until 2026-09-13) let two callers each send a continuation and then
    // argue about which decision survived.
    let followup_spec = if b.decision == "followup" {
        let text = b
            .followup_text
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| LcError::Bad("followup needs followup_text".into()))?;
        let crid = b
            .followup_request_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| LcError::Bad("followup needs a stable followup_request_id".into()))?;
        let target = b.followup_bot_id.clone().unwrap_or_else(|| a.target_bot_id.clone());
        // Validate the target before the transaction: a read, and a 400 for a bot that cannot
        // take work is more useful than a rolled-back transaction.
        super::check_assignable(&app, &target).await?;
        // 續作會掛在同一個任務上開一件新的交辦：任務已經結案（完成／取消）就不能再開（issue #119）。這裡在
        // supervisor 鎖裡，跟 `mission cancel`／`complete` 關任務的那一步序列化。暫停不在這裡擋——那是既有的語意，
        // 換手的 followup 在 daemon 設的暫停底下本來就要走得通。
        if let Some(mid) = a.mission_id.as_deref() {
            if let Some(m) = crate::mission::store::get(&app.db, mid).await.map_err(up)? {
                if m.completed_at.is_some() || m.cancelled_at.is_some() {
                    return Err(LcError::conflict("mission is closed", json!({"reason": "mission_closed", "mission_id": mid})));
                }
            }
        }
        Some((target, crid.to_string(), text.to_string()))
    } else {
        None
    };

    // The caveat goes into the audit row too: whoever reads this decision later should see the
    // same warning the caller got, not just the word `cancelled`.
    let evidence = match (b.evidence.as_deref(), still_running) {
        (Some(e), Some(note)) => Some(format!("{e}｜{note}")),
        (None, Some(note)) => Some(note.to_string()),
        (e, None) => e.map(str::to_string),
    };
    let ownership = if b.ownership.is_empty() { a.ownership() } else { b.ownership.clone() };
    let spec = followup_spec.as_ref().map(|(target, crid, text)| store::FollowupSpec {
        target_bot_id: target,
        client_request_id: crid,
        text,
        ownership: &ownership,
        // The continuation answers the same user request as its parent.
        request_id: a.request_id.as_deref(),
    });
    let decided = store::review_with_followup(
        &app.db,
        &a.id,
        &a.status,
        &b.decision,
        &actor,
        &source,
        b.reason.as_deref(),
        evidence.as_deref(),
        spec,
    )
    .await
    .map_err(|e| match e.downcast_ref::<store::FollowupIdTaken>() {
        Some(t) => LcError::conflict(
            "followup_request_id already belongs to another assignment; pick a new id",
            json!({"reason": "followup_request_id_taken", "client_request_id": t.client_request_id, "assignment_id": t.assignment_id}),
        ),
        None => up(e),
    })?;

    let Some(decided) = decided else {
        // The guard did not match: somebody decided it between our read and our write, and
        // **nothing** was written — no continuation, no audit row.
        let now = store::assignment(&app.db, &id).await.map_err(up)?;
        return Err(LcError::conflict(
            "the assignment was decided by someone else first; nothing was written",
            json!({
                "reason": "decided_concurrently",
                "assignment_id": id,
                "status": now.as_ref().map(|n| n.status.clone()),
                "decided_by": now.as_ref().and_then(|n| n.reviewed_by.clone()),
                "decision": now.as_ref().and_then(|n| n.review_decision.clone()),
            }),
        ));
    };
    let updated = decided.updated;

    // 交辦不要了：它排著還沒送出的 queued turn 一併撤銷、釋放名額（AGM 2026-09-16）。
    // 已經 in_flight 或送出的撤不回來，不動——上面的 warning 已經講清楚。
    // 讀不到交辦、或撤不掉（寫不進去）：這一刻撤不成，不假裝沒有要撤的（#200）。那一則不會因此送出——flush 送之前
    // 再判斷一次，讀不到就不送、讀得到就撤（#159）；這裡叫醒 flush、回應裡講明撤銷待補（`revoke_pending_turn_id`）。
    let mut revoked_turn = None;
    let mut revoke_pending = None;
    if matches!(updated.status.as_str(), "cancelled" | "superseded" | "failed") {
        if let Some(tid) = updated.turn_id.as_deref() {
            // 決定 commit 了、還沒撤的那一瞬（測試在這裡讓交辦讀不到）。
            #[cfg(test)]
            crate::lifecycle::race_point::hit("review_before_revoke", &updated.id).await;
            let withdrawal = crate::lifecycle::assignment_withdrawal(&app, tid).await;
            if let Err(e) = &withdrawal {
                tracing::warn!(assignment = %updated.id, turn = tid, error = ?e, "讀不到交辦是否已撤回：排著的那一則這一刻撤不成，交給 flush");
                // 還排著的才有得撤；已經送出去的不講待補。回合的狀態也讀不到時當成可能還排著。
                let queued = sqlx::query_scalar::<_, String>("SELECT status FROM turns WHERE id=?")
                    .bind(tid)
                    .fetch_optional(&app.db)
                    .await
                    .map_or(true, |s| s.as_deref() == Some("queued"));
                if queued {
                    revoke_pending = Some(tid.to_string());
                }
            }
            if let Ok(Some(why)) = withdrawal {
                match crate::lifecycle::revoke_queued_turn(&app, tid, &why).await {
                    Ok(true) => {
                        revoked_turn = Some(tid.to_string());
                        // 決定寫進稽核時還不知道撤不撤得回來；撤回了，「turn 還在跑」那句就不成立。
                        if still_running.is_some() {
                            let note = format!("排隊中的 turn {tid} 已撤回，沒有送出");
                            let amended = match b.evidence.as_deref() {
                                Some(e) => format!("{e}｜{note}"),
                                None => note,
                            };
                            if let Err(e) = store::amend_review_evidence(&app.db, &decided.review_id, evidence.as_deref(), Some(&amended)).await {
                                tracing::warn!(assignment = %updated.id, error = %e, "could not correct the audit evidence after revoking the queued turn");
                            }
                        }
                    }
                    Ok(false) => {}
                    Err(e) => {
                        tracing::error!(assignment = %updated.id, turn = tid, error = %e, "could not revoke the withdrawn assignment's queued turn");
                        revoke_pending = Some(tid.to_string());
                    }
                }
            }
            if revoke_pending.is_some() {
                crate::lifecycle::schedule_flush_retry(&app, &updated.target_bot_id, REVOKE_RECHECK);
            }
        }
    }

    // Committed, so now it can go out. Dispatch is best effort exactly as in `assign`: a
    // failure leaves the row `queued`, which is the recoverable state.
    if let Some(f) = decided.followup.as_ref() {
        controller::dispatch(&app, &f.id).await;
    }
    drop(_g);
    let followup = match decided.followup {
        Some(f) => store::assignment(&app.db, &f.id).await.map_err(up)?.map(|r| r.to_json()),
        None => None,
    };

    app.emit("supervisor_changed", json!({"assignment_id": updated.id, "status": updated.status})).await;
    let mut out = updated.to_json();
    out["reviews"] = json!(store::reviews(&app.db, &updated.id).await.map_err(up)?);
    if let Some(f) = followup {
        out["followup"] = f;
    }
    if let Some(t) = revoked_turn.as_deref() {
        out["revoked_turn_id"] = json!(t);
    }
    if let Some(t) = revoke_pending.as_deref() {
        out["revoke_pending_turn_id"] = json!(t);
        out["revoke_pending"] = json!("排著的那一則這一刻撤不成（讀不到交辦或寫不進去）；它不會被送出——送出前會再判斷一次，讀得到就撤。");
    }
    // 撤掉的是還沒送出的那則：「turn 還在跑」的警告不成立，不要跟 revoked_turn_id 一起回給呼叫端。
    let still_running = if revoked_turn.is_some() { None } else { still_running };
    if let Some(note) = still_running {
        out["warning"] = json!(note);
        // The transport facts stay readable next to the warning: a cancelled `unknown` keeps
        // its `delivery` and `turn_id` precisely so nobody has to guess afterwards.
        out["may_still_be_running"] = json!(true);
    }
    // 群組任務的交辦：裁示之後任務的下一步由 daemon 推導（issue #74，`mission::flow`），一併回給裁示的人。
    if let Some(mid) = updated.mission_id.as_deref() {
        out["mission_next"] = crate::mission::workflow::next_json(&app, mid).await;
    }
    Ok(Json(out))
}

#[derive(Deserialize, Default)]
pub struct IncidentQuery {
    /// `all=1`: resolved incidents too (newest first). Default: only what is open.
    #[serde(default)]
    pub all: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

pub async fn get_incidents(
    State(app): State<Arc<App>>,
    Query(q): Query<IncidentQuery>,
) -> Result<Json<Value>, LcError> {
    let all = q.all.as_deref().is_some_and(|v| matches!(v, "1" | "true" | "yes"));
    let rows = if all {
        store::incidents(&app.db, q.limit.unwrap_or(100).clamp(1, 1000)).await.map_err(up)?
    } else {
        store::open_incidents(&app.db).await.map_err(up)?
    };
    Ok(Json(json!({
        "incidents": rows.iter().map(store::Incident::to_json).collect::<Vec<_>>(),
        "open": store::open_incidents(&app.db).await.map_err(up)?.len(),
        "all": all,
    })))
}

pub async fn get_handoff(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let sup = store::get_or_init(&app.db).await.map_err(up)?;
    // 未結案的一律帶上，就算它已經掉出最新 200 筆——不然 `open_assignments` 會少報，
    // 而冷啟動的總管就是照這個數字決定還有沒有事要接手（issue #515）。
    let assignments = store::list_assignments_with_open(&app.db, 200).await.map_err(up)?;
    let open = assignments.iter().filter(|a| a.is_open()).count();
    Ok(Json(json!({
        "summary": sup.summary,
        "summary_version": sup.summary_version,
        "updated_at": sup.updated_at,
        "requests": store::requests(&app.db, 100).await.map_err(up)?,
        "assignments": assignments.iter().map(store::Assignment::to_json).collect::<Vec<_>>(),
        "inbox": store::inbox(&app.db, 100).await.map_err(up)?.iter().map(store::InboxEvent::to_json).collect::<Vec<_>>(),
        "open_assignments": open,
        "pending_count": store::pending_count(&app.db).await.map_err(up)?,
    })))
}

#[derive(Deserialize)]
pub struct HandoffIn {
    pub summary: String,
}

pub async fn put_handoff(State(app): State<Arc<App>>, Json(b): Json<HandoffIn>) -> Result<Json<Value>, LcError> {
    let version = store::set_summary(&app.db, &b.summary).await.map_err(up)?;
    // The readable copy in the manager's own directory. The database stays authoritative;
    // this is what the manager reads on a cold start before anything else is available.
    let sup = store::get_or_init(&app.db).await.map_err(up)?;
    if let Some(cwd) = sup.cwd.as_deref() {
        let _ = std::fs::write(
            std::path::Path::new(cwd).join("handoff.md"),
            format!("# AGM 管理摘要（v{version}）\n\n{}\n", b.summary),
        );
    }
    app.emit("supervisor_changed", json!({"summary_version": version})).await;
    Ok(Json(json!({"summary": b.summary, "summary_version": version})))
}

#[derive(Deserialize, Default)]
pub struct InboxQuery {
    /// `all=1`: include handled events too (newest first, the audit view). Default is the
    /// work view: only what is still open, oldest first.
    #[serde(default)]
    pub all: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    /// `patrol` | `responder`：只看這個角色的事件（含還沒分類的列不算）。
    #[serde(default)]
    pub role: Option<String>,
}

pub async fn get_inbox(
    State(app): State<Arc<App>>,
    Query(q): Query<InboxQuery>,
) -> Result<Json<Value>, LcError> {
    let limit = q.limit.unwrap_or(200).clamp(1, 1000);
    let all = q.all.as_deref().is_some_and(|v| matches!(v, "1" | "true" | "yes"));
    let role = match q.role.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(r) => Some(super::roles::Role::parse(r).ok_or_else(|| LcError::Bad("role must be patrol | responder".into()))?),
        None => None,
    };
    // 分類在 controller tick 做；讀的人不必等下一個 tick 才看得到角色。
    let _ = super::roles::classify(&app.db).await;
    // 角色條件在 SQL 裡（LIMIT 之前）：先取 limit 筆再過濾，最舊的一批全是另一個角色時，
    // 自己的待辦會永遠翻不到。
    let events = super::roles::list_for(&app.db, role, all, limit).await.map_err(up)?;
    Ok(Json(json!({
        "events": events.iter().map(store::InboxEvent::to_json).collect::<Vec<_>>(),
        "open": store::open_inbox_count(&app.db).await.map_err(up)?,
        "all": all,
        "limit": limit,
        "role": role.map(super::roles::Role::as_str),
    })))
}

/// 結案一則通知。**只給 AGM 角色**：要帶 `X-AM-Bot-Id` 與那顆 bot 自己的 hook token，
/// 否則 403 `role_required`（issue #432）。角色只結得掉自己收的那些，另一個角色收的回 409。
/// 重複 ack 是冪等的。
pub async fn post_inbox_ack(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, LcError> {
    use super::roles::AckOutcome;
    let actor = super::bot_requests::require_role(&app, &headers).await?;
    let responder_configured = super::roles::responder_configured(&app.db).await.map_err(up)?;
    match super::roles::ack(&app.db, &id, actor, responder_configured).await.map_err(up)? {
        AckOutcome::NotFound => Err(LcError::NotFound("inbox event".into())),
        AckOutcome::ClaimedByOther(owner) => Err(LcError::conflict(
            "this event belongs to the other AGM role",
            json!({"reason": "claimed_by_other_role", "claimed_by": owner, "event_id": id}),
        )),
        AckOutcome::AlreadyHandled => Ok(Json(json!({"already_handled": true}))),
        AckOutcome::Acked => {
            app.emit("supervisor_changed", json!({"acked": id})).await;
            Ok(Json(json!({})))
        }
    }
}

pub async fn get_sanitized_state(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    Ok(Json(super::sanitized_state(&app).await?))
}

#[derive(Deserialize)]
pub struct OpsAlertIn {
    /// 哪一支排程腳本（`daemon-update-kick` 之類）。
    pub source: String,
    /// 卡在什麼上（`stale_lock`、`approval_missing`、`state_corrupt`…）。
    pub reason: String,
    #[serde(default)]
    pub detail: Option<String>,
}

/// 例行維運腳本**停住了**，而且它自己解不開：寫一則 durable inbox 事件。
///
/// 那幾支 kick 腳本遇到殘留鎖、狀態檔壞掉、核准 ID 查不到時只能寫進自己的 log 然後 `exit 0`——
/// 沒有 inbox 事件、沒有 incident、health 也不變，正式 daemon 從此不再自動換版而沒有人知道
/// （review 2026-09-16 c1 M1）。這支就是它們喊人的入口：巡檢收、叫醒。
///
/// event_key 帶小時格：每小時最多一則（`push_inbox` 是 INSERT OR IGNORE），五分鐘一輪的腳本
/// 不會把同一件事灌滿 inbox；換一個 reason 就是另一則。
pub async fn post_ops_alert(State(app): State<Arc<App>>, Json(b): Json<OpsAlertIn>) -> Result<Json<Value>, LcError> {
    let slug = |s: &str| -> Option<String> {
        let s = s.trim();
        (!s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))).then(|| s.to_string())
    };
    let source = slug(&b.source).ok_or_else(|| LcError::Bad("source must be 1-64 chars of [A-Za-z0-9._-]".into()))?;
    let reason = slug(&b.reason).ok_or_else(|| LcError::Bad("reason must be 1-64 chars of [A-Za-z0-9._-]".into()))?;
    let detail: String = b.detail.unwrap_or_default().trim().chars().take(2000).collect();
    let hour = crate::db::now().get(..13).unwrap_or_default().to_string();
    let key = format!("ops_alert:{source}:{reason}:{hour}");
    let payload = json!({
        "source": source,
        "reason": reason,
        "detail": detail,
        "action": "這支排程腳本已經停住，自己解不開：照 detail 處理（例如確認沒有執行者後移除殘留鎖），處理完 ack",
    });
    let id = store::push_inbox(&app.db, &key, "ops_alert", None, None, None, &payload).await.map_err(up)?;
    if id.is_some() {
        tracing::warn!(source, reason, detail, "a scheduled ops script reported that it is stuck");
        app.emit("supervisor_changed", json!({"ops_alert": key})).await;
    }
    Ok(Json(json!({"queued": id.is_some(), "inbox_event_id": id, "event_key": key})))
}

// ------------------------------------------------------------------------- remote

/// The phone entry point: what is claimed, on what evidence, and when it stops counting.
pub async fn get_remote(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    Ok(Json(super::remote::status(&app).await))
}

#[derive(Deserialize)]
pub struct RemoteObservationIn {
    /// `requested` | `verified` | `unavailable` | `unknown`.
    pub status: String,
    /// Only `manual` is accepted on this deployment. `provider` is reserved for a future
    /// authenticated adapter; callers cannot upgrade a manual claim into provider evidence.
    pub source: String,
    /// Who is making the claim. Required for anything that asserts the entry point works: an
    /// unattributed "it is fine" is exactly the fiction this endpoint exists to prevent.
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub evidence: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

/// Record an observation of the remote entry point.
///
/// Refuses to let argv, or an anonymous caller, claim the entry point works. The observation is
/// bound to the current session and expires (`remote::OBSERVATION_TTL_SECS`), so it can never
/// harden into a permanent "connected" that nobody rechecked.
pub async fn post_remote_observation(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(b): Json<RemoteObservationIn>,
) -> Result<Json<Value>, LcError> {
    // 身分照 #414 的形狀：驗過的角色才寫得出 `AGM:<role>`，其餘一律 `user` / `user(<自稱>)`。
    // 以前 `actor` 整個由 body 決定，而 doc 卻寫「refuses an anonymous caller」——擋的只有空字串
    // （issue #463）。
    let verified = super::bot_requests::actor_role(&app, &headers).await?;
    if !super::remote::STATES.contains(&b.status.as_str()) {
        return Err(LcError::Bad(format!("status must be one of {:?}", super::remote::STATES)));
    }
    let source = super::remote::Source::parse(&b.source)
        .ok_or_else(|| LcError::Bad("source must be `manual` or `provider`".into()))?;
    super::remote::validate_external_source(source).map_err(|e| LcError::Bad(e.into()))?;
    if b.status == "verified" && !source.can_verify() {
        return Err(LcError::conflict(
            "this source cannot verify the remote entry point",
            json!({"reason": "source_cannot_verify", "source": b.source}),
        ));
    }
    let claim = b.actor.as_deref().map(str::trim).filter(|s| !s.is_empty());
    // 驗過的角色不必再自稱：它的身分本身就是署名。沒驗過的照舊要填一個名字，
    // 只是那個名字會被包成 `user(<自稱>)`，讀的人一眼看得出沒驗過。
    if matches!(b.status.as_str(), "verified" | "unavailable") && claim.is_none() && verified.is_none() {
        return Err(LcError::Bad("an observation that claims verified or unavailable needs an actor".into()));
    }
    let recorded = decision_actor(verified, claim);
    let actor = Some(recorded.as_str());
    if matches!(b.status.as_str(), "verified" | "unavailable")
        && b.evidence.as_deref().is_none_or(|v| v.trim().is_empty()) {
        return Err(LcError::Bad("verified or unavailable needs observation evidence".into()));
    }
    let sup = store::get_or_init(&app.db).await.map_err(up)?;
    let session = match sup.bot_id.as_deref() {
        Some(id) => crate::db::active_run(&app.db, id).await.map_err(up)?.map(|r| r.id),
        None => None,
    };
    if matches!(b.status.as_str(), "verified" | "unavailable") && session.is_none() {
        return Err(LcError::Bad("cannot record a remote observation without an active manager session".into()));
    }
    store::set_remote_observed(
        &app.db,
        &b.status,
        b.url.as_deref(),
        source.as_str(),
        session.as_deref(),
        actor,
        b.evidence.as_deref(),
    )
    .await
    .map_err(up)?;
    tracing::info!(status = %b.status, source = source.as_str(), actor = actor.unwrap_or(""), "remote entry observation recorded");
    app.emit("supervisor_changed", json!({"remote": true})).await;
    Ok(Json(super::remote::status(&app).await))
}

// ------------------------------------------------------------------------ persona

/// Which copy is which, and what the running session can honestly be said to have.
pub async fn get_persona(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let (text, _) = setup::effective_persona(&app).await?;
    let sup = store::get_or_init(&app.db).await.map_err(up)?;
    let embedded = setup::persona_body();
    let embedded_hash = super::persona::hash(&embedded);
    let run_started = match sup.bot_id.as_deref() {
        Some(id) => crate::db::active_run(&app.db, id).await.map_err(up)?.map(|r| r.started_at),
        None => None,
    };
    let loaded = super::persona::loaded_state(run_started.as_deref(), sup.persona_updated_at.as_deref());
    Ok(Json(json!({
        "stored": {
            "version": sup.persona_version,
            "hash": sup.persona_hash,
            "source": sup.persona_source,
            "updated_at": sup.persona_updated_at,
            "seeded_from": sup.persona_seed_hash,
            "length": text.chars().count(),
            "text": text,
        },
        "embedded": {"hash": embedded_hash, "length": embedded.chars().count()},
        // Never `verified`. The daemon passes the persona when it starts the CLI and cannot see
        // what the session holds now, so the only claims here are "nothing is running",
        // "the session predates this text" and "it was started with this text".
        "loaded": {
            "status": loaded.as_str(),
            "run_started_at": run_started,
            "evidence": "the daemon passes the persona at start; it cannot observe the live session",
        },
        // The embedded text moved on. Reported, never applied on its own — that is
        // `POST /api/supervisor/persona/adopt-embedded`.
        "upgrade_available": sup.persona_seed_hash.as_deref().is_some_and(|h| h != embedded_hash),
        "needs_restart": loaded.needs_restart(),
    })))
}

#[derive(Deserialize)]
pub struct PersonaIn {
    pub text: String,
    /// Optimistic concurrency: refuse if somebody else has written since you read.
    #[serde(default)]
    pub expected_version: Option<i64>,
}

/// 改寫總管人設的呼叫端是誰（issue #462）。
///
/// 共用 UI token 的前提下「沒帶身分＝使用者」這個預設不動（網頁就是這樣改 persona 的），所以：
///
/// | `X-AM-Bot-Id` | 結果 |
/// |---|---|
/// | 沒帶 | `Ok("user")`＝使用者／UI，照收 |
/// | 帶了、驗得過、那顆是角色 bot | `Ok("AGM:<role>")` |
/// | 帶了、驗得過、**那顆不是角色 bot** | **403 `role_required`** |
/// | 帶了、證明不了 | **403 `bot_proof_mismatch`**（`verified_bot_id` 丟的） |
///
/// 第三列是這張票的重點：人設就是總管跑的那份角色前導詞（`persona.rs`、`setup.rs`），
/// 一顆普通 bot 沒有理由改寫它；而它本來連 `HeaderMap` 都不收。
/// 界線（沒帶身分仍然過得去）留在 #447 等 per-bot token。
pub(super) async fn persona_actor(app: &Arc<App>, headers: &HeaderMap) -> Result<String, LcError> {
    let Some(id) = super::bot_requests::verified_bot_id(app, headers).await? else {
        return Ok("user".to_string());
    };
    match super::roles::role_of_bot(&app.db, &id).await.map_err(up)? {
        Some(r) => Ok(format!("{}:{}", store::SUPERVISOR_ID, r.as_str())),
        None => Err(LcError::Forbidden(json!({
            "error": "forbidden",
            "reason": "role_required",
            "message": "改寫總管人設只有 AGM 角色或沒有 bot 身分的使用者做得到：這顆 bot 證明得了自己，但它不是巡檢也不是協調者",
            "bot_id": id,
        }))),
    }
}

/// 人設換了就讓它看得見：log 一行＋推一則 `persona_changed` 給巡檢（issue #462）。
///
/// 沒帶身分的呼叫端照收，但「照收」不等於「沒發生過」——改的是總管之後照著做事的那份字，
/// 而 `persona.rs` 自己說線上載入的那份不可觀測，不留痕跡的話改完不會有人發現。
pub(super) async fn note_persona_change(app: &Arc<App>, actor: &str, old: Option<&str>, new: &str, version: i64, how: &str) {
    let lines = |t: &str| t.lines().count();
    let (old_len, new_len) = (old.map(str::len).unwrap_or(0), new.len());
    let summary = json!({
        "actor": actor,
        "verified": actor != "user",
        "how": how,
        "version": version,
        "old_hash": old.map(super::persona::hash),
        "new_hash": super::persona::hash(new),
        "old_bytes": old_len, "new_bytes": new_len,
        "old_lines": old.map(lines).unwrap_or(0), "new_lines": lines(new),
    });
    tracing::info!(
        actor,
        how,
        version,
        old_bytes = old_len,
        new_bytes = new_len,
        old_hash = old.map(super::persona::hash).unwrap_or_default(),
        new_hash = super::persona::hash(new),
        "AGM persona rewritten"
    );
    // 一個版本一則：同一版重送（修 projection）不該再叫醒巡檢一次。
    let key = format!("persona:{version}:changed");
    if let Err(e) = store::push_inbox(&app.db, &key, "persona_changed", None, None, None, &summary).await {
        tracing::warn!(error = ?e, "persona 換了，但通知寫不進 inbox");
    }
}

/// Set the persona. This is the supported path — not editing `config.toml` by hand, and not
/// waiting for some binary's default to win.
pub async fn put_persona(State(app): State<Arc<App>>, headers: HeaderMap, Json(b): Json<PersonaIn>) -> Result<Json<Value>, LcError> {
    let actor = persona_actor(&app, &headers).await?;
    let text = b.text.trim();
    if text.is_empty() {
        return Err(LcError::Bad("persona text must not be empty".into()));
    }
    let _g = super::lock().await;
    let sup = store::get_or_init(&app.db).await.map_err(up)?;
    if let Some(expected) = b.expected_version {
        if expected != sup.persona_version && sup.persona_text.as_deref() != Some(text) {
            return Err(LcError::conflict(
                "the persona changed since you read it",
                json!({"reason": "version_mismatch", "expected": expected, "current": sup.persona_version}),
            ));
        }
    }
    let changed = sup.persona_text.as_deref() != Some(text);
    let version = if changed { store::set_persona(&app.db, text, "api", None).await.map_err(up)? } else { sup.persona_version };
    // 留痕擺在 `sync_persona` **之前**：DB 那一份才是權威，projection 寫不出去時文字已經生效了
    // （`sync_persona` 的說明就是這個意思）。把留痕放在後面的話，部分失敗＝改了卻沒人知道。
    if changed {
        note_persona_change(&app, &actor, sup.persona_text.as_deref(), text, version, "api").await;
    }
    sync_persona(&app, text, version).await?;
    drop(_g);
    app.emit("supervisor_changed", json!({"persona_version": version})).await;
    Ok(Json(get_persona(State(app.clone())).await?.0))
}

#[derive(Deserialize)]
pub struct AdoptIn {
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Take the binary's persona deliberately. The only way the embedded text replaces a stored one
/// — a migration somebody asked for, recorded as such, never a side effect of running `setup`.
pub async fn post_persona_adopt(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(b): Json<AdoptIn>,
) -> Result<Json<Value>, LcError> {
    let actor = persona_actor(&app, &headers).await?;
    let embedded = setup::persona_body();
    let _g = super::lock().await;
    let sup = store::get_or_init(&app.db).await.map_err(up)?;
    let embedded_hash = super::persona::hash(&embedded);
    if sup.persona_hash.as_deref() == Some(embedded_hash.as_str()) {
        sync_persona(&app, &embedded, sup.persona_version).await?;
        return Ok(Json(json!({"changed": false, "reason": "already_identical", "version": sup.persona_version})));
    }
    let previous = sup.persona_text.clone();
    let version = store::set_persona(&app.db, &embedded, "embedded", Some(&embedded_hash)).await.map_err(up)?;
    note_persona_change(&app, &actor, previous.as_deref(), &embedded, version, "adopt_embedded").await;
    sync_persona(&app, &embedded, version).await?;
    drop(_g);
    // `actor` 從驗過的身分來，不是 body 自稱的（issue #463）：以前這裡是
    // `b.actor.as_deref().unwrap_or("AGM")`，任何呼叫端都能把這行 log 掛成 AGM。
    tracing::info!(version, actor, reason = b.reason.as_deref().unwrap_or(""), "AGM persona migrated to the embedded version");
    app.emit("supervisor_changed", json!({"persona_version": version})).await;
    Ok(Json(json!({"changed": true, "version": version, "hash": embedded_hash})))
}

/// A DB write is durable even if a derived file cannot be written. Return that fact instead
/// of silently reporting success; repeating the identical PUT repairs projections without
/// incrementing the version or being rejected by the old expected_version.
async fn sync_persona(app: &Arc<App>, text: &str, version: i64) -> Result<(), LcError> {
    apply_persona(app, text).await.map_err(|e| LcError::conflict(
        "persona stored but projection sync is incomplete; retry the same text to repair",
        json!({"reason": "persona_sync_incomplete", "stored": true, "version": version,
               "sync_error": format!("{e:?}")}),
    ))
}

/// Push the stored persona into the derived copies: the bot's config entry and `persona.md`.
/// Neither is authoritative; both are rewritten from the stored text so they cannot drift.
async fn apply_persona(app: &Arc<App>, text: &str) -> Result<(), LcError> {
    let sup = store::get_or_init(&app.db).await.map_err(up)?;
    let Some(bot_id) = sup.bot_id.clone() else { return Ok(()) };
    let t = text.to_string();
    let bid = bot_id.clone();
    crate::projection::update_and_project(&app.cfg, &app.db, move |cfg| {
        let mut found = false;
        for p in cfg.projects.iter_mut() {
            if let Some(b) = p.bots.iter_mut().find(|b| b.id.as_deref() == Some(bid.as_str())) {
                b.persona = Some(t.clone());
                found = true;
            }
        }
        anyhow::ensure!(found, "manager bot is missing from config");
        Ok(())
    })
    .await
    .map_err(up)?;
    std::fs::write(setup::agm_dir(app).join("persona.md"), text).map_err(up)?;
    Ok(())
}

/// What is compiled into the binary, so the update script can tell "the release is behind" from
/// "only docs moved" without having to know about `include_str!` itself.
pub async fn get_build_inputs(State(_app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    Ok(Json(super::persona::build_inputs_json()))
}

// ------------------------------------------------------------- approvals & leases

#[derive(Deserialize)]
pub struct ApprovalIn {
    pub requester: String,
    /// `rebuild` | `restart`: what this is permission *for*. A lease can only be taken on the
    /// resource its approval names.
    pub purpose: String,
    pub scope: String,
    #[serde(default)]
    pub target_commit: Option<String>,
    #[serde(default)]
    pub expires_in_secs: Option<i64>,
    /// 穩定的 request id：同一個再送回**原本那一筆**，不新增（2026-09-16 的重複申請事故）。
    /// 不帶就是舊行為，每次都開一筆新的。
    #[serde(default)]
    pub request_id: Option<String>,
    /// 同一個申請者換 commit 重新申請：舊的那筆標成 `superseded`，等待起點接過來（SPEC §18.10）。
    #[serde(default)]
    pub supersedes: Option<String>,
    /// 申請理由：AGM 裁示時看的就是這段（2026-09-19：CLI 有 `--reason` 但根本沒送，三張申請
    /// 都被當成「未附理由」駁回）。存在 `request_reason`，不會被裁示理由蓋掉。
    #[serde(default)]
    pub reason: Option<String>,
}

fn iso_in(secs: i64) -> String {
    crate::db::iso_in(secs)
}

/// 這筆申請的 `requester` 是不是真的是呼叫端本人（issue #436）。
///
/// **驗過就必須是自己**：帶著 `X-AM-Bot-Id`＋自己的 hook token 來申請，卻把 `requester` 填成別人，
/// 一律 403——不是默默改寫成驗過的那顆。改寫看起來比較體貼，但 `requester` 之後要跟 `lease acquire`
/// 的 `--owner` **逐字相等**（`maintenance.rs` 的 `approval_owner_mismatch`），而那串字只有呼叫端
/// 自己知道會填什麼形式（bot id／bot 名／herdr agent 名三種都合法，`requester_bot_id` 三種都認）。
/// 我們改寫＝讓它後面開不了窗口，還很難查。
///
/// 沒帶身分的照收（`daemon-update-kick.sh` 這類 launchd 腳本就是這樣申請的），只是記成沒驗過。
async fn requester_claim(app: &Arc<App>, headers: &HeaderMap, requester: &str) -> Result<bool, LcError> {
    let Some(caller) = super::bot_requests::verified_bot_id(app, headers).await? else { return Ok(true) };
    if super::maintenance::requester_bot_id(app, requester).await.as_deref() == Some(caller.as_str()) {
        return Ok(false);
    }
    Err(LcError::Forbidden(json!({
        "error": "forbidden",
        "reason": "requester_not_the_caller",
        "message": "帶了 X-AM-Bot-Id 就只能用自己的名義申請：requester 要是你這顆 bot（id、bot 名或 agent 名都行）",
        "requester": requester,
    })))
}

/// Ask for a rebuild / restart window. Creates a `pending` record; AGM decides it.
pub async fn post_approval(State(app): State<Arc<App>>, headers: HeaderMap, Json(b): Json<ApprovalIn>) -> Result<Json<Value>, LcError> {
    if !super::maintenance::RESOURCES.contains(&b.purpose.as_str()) {
        return Err(LcError::Bad(format!("purpose must be one of {:?}", super::maintenance::RESOURCES)));
    }
    let unverified = requester_claim(&app, &headers, &b.requester).await?;
    let expires = b.expires_in_secs.map(iso_in);
    let out = store::create_approval_notifying(
        &app.db,
        &b.requester,
        &b.purpose,
        &b.scope,
        b.target_commit.as_deref(),
        expires.as_deref(),
        b.request_id.as_deref(),
        b.supersedes.as_deref(),
        b.reason.as_deref(),
        unverified,
    )
    .await
    .map_err(|e| match e.downcast::<store::ApprovalRequestMismatch>() {
        // 同一個 request id 換了內容不是重送，是兩件事共用了一個 id：什麼都不動。
        Ok(m) => LcError::conflict(
            "approval_request_mismatch",
            json!({"reason": "approval_request_mismatch", "message": m.to_string(), "request_id": m.request_id,
                   "approval_id": m.approval_id, "field": m.field, "existing": m.old, "requested": m.new}),
        ),
        Err(e) => match e.downcast::<store::ApprovalSupersedeRefused>() {
            Ok(r) => LcError::conflict(
                "approval_supersede_refused",
                json!({"reason": r.reason, "message": r.to_string(), "supersedes": r.approval_id}),
            ),
            Err(e) => up(e),
        },
    })?;
    let a = out.approval;
    // 重送不再叫醒 AGM 一次，也不再推一次事件：它看到的還是同一筆。
    if out.created {
        // AGM decides these itself (CLAUDE.md, 2026-09-12): the request is put in front of it as an
        // inbox event rather than sent to the user to relay. 事件已在建立的同一個交易裡寫好（#320）。
        app.emit("supervisor_changed", json!({"approval": a.to_json()})).await;
    }
    if let Some(old) = out.superseded.as_deref() {
        app.emit("supervisor_changed", json!({"approval_superseded": old, "by": a.id})).await;
    }
    let mut v = a.to_json();
    // 呼叫端要分得出「這次新開的」與「回你原本那筆」，才不會把重送讀成沒送出去。
    v["created"] = json!(out.created);
    v["superseded"] = json!(out.superseded);
    Ok(Json(v))
}

#[derive(Deserialize)]
pub struct DecisionIn {
    /// approve | deny | revoke
    pub decision: String,
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub expires_in_secs: Option<i64>,
}

pub async fn post_approval_decision(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(b): Json<DecisionIn>,
) -> Result<Json<Value>, LcError> {
    let verified = super::bot_requests::actor_role(&app, &headers).await?;
    let _g = super::lock().await;
    let status = match b.decision.as_str() {
        "approve" => "approved",
        "deny" => "denied",
        "revoke" => "revoked",
        _ => return Err(LcError::Bad("decision must be approve | deny | revoke".into())),
    };
    let current = store::approval(&app.db, &id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("approval".into()))?;
    // 自己核准自己不算核准（issue #436）。核准是換 binary／重啟窗口的授權來源，申請與裁示要是兩個人。
    //
    // 只擋 `approve`：把自己送出去的申請 `deny`／`revoke` 掉就是收回申請，本來就該讓他做。
    //
    // **不看那筆申請驗過沒**（2026-09-24 裁示，i406 的審核）：以前只在 `requester_unverified == 0` 時比對，
    // 於是守衛變成被約束者自己選配的——申請時不帶 `X-AM-Bot-Id`（`requester` 照樣填自己），裁示時再帶自己的
    // 身分，整段就跳過了。現在只問一件事：**這個呼叫端驗得過，而且 `requester` 指的就是它**。
    // 沒驗過的申請被擋下來也是對的：要嘛真的是它送的，要嘛有人冒它的名，兩種都該去查而不是直接放行。
    // 不影響 `daemon-update-kick`（`agm-kick` 解析不到任何 bot）與走 UI 的人（沒有 bot 身分）。
    if status == "approved" {
        if let Some(caller) = super::bot_requests::verified_bot_id(&app, &headers).await? {
            // 讀不到就不敢放行（issue #436）：這裡的「解析不到」等於通過，所以 DB 出錯不能吞成「不是它」。
            let owner = super::maintenance::try_requester_bot_id(&app, &current.requester).await.map_err(|e| {
                LcError::Unavailable(json!({
                    "error": "unavailable", "reason": "requester_lookup_failed", "retryable": true, "sent": false,
                    "message": format!("查不出這筆申請是誰送的，不敢就這樣核准：{e}"),
                    "approval_id": current.id,
                }))
            })?;
            if owner.as_deref() == Some(caller.as_str()) {
                return Err(LcError::Forbidden(json!({
                    "error": "forbidden",
                    "reason": "self_approval_forbidden",
                    "message": "不能核准自己送出的申請：換 binary／重啟的窗口要由另一個人裁示（自己的申請可以 deny／revoke 收回）",
                    "approval_id": current.id,
                    "requester": current.requester,
                })));
            }
        }
    }
    // A decision that is already recorded is answered from the row: a retry is not a new
    // decision, and re-approving something that was revoked has to be explicit.
    if current.status == status {
        let mut out = current.to_json();
        out["idempotent"] = json!(true);
        return Ok(Json(out));
    }
    let expires = b.expires_in_secs.map(iso_in);
    let actor = decision_actor(verified, b.actor.as_deref());
    // **第一個裁示定案**：approve／deny 只從 `pending` 寫得進去，而且條件寫在 SQL 裡、不看先前
    // 讀到的值。否則兩個角色同時決定時，後到的那個會把 `approved` 改成 `denied`——同一筆核准就
    // 有了兩個「第一次裁示」。要翻案得明講 `revoke`，而且歷程留著（`supervisor_notes`）。
    let allowed_from: &[&str] = match status {
        "approved" | "denied" => &["pending"],
        // 撤銷的是還有效的許可：已核准的收回，還沒決定的等於作廢。
        _ => &["approved", "pending"],
    };
    let mut decided = None;
    let mut from_status = "";
    for from in allowed_from {
        // 決定與它的稽核紀錄在同一個 transaction 裡（`decide_approval_from`）：分兩步寫的話，
        // note 失敗會留下「有決定、沒紀錄」，而重送同一個決定會撞上 idempotent 直接回成功，
        // 那筆 audit 就永遠補不回來。
        if let Some(pair) = store::decide_approval_from(&app.db, &id, from, status, &actor, b.reason.as_deref(), expires.as_deref())
            .await
            .map_err(up)?
        {
            decided = Some(pair);
            from_status = from;
            break;
        }
    }
    let Some((a, note)) = decided else {
        let now = store::approval(&app.db, &id).await.map_err(up)?;
        let status_now = now.as_ref().map(|n| n.status.clone());
        let reason = if status_now.as_deref() == Some("pending") { "decided_concurrently" } else { "already_decided" };
        return Err(LcError::conflict(
            "this approval already has a decision; nothing was written",
            json!({
                "reason": reason,
                "approval_id": id,
                "status": status_now,
                "decided_by": now.as_ref().and_then(|n| n.decided_by.clone()),
                "allowed_from": allowed_from,
                "hint": "翻案要明講 revoke；要重新申請就開一筆新的 approval",
            }),
        ));
    };
    app.emit("supervisor_changed", json!({"approval": a.to_json()})).await;
    let mut out = a.to_json();
    out["audit_note_id"] = json!(note);
    out["decided_from"] = json!(from_status);
    Ok(Json(out))
}

#[derive(Deserialize, Default)]
pub struct ApprovalQuery {
    /// 只查這一筆。清單本身只回最新 100 筆，被擠出去之後排程腳本就查不到自己那筆核准，
    /// 每一輪都停在「無法確認核准」（review 2026-09-16 c1 M1）。
    #[serde(default)]
    pub id: Option<String>,
}

pub async fn get_approvals(State(app): State<Arc<App>>, Query(q): Query<ApprovalQuery>) -> Result<Json<Value>, LcError> {
    let rows = match q.id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(id) => store::approval(&app.db, id).await.map_err(up)?.into_iter().collect(),
        None => store::approvals(&app.db, 100).await.map_err(up)?,
    };
    // 決定歷程跟著回：核准列只有最後一個狀態，「誰核准的、後來被誰撤銷」要查得到。
    let mut history = store::approval_decisions(&app.db).await.map_err(up)?;
    let out: Vec<Value> = rows
        .iter()
        .map(|a| {
            let mut v = a.to_json();
            v["decisions"] = json!(history.remove(&a.id).unwrap_or_default());
            v
        })
        .collect();
    Ok(Json(json!({"approvals": out})))
}

/// Whether a window would be safe *right now*. A read: poll it while you wait, and take the
/// window with `acquire`, which re-checks this under the lock.
#[derive(Deserialize, Default)]
pub struct SafetyQuery {
    /// Comma-separated bot IDs, matching acquire's exclude_bot_ids.
    #[serde(default)]
    pub exclude: Option<String>,
    /// 要問「這一筆核准現在開得了窗口嗎」時帶上它：升級的計時就綁在它身上（SPEC §18.10）。
    /// 不帶＝純查詢，退回看最早那筆還活著的核准。
    #[serde(default)]
    pub approval: Option<String>,
    /// 以這個 owner 的身分問：他自己握的租約不算擋（SPEC §18.10「自己的租約不擋自己」）。
    /// 不帶＝舊行為，每一把租約都算擋。
    #[serde(default)]
    pub owner: Option<String>,
}

pub async fn get_maintenance_safety(
    State(app): State<Arc<App>>,
    Query(q): Query<SafetyQuery>,
) -> Result<Json<Value>, LcError> {
    let mut exclude = Vec::new();
    for id in q.exclude.as_deref().unwrap_or("").split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if !exclude.iter().any(|s| s == id) {
            exclude.push(id.to_string());
        }
    }
    let approval = q.approval.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let owner = q.owner.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let mut snapshot = super::maintenance::safety_as(&app, &exclude, approval, owner).await?;
    // Echo the applied IDs so callers can distinguish an older daemon ignoring the query.
    snapshot["excluded_bot_ids"] = json!(exclude);
    Ok(Json(snapshot))
}

pub async fn get_leases(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let rows = store::leases(&app.db).await.map_err(up)?;
    Ok(Json(json!({"leases": rows.iter().map(store::Lease::to_json).collect::<Vec<_>>()})))
}

#[derive(Deserialize)]
pub struct AcquireIn {
    pub owner: String,
    pub approval_id: String,
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub ttl_secs: Option<i64>,
    /// Refuse the window unless nothing is running. Default true: taking a restart window while
    /// a bot is mid-turn is the thing this exists to prevent.
    #[serde(default = "yes")]
    pub require_idle: bool,
    /// Bots to ignore in the idle check — normally just the one doing the maintenance.
    #[serde(default)]
    pub exclude_bot_ids: Vec<String>,
}

fn yes() -> bool {
    true
}

pub async fn post_lease_acquire(
    State(app): State<Arc<App>>,
    Path(resource): Path<String>,
    Json(b): Json<AcquireIn>,
) -> Result<Json<Value>, LcError> {
    Ok(Json(
        super::maintenance::acquire(
            &app,
            &resource,
            &b.owner,
            &b.approval_id,
            b.commit.as_deref(),
            b.ttl_secs.unwrap_or(super::maintenance::DEFAULT_TTL_SECS),
            b.require_idle,
            &b.exclude_bot_ids,
        )
        .await?,
    ))
}

#[derive(Deserialize)]
pub struct LeaseHolderIn {
    pub owner: String,
    /// The fence from `acquire`. A stale one is refused — that is the whole point of it.
    pub fence: i64,
    #[serde(default)]
    pub ttl_secs: Option<i64>,
    /// `acquire` 當下發的一次性憑證。owner 與 fence 是公開的，只靠它們等於誰都能把別人
    /// 正在換 binary 的窗口收掉（review 2026-09-16）。
    #[serde(default)]
    pub lease_token: Option<String>,
    /// AGM 的強制釋放（例如持有者已經不在了）。`reason` 必填，會寫進 supervisor_notes。
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub reason: Option<String>,
}

/// 把 body 換成一個憑證。`force` 是**授權**問題，不只是稽核問題：只有 AGM 角色能接管別人的窗口，
/// 而且一定要附理由。少了角色檢查的話，任何打得到 7788 的呼叫端多帶一個 `force=true` 就能把別人
/// 正在換 binary 的窗口收掉——留了紀錄，但沒有界線（review 續補 2026-09-16）。
fn lease_proof<'a>(b: &'a LeaseHolderIn, actor: Option<super::roles::Role>) -> Result<store::LeaseProof<'a>, LcError> {
    if b.force {
        let Some(role) = actor else {
            return Err(LcError::Forbidden(json!({
                "error": "forbidden", "reason": "lease_force_forbidden",
                "message": "只有 AGM 角色可以強制接管窗口；持有者請用 acquire 當下拿到的 lease_token 交還"
            })));
        };
        let _ = role;
        let ok = b.reason.as_deref().map(str::trim).is_some_and(|r| !r.is_empty());
        if !ok {
            return Err(LcError::Bad("force 需要 --reason（會寫進稽核紀錄）".into()));
        }
        return Ok(store::LeaseProof::Forced);
    }
    // 沒帶不是當場拒絕：升級前建立的租約沒有 token 可以帶，那一種由 `allows()` 放行。
    Ok(match b.lease_token.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => store::LeaseProof::Token(t),
        None => store::LeaseProof::Absent,
    })
}

/// 被擋下來的 force 也要留一行：誰、在哪個 resource、判定到什麼角色。
fn force_warn(e: LcError, resource: &str, owner: &str, actor: Option<super::roles::Role>) -> LcError {
    if let LcError::Forbidden(v) = &e {
        if v["reason"] == "lease_force_forbidden" {
            tracing::warn!(
                resource,
                claimed_owner = owner,
                role = actor.map(|r| r.as_str()).unwrap_or("none"),
                "refused a forced lease release from a caller that is not an AGM role"
            );
        }
    }
    e
}

/// 憑證對不上：一個字都不動。
fn lease_forbidden(resource: &str, owner: &str, had_token: bool) -> LcError {
    tracing::warn!(resource, claimed_owner = owner, had_token, "refused a lease renew/release without a matching token");
    let reason = if had_token { "lease_token_mismatch" } else { "lease_token_required" };
    LcError::Forbidden(json!({
        "error": "forbidden", "reason": reason,
        "message": "renew／release 要帶 acquire 當下拿到的 lease_token（真的要接管請用 force 並附理由）；租約沒有任何變動"
    }))
}

pub async fn post_lease_renew(
    State(app): State<Arc<App>>,
    Path(resource): Path<String>,
    headers: HeaderMap,
    Json(b): Json<LeaseHolderIn>,
) -> Result<Json<Value>, LcError> {
    let actor = super::bot_requests::actor_role(&app, &headers).await?;
    // force 是「持有者已經不在了、收掉它的窗口」，不是「替別人延長」：renew 只認 token。
    // 以前 AGM 角色帶 force 就能用公開的 owner／fence 延長別人的租約，而且不留 note（review2 sup #5）。
    if b.force {
        return Err(LcError::Bad("renew 不接受 force：延長租約要帶 acquire 當下的 lease_token；要接管請 release --force".into()));
    }
    let _g = super::lock().await;
    let ttl = b.ttl_secs.unwrap_or(super::maintenance::DEFAULT_TTL_SECS).clamp(30, super::maintenance::MAX_TTL_SECS);
    // Renewal re-checks the permission, it does not just extend the clock. An approval that was
    // revoked (or has lapsed) while the holder was working must not be extendable by the holder
    // — otherwise "revoked" only takes effect whenever the current lease happens to run out.
    let held = store::lease(&app.db, &resource).await.map_err(up)?;
    let approval = match held.as_ref().and_then(|l| l.approval_id.clone()) {
        Some(ap) => store::approval(&app.db, &ap).await.map_err(up)?,
        None => None,
    };
    if approval.is_none() {
        return Err(LcError::conflict(
            "the lease has no valid approval record; release it and request a new window",
            json!({"reason": "approval_missing"}),
        ));
    }
    if let Some(approval) = approval.as_ref() {
        let commit = held.as_ref().and_then(|l| l.target_commit.clone());
        if let Some(reason) = approval.refusal(&crate::db::now(), &resource, commit.as_deref()) {
            return Err(LcError::conflict(
                "the approval behind this lease is no longer valid; release the window and ask again",
                json!({"reason": reason, "approval_id": approval.id, "status": approval.status}),
            ));
        }
    }
    // 憑證在任何寫入之前比對；`renew_lease` 只認 owner＋fence，那兩個是公開欄位。
    let proof = lease_proof(&b, actor).map_err(|e| force_warn(e, &resource, &b.owner, actor))?;
    if !proof.allows(store::lease_token(&app.db, &resource).await.map_err(up)?.as_deref()) {
        return Err(lease_forbidden(&resource, &b.owner, b.lease_token.is_some()));
    }
    let deadline = super::maintenance::lease_deadline(&iso_in(ttl), approval.as_ref().and_then(|a| a.expires_at.as_deref()));
    if !store::renew_lease(&app.db, &resource, &b.owner, b.fence, &deadline).await.map_err(up)? {
        let held = store::lease(&app.db, &resource).await.map_err(up)?;
        return Err(LcError::conflict(
            "this lease is no longer yours",
            json!({"reason": "lease_lost", "lease": held.map(|l| l.to_json())}),
        ));
    }
    let l = store::lease(&app.db, &resource).await.map_err(up)?.ok_or_else(|| LcError::NotFound("lease".into()))?;
    Ok(Json(l.to_json()))
}

pub async fn post_lease_release(
    State(app): State<Arc<App>>,
    Path(resource): Path<String>,
    headers: HeaderMap,
    Json(b): Json<LeaseHolderIn>,
) -> Result<Json<Value>, LcError> {
    let actor = super::bot_requests::actor_role(&app, &headers).await?;
    let proof = lease_proof(&b, actor).map_err(|e| force_warn(e, &resource, &b.owner, actor))?;
    // Consumes the approval (one yes, one window) and, for a restart window, lifts the holds it
    // placed so held assignments go out on the next pass.
    let released = super::maintenance::release(&app, &resource, &b.owner, b.fence, proof).await.map_err(|e| {
        if e.to_string() == "lease_token_mismatch" {
            lease_forbidden(&resource, &b.owner, b.lease_token.is_some())
        } else {
            up(e)
        }
    })?;
    if b.force {
        let reason = b.reason.clone().unwrap_or_default();
        let role = actor.map(|r| r.as_str()).unwrap_or("?");
        tracing::warn!(resource = %resource, owner = %b.owner, role, reason = %reason, "a maintenance window was force-released");
        let _ = store::add_note(
            &app.db,
            "lease_force_release",
            &json!({"resource": resource, "owner": b.owner, "fence": b.fence, "reason": reason,
                    "released": released, "by_role": role}),
        )
        .await;
    }
    let l = store::lease(&app.db, &resource).await.map_err(up)?.ok_or_else(|| LcError::NotFound("lease".into()))?;
    app.emit("supervisor_changed", json!({"lease": l.to_json()})).await;
    Ok(Json(json!({"released": released, "lease": l.to_json()})))
}

#[cfg(test)]
mod persona_sync_tests {
    use super::*;

    #[tokio::test]
    async fn migration_and_failed_projection_can_be_retried_without_losing_text() {
        let dir = std::env::temp_dir().join(format!("agm-persona-sync-{}", crate::db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let bid = crate::db::ulid();
        let pid = crate::db::ulid();
        std::fs::write(dir.join("config.toml"), format!(
            "[[projects]]\nid = '{pid}'\npath = '{}'\nlabel = 'AGM'\n[[projects.bots]]\nid = '{bid}'\nname = 'AGM'\nkind = 'claude'\npersona = 'existing custom persona'\n", dir.display()
        )).unwrap();
        let db = crate::db::open(&dir.join("test.sqlite")).await.unwrap();
        let cfg = crate::config::ConfigStore::load(dir.join("config.toml")).await.unwrap();
        crate::projection::project_config(&cfg, &db).await.unwrap();
        let client = crate::herdr::HerdrClient::new(dir.join("absent.sock"));
        let app = App::new(db, client.clone(), client, cfg, dir.clone(), dir.join("daemon"),
                           7799, "test".into(), "test".into(), false);
        store::get_or_init(&app.db).await.unwrap();
        sqlx::query("UPDATE supervisors SET bot_id=? WHERE id=?")
            .bind(&bid).bind(store::SUPERVISOR_ID).execute(&app.db).await.unwrap();
        assert_eq!(setup::effective_persona(&app).await.unwrap().0, "existing custom persona");
        let initial = store::get_or_init(&app.db).await.unwrap().persona_version;
        // persona.md cannot be created yet. A partial sync must be visible, while the new
        // authoritative text remains durable and recoverable using the exact same request.
        let err = put_persona(State(app.clone()), HeaderMap::new(), Json(PersonaIn {
            text: "new persona".into(), expected_version: Some(initial),
        })).await.unwrap_err();
        assert!(format!("{err:?}").contains("persona_sync_incomplete"));
        let stored = store::get_or_init(&app.db).await.unwrap();
        assert_eq!(stored.persona_text.as_deref(), Some("new persona"));
        std::fs::create_dir_all(setup::agm_dir(&app)).unwrap();
        let repaired = put_persona(State(app.clone()), HeaderMap::new(), Json(PersonaIn {
            text: "new persona".into(), expected_version: Some(initial),
        })).await.unwrap().0;
        assert_eq!(repaired["stored"]["version"], stored.persona_version);
        assert_eq!(std::fs::read_to_string(setup::agm_dir(&app).join("persona.md")).unwrap(), "new persona");
        assert_eq!(crate::db::bot(&app.db, &bid).await.unwrap().unwrap().persona.as_deref(), Some("new persona"));
        // An old request with DIFFERENT text still cannot overwrite the new revision.
        assert!(put_persona(State(app.clone()), HeaderMap::new(), Json(PersonaIn {
            text: "stale edit".into(), expected_version: Some(initial),
        })).await.is_err());
        app.db.close().await;
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod approval_decision_tests {
    use super::*;

    async fn app() -> Arc<App> {
        let dir = std::env::temp_dir().join(format!("agm-approval-{}", crate::db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = crate::db::open(&dir.join("test.sqlite")).await.unwrap();
        let cfg = crate::config::ConfigStore::load(dir.join("config.toml")).await.unwrap();
        let client = crate::herdr::HerdrClient::new(dir.join("absent.sock"));
        let app = App::new(db, client.clone(), client, cfg, dir.clone(), dir.join("daemon"), 7799, "test".into(), "test".into(), false);
        store::get_or_init(&app.db).await.unwrap();
        app
    }

    async fn decide(app: &Arc<App>, id: &str, decision: &str, actor: &str) -> Result<Json<Value>, LcError> {
        let body: DecisionIn = serde_json::from_value(json!({"decision": decision, "actor": actor})).unwrap();
        post_approval_decision(State(app.clone()), Path(id.to_string()), HeaderMap::new(), Json(body)).await
    }

    async fn pending(app: &Arc<App>) -> String {
        store::create_approval(&app.db, "fixer", "rebuild", "release", Some("abc123"), None, None).await.unwrap().approval.id
    }

    /// issue #462：改寫總管人設以前連 `HeaderMap` 都不收。共用 UI token 的前提下「沒帶身分＝
    /// 使用者」的預設不動（網頁就是這樣改的），但**證明得了自己卻不是角色 bot** 的呼叫端要 403——
    /// 一顆普通 bot 沒有理由改寫總管跑的那份角色前導詞。而且不管誰改，都要留下痕跡。
    #[tokio::test]
    async fn rewriting_the_persona_is_refused_for_a_bot_that_is_not_a_role_and_always_leaves_a_trace() {
        let app = app().await;
        let agm = agm_role_headers(&app).await; // 巡檢角色
        // 這個精簡的測試 app 沒有 manager bot 的 config 列，`sync_persona` 必然回
        // `persona_sync_incomplete`。那正好是要測的那一格：權威寫入已經生效，留痕不能被它吃掉。
        let put = |app: &Arc<App>, h: HeaderMap, text: &str| {
            let (app, text) = (app.clone(), text.to_string());
            async move {
                match put_persona(State(app), h, Json(PersonaIn { text, expected_version: None })).await {
                    Ok(_) => Ok(()),
                    Err(LcError::Conflict(v)) if v["reason"] == "persona_sync_incomplete" => Ok(()),
                    Err(e) => Err(e),
                }
            }
        };
        let inbox = |app: &Arc<App>| {
            let app = app.clone();
            async move {
                sqlx::query_as::<_, (String, String)>(
                    "SELECT payload_json, COALESCE(role,'') FROM supervisor_inbox WHERE kind='persona_changed' ORDER BY rowid",
                )
                .fetch_all(&app.db)
                .await
                .unwrap()
            }
        };

        // 1) 驗得過、但那顆不是角色 bot：403，一個字都不寫。
        let plain = crate::db::ulid();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES (?,'p-agm','plain','claude','plain-tok',?)")
            .bind(&plain)
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let mut plain_h = HeaderMap::new();
        plain_h.insert("X-AM-Bot-Id", plain.parse().unwrap());
        plain_h.insert("X-AM-Bot-Token", "plain-tok".parse().unwrap());
        let err = put(&app, plain_h, "一顆普通 bot 塞進來的指示").await.unwrap_err();
        let LcError::Forbidden(v) = &err else { panic!("expected 403, got {err:?}") };
        assert_eq!(v["reason"], "role_required");
        assert!(store::get_or_init(&app.db).await.unwrap().persona_text.is_none(), "被擋下就不該寫進去");
        assert!(inbox(&app).await.is_empty(), "被擋下就不該推通知");

        // 2) 宣告了身分卻證明不了：403（`verified_bot_id` 丟的），也不寫。
        let mut forged = HeaderMap::new();
        forged.insert("X-AM-Bot-Id", plain.parse().unwrap());
        forged.insert("X-AM-Bot-Token", "nope".parse().unwrap());
        let err = put(&app, forged, "冒名塞進來的指示").await.unwrap_err();
        assert!(matches!(&err, LcError::Forbidden(v) if v["reason"] == "bot_proof_mismatch"), "got {err:?}");
        assert!(store::get_or_init(&app.db).await.unwrap().persona_text.is_none());

        // 3) 沒宣告身分＝使用者／UI：照收（界線留在 #447），但要留痕，記成 user、歸巡檢。
        put(&app, HeaderMap::new(), "使用者從網頁改的").await.unwrap();
        let rows = inbox(&app).await;
        assert_eq!(rows.len(), 1, "每次真的改動都要推一則");
        let p: Value = serde_json::from_str(&rows[0].0).unwrap();
        assert_eq!(p["actor"], "user");
        assert_eq!(p["verified"], json!(false));
        assert_eq!(p["how"], "api");
        assert_eq!(p["old_bytes"], json!(0), "第一次沒有舊文字");
        assert_ne!(p["new_hash"], p["old_hash"], "diff 摘要要看得出換了");
        crate::supervisor::roles::classify(&app.db).await.unwrap();
        assert_eq!(inbox(&app).await[0].1, "patrol", "persona_changed 歸巡檢");

        // 4) 角色 bot：照收，記成 AGM:<role>。
        put(&app, agm, "協調者改的").await.unwrap();
        let rows = inbox(&app).await;
        assert_eq!(rows.len(), 2);
        let p: Value = serde_json::from_str(&rows[1].0).unwrap();
        assert_eq!(p["actor"], format!("{}:patrol", store::SUPERVISOR_ID));
        assert_eq!(p["verified"], json!(true));
        assert_eq!(p["old_bytes"], json!("使用者從網頁改的".len()), "舊文字的長度要對");

        // 5) 同一段文字再送一次（修 projection）：不是改動，不該再叫醒巡檢一次。
        put(&app, HeaderMap::new(), "協調者改的").await.unwrap();
        assert_eq!(inbox(&app).await.len(), 2, "重送同一段文字不該再推一則");

        app.db.close().await;
        std::fs::remove_dir_all(&app.data_dir).unwrap();
    }

    /// issue #463：`post_remote_observation` 的 `actor` 以前整個由 body 決定，而它的 doc 卻寫
    /// 「refuses an anonymous caller」——擋的只有空字串。照 #414 的形狀：驗過才寫得出 `AGM:<role>`。
    #[tokio::test]
    async fn a_remote_observation_records_who_it_really_came_from() {
        let app = app().await;
        let obs = |app: &Arc<App>, h: HeaderMap, actor: Option<&str>| {
            let (app, actor) = (app.clone(), actor.map(str::to_string));
            async move {
                let mut body = json!({"status": "requested", "source": "manual"});
                if let Some(a) = actor {
                    body["actor"] = json!(a);
                }
                post_remote_observation(State(app), h, Json(serde_json::from_value(body).unwrap())).await
            }
        };
        let recorded = |app: &Arc<App>| {
            let app = app.clone();
            async move {
                sqlx::query_scalar::<_, Option<String>>("SELECT remote_actor FROM supervisors WHERE id=?")
                    .bind(store::SUPERVISOR_ID)
                    .fetch_one(&app.db)
                    .await
                    .unwrap()
            }
        };

        // 沒驗過卻自稱是總管：包成 user(...)，變不成 AGM 開頭。
        obs(&app, HeaderMap::new(), Some("AGM:responder")).await.unwrap();
        let who = recorded(&app).await.unwrap_or_default();
        assert_eq!(who, "user(AGM:responder)");
        assert!(!who.starts_with(store::SUPERVISOR_ID), "未驗證的呼叫端寫不出 AGM 開頭的身分，卻拿到 {who:?}");

        // 驗過的角色：以 token 為準，body 的自稱不影響。
        let agm = agm_role_headers(&app).await;
        obs(&app, agm, Some("我是別人")).await.unwrap();
        assert_eq!(recorded(&app).await.unwrap_or_default(), format!("{}:patrol", store::SUPERVISOR_ID));

        app.db.close().await;
        std::fs::remove_dir_all(&app.data_dir).unwrap();
    }

    /// 第一個裁示定案。後到的 deny 不能把 approved 翻成 denied——那會讓同一筆核准有兩個
    /// 「第一次決定」，而執行端可能已經拿著 approved 去建置了。
    /// HTTP 這一層的契約：重送同一個 request id 回同一筆、`created` 分得出來、
    /// 不會再推一次 `approval_requested`（AGM 不該為同一件事被叫醒兩次）。
    /// owner 與 fence 是公開欄位（`lease status` 就看得到），只靠它們等於誰都能把別人正在換
    /// binary 的窗口收掉——那一刻正好最不能被打斷（review 2026-09-16）。
    #[tokio::test]
    async fn only_the_holder_can_give_a_maintenance_window_back() {
        let app = app().await;
        let ap = store::create_approval(&app.db, "runner", "rebuild", "daemon", None, None, None).await.unwrap().approval;
        store::decide_approval(&app.db, &ap.id, "approved", "AGM", None, None).await.unwrap();
        let until = iso_in(900);
        let lease = store::acquire_lease(&app.db, "rebuild", "runner", Some(&ap.id), None, &until, false, None, &json!({})).await.unwrap().unwrap();
        let token = lease.lease_token.clone().expect("acquire 要發一把憑證");
        let held = || async { store::lease(&app.db, "rebuild").await.unwrap().unwrap() };
        let body = |v: serde_json::Value| -> LeaseHolderIn { serde_json::from_value(v).unwrap() };

        // 什麼都不帶：403，租約一個字都不動。
        let err = post_lease_release(State(app.clone()), Path("rebuild".into()), HeaderMap::new(), Json(body(json!({"owner": "runner", "fence": lease.fence}))))
            .await
            .unwrap_err();
        let LcError::Forbidden(v) = &err else { panic!("expected 403, got {err:?}") };
        assert_eq!(v["reason"], "lease_token_required");
        assert!(held().await.released_at.is_none(), "被擋下來就不該動到租約");

        // 帶錯的：一樣 403。
        let err = post_lease_release(
            State(app.clone()),
            Path("rebuild".into()),
            HeaderMap::new(),
            Json(body(json!({"owner": "runner", "fence": lease.fence, "lease_token": "0".repeat(token.len())}))),
        )
        .await
        .unwrap_err();
        let LcError::Forbidden(v) = &err else { panic!("expected 403, got {err:?}") };
        assert_eq!(v["reason"], "lease_token_mismatch");
        assert!(held().await.released_at.is_none());
        assert_eq!(store::approval(&app.db, &ap.id).await.unwrap().unwrap().status, "approved", "核准也不該被消耗");

        // 帶對的：成功。
        let Json(out) = post_lease_release(
            State(app.clone()),
            Path("rebuild".into()),
            HeaderMap::new(),
            Json(body(json!({"owner": "runner", "fence": lease.fence, "lease_token": token}))),
        )
        .await
        .unwrap();
        assert_eq!(out["released"], true);
        assert!(held().await.released_at.is_some());
        assert_eq!(store::approval(&app.db, &ap.id).await.unwrap().unwrap().status, "consumed");
        app.db.close().await;
        std::fs::remove_dir_all(&app.data_dir).unwrap();
    }

    /// 巡檢角色的呼叫端：`X-AM-Bot-Id` + 那顆 bot 自己的 hook token（CLI 在角色的 pane 裡跑，
    /// 環境本來就有這兩個值）。模型打出來的字串冒充不了。
    /// issue #415：`X-AM-Bot-Id` 帶了卻證明不了，以前回 `None`，而 `None` 在 `roles::ack` 是
    /// `1=1`＝使用者本人。於是巡檢只要把自己的 token 打壞，就結得掉協調者的事件——帶對反而
    /// 被 409 擋下。降級同時是提權，所以現在整個請求 403。
    #[tokio::test]
    async fn a_claimed_bot_identity_that_cannot_be_proven_is_refused_not_downgraded_to_the_user() {
        let app = app().await;
        let good = agm_role_headers(&app).await; // 巡檢角色
        let bot_id = good.get("X-AM-Bot-Id").unwrap().to_str().unwrap().to_string();
        // 協調者要先建立起來，跨角色才成立：沒有協調者時，協調的事件本來就是巡檢在收
        // （`roles::ack` 的 `Patrol if !responder_configured`），那一格不是這條測試要問的。
        let responder = crate::db::ulid();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES (?,'p-agm','AGM-responder','claude','resp-tok',?)")
            .bind(&responder)
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .unwrap();
        crate::supervisor::roles::set_env(&app.db, crate::supervisor::roles::Role::Responder, &responder, "p-agm", "/tmp").await.unwrap();
        assert!(crate::supervisor::roles::responder_configured(&app.db).await.unwrap(), "前提：協調者已建立");

        // 一筆歸協調者的事件。
        let ev = |app: &Arc<App>| {
            let app = app.clone();
            async move {
                let id = store::push_inbox(&app.db, &format!("k-{}", crate::db::ulid()), "approval_requested", None, None, None, &json!({}))
                    .await
                    .unwrap()
                    .unwrap();
                sqlx::query("UPDATE supervisor_inbox SET role='responder' WHERE id=?").bind(&id).execute(&app.db).await.unwrap();
                id
            }
        };
        let state = |app: &Arc<App>, id: String| {
            let app = app.clone();
            async move {
                sqlx::query_scalar::<_, String>("SELECT state FROM supervisor_inbox WHERE id=?")
                    .bind(&id)
                    .fetch_one(&app.db)
                    .await
                    .unwrap()
            }
        };

        // 1) 帶對 token：還是只能結自己角色的，409（既有行為，別退化）。
        let e1 = ev(&app).await;
        let err = post_inbox_ack(State(app.clone()), Path(e1.clone()), good.clone()).await.unwrap_err();
        let LcError::Conflict(v) = &err else { panic!("expected 409, got {err:?}") };
        assert_eq!(v["reason"], "claimed_by_other_role");
        assert_eq!(state(&app, e1.clone()).await, "pending", "被擋下就不該動到事件");

        // 2) 同一顆 bot、token 打錯：403，不是「當成使用者」。
        for bad in ["nope", ""] {
            let mut h = HeaderMap::new();
            h.insert("X-AM-Bot-Id", bot_id.parse().unwrap());
            h.insert("X-AM-Bot-Token", bad.parse().unwrap());
            let e = ev(&app).await;
            let err = post_inbox_ack(State(app.clone()), Path(e.clone()), h).await.unwrap_err();
            let LcError::Forbidden(v) = &err else { panic!("token={bad:?} 應該 403，卻是 {err:?}") };
            assert_eq!(v["reason"], "bot_proof_mismatch", "token={bad:?}");
            assert_eq!(state(&app, e).await, "pending", "token={bad:?}：擋下來就不該結掉別人的事件");
        }

        // 3) 完全不帶 token 但宣告了 bot id：一樣證明不了。
        let mut only_id = HeaderMap::new();
        only_id.insert("X-AM-Bot-Id", bot_id.parse().unwrap());
        let e3 = ev(&app).await;
        let err = post_inbox_ack(State(app.clone()), Path(e3.clone()), only_id).await.unwrap_err();
        assert!(matches!(&err, LcError::Forbidden(v) if v["reason"] == "bot_proof_mismatch"), "got {err:?}");
        assert_eq!(state(&app, e3).await, "pending");

        // 4) 什麼身分都沒宣告：以前是「使用者／UI，什麼都結得掉」，issue #432 之後也是 403
        //    ——不宣告曾經比宣告了卻證明不了更有權限，那個反差正是要消掉的東西。
        let e4 = ev(&app).await;
        let err = post_inbox_ack(State(app.clone()), Path(e4.clone()), HeaderMap::new()).await.unwrap_err();
        let LcError::Forbidden(v) = &err else { panic!("沒宣告身分應該 403，卻是 {err:?}") };
        assert_eq!(v["reason"], "role_required");
        assert_eq!(state(&app, e4).await, "pending", "擋下來就不該動到事件");

        // 5) 驗過、但那顆不是角色 bot：跟沒宣告一樣結不掉（`actor_role` 回 Ok(None)）。
        let plain = crate::db::ulid();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES (?,'p-agm','plain','claude','plain-tok',?)")
            .bind(&plain)
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let mut plain_h = HeaderMap::new();
        plain_h.insert("X-AM-Bot-Id", plain.parse().unwrap());
        plain_h.insert("X-AM-Bot-Token", "plain-tok".parse().unwrap());
        let e5 = ev(&app).await;
        let err = post_inbox_ack(State(app.clone()), Path(e5.clone()), plain_h).await.unwrap_err();
        assert!(matches!(&err, LcError::Forbidden(v) if v["reason"] == "role_required"), "got {err:?}");
        assert_eq!(state(&app, e5).await, "pending");

        app.db.close().await;
        std::fs::remove_dir_all(&app.data_dir).unwrap();
    }

    /// issue #414：`decided_by` 以前是 body 說了算，預設還填 `AGM`。UI token 本機任何行程都
    /// 拿得到，所以那等於任何一顆 bot 都能把自己的裁示掛成 `AGM:responder`。
    #[tokio::test]
    async fn only_a_verified_role_can_be_recorded_as_the_supervisor() {
        let app = app().await;
        let decided_by = |app: &Arc<App>, id: String| {
            let app = app.clone();
            async move { store::approval(&app.db, &id).await.unwrap().unwrap().decided_by.unwrap_or_default() }
        };

        // 沒驗過卻自稱是協調者：記成 user(...)，絕不是 AGM 開頭。
        let a1 = pending(&app).await;
        decide(&app, &a1, "approve", "AGM:responder").await.unwrap();
        let who = decided_by(&app, a1).await;
        assert_eq!(who, "user(AGM:responder)");
        assert!(!who.starts_with(store::SUPERVISOR_ID), "未驗證的呼叫端寫不出 AGM 開頭的身分，卻拿到 {who:?}");

        // 沒驗過、也沒自稱：user，而不是以前的 AGM。
        let a2 = pending(&app).await;
        let body: DecisionIn = serde_json::from_value(json!({"decision": "approve"})).unwrap();
        post_approval_decision(State(app.clone()), Path(a2.clone()), HeaderMap::new(), Json(body)).await.unwrap();
        assert_eq!(decided_by(&app, a2).await, "user");

        // 驗過的角色：以 token 為準，body 自稱的名字不影響。
        let a3 = pending(&app).await;
        let h = agm_role_headers(&app).await;
        let body: DecisionIn = serde_json::from_value(json!({"decision": "approve", "actor": "AGM:responder"})).unwrap();
        post_approval_decision(State(app.clone()), Path(a3.clone()), h, Json(body)).await.unwrap();
        assert_eq!(decided_by(&app, a3).await, format!("{}:patrol", store::SUPERVISOR_ID));

        app.db.close().await;
        std::fs::remove_dir_all(&app.data_dir).unwrap();
    }

    /// issue #436：`requester` 以前是 body 說了算（`post_approval` 連 headers 都不收）。#414 之後
    /// 「誰裁示」已經只有驗過的角色寫得出 `AGM:<role>`，「誰申請」卻還是自由填——同一筆紀錄一半可信
    /// 一半不可信，而 `decide` 想擋「自己核准自己」也沒有可信的申請人可比。
    ///
    /// 注意這裡**不改寫** `requester`：它之後要跟 `lease acquire --owner` 逐字相等
    /// （`maintenance.rs` 的 `approval_owner_mismatch`），改寫等於讓申請人後面開不了窗口。
    #[tokio::test]
    async fn an_approval_records_whether_its_requester_was_proven() {
        let app = app().await;
        let ask = |app: &Arc<App>, h: HeaderMap, who: &str, rid: &str| {
            let (app, who, rid) = (app.clone(), who.to_string(), rid.to_string());
            async move {
                let body: ApprovalIn =
                    serde_json::from_value(json!({"requester": who, "purpose": "rebuild", "scope": "daemon", "request_id": rid})).unwrap();
                post_approval(State(app), h, Json(body)).await
            }
        };
        let row = |app: &Arc<App>, id: String| {
            let app = app.clone();
            async move { store::approval(&app.db, &id).await.unwrap().unwrap() }
        };

        // 1) 沒宣告身分：照收（`daemon-update-kick.sh` 這類 launchd 腳本就是這樣申請的），
        //    `requester` 一個字都不動，但記成沒驗過。
        let Json(a1) = ask(&app, HeaderMap::new(), "daemon-update-kick", "r-1").await.unwrap();
        let a1 = row(&app, a1["id"].as_str().unwrap().to_string()).await;
        assert_eq!(a1.requester, "daemon-update-kick", "自稱的名字要原樣留著，lease 的 --owner 靠它對");
        assert_eq!(a1.requester_unverified, 1);
        assert_eq!(a1.to_json()["requester_unverified"], json!(true), "API 要看得出來沒驗過");

        // 2) 驗過、而且申請人就是自己（這裡用 bot 名；bot id 與 agent 名一樣認得）：記成驗過。
        let h = agm_role_headers(&app).await;
        let Json(a2) = ask(&app, h.clone(), "AGM", "r-2").await.unwrap();
        let a2 = row(&app, a2["id"].as_str().unwrap().to_string()).await;
        assert_eq!((a2.requester.as_str(), a2.requester_unverified), ("AGM", 0));

        // 3) 驗過、卻用別人的名義申請：403，不是默默改寫成自己。
        let err = ask(&app, h.clone(), "somebody-else", "r-3").await.unwrap_err();
        let LcError::Forbidden(v) = &err else { panic!("expected 403, got {err:?}") };
        assert_eq!(v["reason"], "requester_not_the_caller");
        assert!(store::approval_by_request(&app.db, "r-3").await.unwrap().is_none(), "擋下來就不該留下半筆申請");

        // 4) 宣告了身分卻證明不了：照 #415 是 403 `bot_proof_mismatch`，不是退回「使用者申請」。
        let mut bad = HeaderMap::new();
        bad.insert("X-AM-Bot-Id", h.get("X-AM-Bot-Id").unwrap().clone());
        bad.insert("X-AM-Bot-Token", "nope".parse().unwrap());
        let err = ask(&app, bad, "AGM", "r-4").await.unwrap_err();
        assert!(matches!(&err, LcError::Forbidden(v) if v["reason"] == "bot_proof_mismatch"), "got {err:?}");
        assert!(store::approval_by_request(&app.db, "r-4").await.unwrap().is_none());

        app.db.close().await;
        std::fs::remove_dir_all(&app.data_dir).unwrap();
    }

    /// issue #436：核准是換 binary／重啟窗口的授權來源，申請與裁示要是兩個人。
    /// 只擋 `approve`——把自己送出去的申請 `deny` 掉是收回申請，本來就該讓他做。
    #[tokio::test]
    async fn nobody_approves_the_window_they_asked_for_themselves() {
        let app = app().await;
        let h = agm_role_headers(&app).await;
        let ask = |app: &Arc<App>, h: HeaderMap, who: &str, rid: &str| {
            let (app, who, rid) = (app.clone(), who.to_string(), rid.to_string());
            async move {
                let body: ApprovalIn =
                    serde_json::from_value(json!({"requester": who, "purpose": "rebuild", "scope": "daemon", "request_id": rid})).unwrap();
                let Json(a) = post_approval(State(app), h, Json(body)).await.unwrap();
                a["id"].as_str().unwrap().to_string()
            }
        };
        let decide_as = |app: &Arc<App>, id: String, h: HeaderMap, what: &str| {
            let (app, what) = (app.clone(), what.to_string());
            async move {
                let body: DecisionIn = serde_json::from_value(json!({"decision": what})).unwrap();
                post_approval_decision(State(app), Path(id), h, Json(body)).await
            }
        };
        let status = |app: &Arc<App>, id: String| {
            let app = app.clone();
            async move { store::approval(&app.db, &id).await.unwrap().unwrap().status }
        };

        // 自己申請、自己核准：擋下來，而且那筆還是 pending。
        let mine = ask(&app, h.clone(), "AGM", "s-1").await;
        let err = decide_as(&app, mine.clone(), h.clone(), "approve").await.unwrap_err();
        let LcError::Forbidden(v) = &err else { panic!("expected 403, got {err:?}") };
        assert_eq!(v["reason"], "self_approval_forbidden");
        assert_eq!(status(&app, mine.clone()).await, "pending", "擋下來就不該動到那筆");

        // 同一個人把自己的申請收回（deny）照舊可以。
        decide_as(&app, mine.clone(), h.clone(), "deny").await.unwrap();
        assert_eq!(status(&app, mine).await, "denied");

        // 別人送的申請照常裁示得了。
        let theirs = ask(&app, HeaderMap::new(), "fixer", "s-2").await;
        decide_as(&app, theirs.clone(), h.clone(), "approve").await.unwrap();
        assert_eq!(status(&app, theirs).await, "approved");

        // 申請時不帶身分、裁示時才帶自己的——守衛不能因此變成選配的（i406 的審核，2026-09-24 裁示）：
        // 只要 `requester` 解析出來就是這個呼叫端，照擋。
        let side_door = ask(&app, HeaderMap::new(), "AGM", "s-3").await;
        let err = decide_as(&app, side_door.clone(), h, "approve").await.unwrap_err();
        assert!(matches!(&err, LcError::Forbidden(v) if v["reason"] == "self_approval_forbidden"), "got {err:?}");
        assert_eq!(status(&app, side_door).await, "pending", "空 header 申請、自己核准，一樣不該過");

        app.db.close().await;
        std::fs::remove_dir_all(&app.data_dir).unwrap();
    }

    /// issue #436（i264 的審核）：守衛「查不出申請人是誰就不敢核准」這條路本來沒有測到——
    /// `requester_lookup_failed` 在整個 daemon 只出現在產生它的那一行，`try_requester_bot_id` 的 `Err`
    /// 分支從沒被走過。以後有人把它改回 `requester_bot_id`（把讀取失敗吞成 `None`），洞會無聲回來而全套照綠。
    ///
    /// 這裡只弄壞 `runs`：`requester` 是 agent 名那一段才查它（`try_requester_bot_id` 的最後一步），
    /// 呼叫端自己的身分驗證只讀 `bots`，所以壞的剛好是「查申請人」這一步——403 與 503 分得開。
    #[tokio::test]
    async fn an_unreadable_db_refuses_the_approval_instead_of_guessing_it_is_someone_else() {
        let app = app().await;
        let h = agm_role_headers(&app).await;
        // 申請人是個解析不到 bot 的名字：DB 正常時 `Ok(None)`＝不是自我核准，照常放行。
        let body: ApprovalIn =
            serde_json::from_value(json!({"requester": "some-agent-name", "purpose": "rebuild", "scope": "daemon", "request_id": "u-1"}))
                .unwrap();
        let Json(a) = post_approval(State(app.clone()), HeaderMap::new(), Json(body)).await.unwrap();
        let id = a["id"].as_str().unwrap().to_string();

        sqlx::query("DROP TABLE runs").execute(&app.db).await.unwrap();
        let decision: DecisionIn = serde_json::from_value(json!({"decision": "approve"})).unwrap();
        let err = post_approval_decision(State(app.clone()), Path(id.clone()), h, Json(decision)).await.unwrap_err();
        let LcError::Unavailable(v) = &err else { panic!("讀不到就不該猜『不是它』，要回 503：{err:?}") };
        assert_eq!(v["reason"], "requester_lookup_failed");
        assert_eq!((v["retryable"].as_bool(), v["sent"].as_bool()), (Some(true), Some(false)));
        assert_eq!(store::approval(&app.db, &id).await.unwrap().unwrap().status, "pending", "沒查清楚就不能動那筆");

        app.db.close().await;
        std::fs::remove_dir_all(&app.data_dir).unwrap();
    }

    pub(super) async fn agm_role_headers(app: &Arc<App>) -> HeaderMap {
        let id = crate::db::ulid();
        sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p-agm','/tmp','AGM',?)")
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .ok();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES (?,'p-agm','AGM','claude','agm-tok',?)")
            .bind(&id)
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .unwrap();
        store::set_env(&app.db, &id, "p-agm", "/tmp").await.unwrap();
        crate::supervisor::roles::set_env(&app.db, crate::supervisor::roles::Role::Patrol, &id, "p-agm", "/tmp").await.unwrap();
        let mut h = HeaderMap::new();
        h.insert("X-AM-Bot-Id", id.parse().unwrap());
        h.insert("X-AM-Bot-Token", "agm-tok".parse().unwrap());
        h
    }

    /// 強制釋放是可以做的事（持有者不在了），但要留下是誰、為什麼；沒理由就不准。
    #[tokio::test]
    async fn a_forced_release_needs_a_reason_and_leaves_a_note() {
        let app = app().await;
        let ap = store::create_approval(&app.db, "runner", "rebuild", "daemon", None, None, None).await.unwrap().approval;
        store::decide_approval(&app.db, &ap.id, "approved", "AGM", None, None).await.unwrap();
        let until = iso_in(900);
        let lease = store::acquire_lease(&app.db, "rebuild", "runner", Some(&ap.id), None, &until, false, None, &json!({})).await.unwrap().unwrap();
        let body = |v: serde_json::Value| -> LeaseHolderIn { serde_json::from_value(v).unwrap() };

        // 一般呼叫端（沒有角色）就算附了理由也不能接管：force 是授權問題，不只是稽核問題。
        let err = post_lease_release(
            State(app.clone()),
            Path("rebuild".into()),
            HeaderMap::new(),
            Json(body(json!({"owner": "someone-else", "fence": lease.fence, "force": true, "reason": "我想收掉"}))),
        )
        .await
        .unwrap_err();
        let LcError::Forbidden(v) = &err else { panic!("expected 403, got {err:?}") };
        assert_eq!(v["reason"], "lease_force_forbidden");
        assert!(store::lease(&app.db, "rebuild").await.unwrap().unwrap().released_at.is_none(), "被擋下就不該動到租約");

        let agm = agm_role_headers(&app).await;
        let err = post_lease_release(
            State(app.clone()),
            Path("rebuild".into()),
            agm.clone(),
            Json(body(json!({"owner": "agm", "fence": lease.fence, "force": true}))),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, LcError::Bad(_)), "force 沒有理由就不准：{err:?}");
        assert!(store::lease(&app.db, "rebuild").await.unwrap().unwrap().released_at.is_none());

        let Json(out) = post_lease_release(
            State(app.clone()),
            Path("rebuild".into()),
            agm,
            Json(body(json!({"owner": "agm", "fence": lease.fence, "force": true, "reason": "持有者的 pane 不在了"}))),
        )
        .await
        .unwrap();
        assert_eq!(out["released"], true);
        let note: String = sqlx::query_scalar("SELECT body FROM supervisor_notes WHERE kind='lease_force_release'")
            .fetch_one(&app.db)
            .await
            .unwrap();
        let note: Value = serde_json::from_str(&note).unwrap();
        assert_eq!(note["reason"], "持有者的 pane 不在了");
        assert_eq!(note["by_role"], "patrol", "稽核紀錄要記下是哪個角色做的");
        app.db.close().await;
        std::fs::remove_dir_all(&app.data_dir).unwrap();
    }

    /// 升級當下已經握在手上的舊租約沒有憑證可出示：擋死的話那個窗口永遠沒人還得了。
    #[tokio::test]
    async fn a_lease_taken_before_tokens_existed_can_still_be_returned() {
        let app = app().await;
        let ap = store::create_approval(&app.db, "runner", "rebuild", "daemon", None, None, None).await.unwrap().approval;
        store::decide_approval(&app.db, &ap.id, "approved", "AGM", None, None).await.unwrap();
        let until = iso_in(900);
        let lease = store::acquire_lease(&app.db, "rebuild", "runner", Some(&ap.id), None, &until, false, None, &json!({})).await.unwrap().unwrap();
        // 升級前的那一列：token 是 NULL。
        sqlx::query("UPDATE supervisor_leases SET lease_token=NULL WHERE resource='rebuild'").execute(&app.db).await.unwrap();

        let body: LeaseHolderIn = serde_json::from_value(json!({"owner": "runner", "fence": lease.fence})).unwrap();
        let Json(out) = post_lease_release(State(app.clone()), Path("rebuild".into()), HeaderMap::new(), Json(body)).await.unwrap();
        assert_eq!(out["released"], true, "舊租約要還得了");
        app.db.close().await;
        std::fs::remove_dir_all(&app.data_dir).unwrap();
    }

    /// 申請與叫醒 AGM 的 `approval_requested` 同一個交易：通知寫不進去就整筆不成立（回錯、沒有 approval 列），
    /// 申請者重送再來。以前先建 approval、再 `let _` 推事件；寫不進去時 approval 停在 pending、AGM 不知道要裁示，
    /// 重送走冪等分支（`created=false`）又不會補推。
    #[tokio::test]
    async fn an_approval_and_its_notification_land_together() {
        let app = app().await;
        let ask = || ApprovalIn {
            reason: None,
            requester: "k8bw2f".into(),
            purpose: "restart".into(),
            scope: "daemon".into(),
            target_commit: Some("ca7b22d".into()),
            expires_in_secs: None,
            request_id: Some("restart-atomic".into()),
            supersedes: None,
        };
        sqlx::query("CREATE TRIGGER no_approval_event BEFORE INSERT ON supervisor_inbox WHEN NEW.kind='approval_requested' BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END")
            .execute(&app.db)
            .await
            .unwrap();
        assert!(post_approval(State(app.clone()), HeaderMap::new(), Json(ask())).await.is_err(), "通知寫不進去：回錯讓申請者重送");
        assert!(store::approvals(&app.db, 10).await.unwrap().is_empty(), "沒有留下一筆沒人知道的 pending");

        sqlx::query("DROP TRIGGER no_approval_event").execute(&app.db).await.unwrap();
        let Json(ok) = post_approval(State(app.clone()), HeaderMap::new(), Json(ask())).await.unwrap();
        assert_eq!(ok["created"], true);
        let events: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE kind='approval_requested'").fetch_one(&app.db).await.unwrap();
        assert_eq!(events, 1);
    }

    #[tokio::test]
    async fn resending_one_approval_request_id_returns_the_same_row() {
        let app = app().await;
        let ask = |rid: Option<&str>, commit: &str| ApprovalIn {
            reason: None,
            requester: "k8bw2f".into(),
            purpose: "restart".into(),
            scope: "daemon".into(),
            target_commit: Some(commit.into()),
            expires_in_secs: None,
            request_id: rid.map(str::to_string),
            supersedes: None,
        };
        let Json(first) = post_approval(State(app.clone()), HeaderMap::new(), Json(ask(Some("restart-ca7b22d"), "ca7b22d"))).await.unwrap();
        assert_eq!(first["created"], true);
        let Json(again) = post_approval(State(app.clone()), HeaderMap::new(), Json(ask(Some("restart-ca7b22d"), "ca7b22d"))).await.unwrap();
        assert_eq!(again["created"], false);
        assert_eq!(again["id"], first["id"]);
        assert_eq!(store::approvals(&app.db, 10).await.unwrap().len(), 1);
        let events: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE kind='approval_requested'").fetch_one(&app.db).await.unwrap();
        assert_eq!(events, 1, "重送不再叫醒 AGM 一次");

        // 同 id 換 commit：409，原本那筆不動。
        let err = post_approval(State(app.clone()), HeaderMap::new(), Json(ask(Some("restart-ca7b22d"), "deadbee"))).await.unwrap_err();
        let LcError::Conflict(detail) = &err else { panic!("expected 409, got {err:?}") };
        assert_eq!(detail["reason"], "approval_request_mismatch");
        assert_eq!(detail["field"], "target_commit");
        assert_eq!(detail["approval_id"], first["id"]);
        assert_eq!(store::approvals(&app.db, 10).await.unwrap().len(), 1);

        // 另一顆 bot 撞上同一個自然 id：不能拿回別人的那一筆（review2 sup 沒把握 3）。
        let mut other = ask(Some("restart-ca7b22d"), "ca7b22d");
        other.requester = "someone-else".into();
        let err = post_approval(State(app.clone()), HeaderMap::new(), Json(other)).await.unwrap_err();
        let LcError::Conflict(detail) = &err else { panic!("expected 409, got {err:?}") };
        assert_eq!((detail["reason"].as_str(), detail["field"].as_str()), (Some("approval_request_mismatch"), Some("requester")));
        assert_eq!(store::approvals(&app.db, 10).await.unwrap().len(), 1);

        // 不帶 id 照舊：每次都是新的一筆。
        let Json(a) = post_approval(State(app.clone()), HeaderMap::new(), Json(ask(None, "ca7b22d"))).await.unwrap();
        let Json(b) = post_approval(State(app.clone()), HeaderMap::new(), Json(ask(None, "ca7b22d"))).await.unwrap();
        assert_ne!(a["id"], b["id"]);
        assert_eq!(a["client_request_id"], serde_json::Value::Null);
        app.db.close().await;
        std::fs::remove_dir_all(&app.data_dir).unwrap();
    }

    #[tokio::test]
    async fn a_second_opposite_decision_is_refused_not_written() {
        let app = app().await;
        let id = pending(&app).await;
        assert_eq!(decide(&app, &id, "approve", "AGM:patrol").await.unwrap().0["status"], "approved");
        let err = decide(&app, &id, "deny", "AGM:responder").await.unwrap_err();
        match err {
            LcError::Conflict(v) => {
                assert_eq!(v["reason"], "already_decided");
                assert_eq!(v["status"], "approved");
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
        assert_eq!(store::approval(&app.db, &id).await.unwrap().unwrap().status, "approved", "沒有被改寫");
    }

    /// 兩個角色同時決定：只有一個寫得進去，另一個拿到 409，狀態是贏的那個。
    #[tokio::test]
    async fn concurrent_approve_and_deny_leave_exactly_one_ruling() {
        let app = app().await;
        let id = pending(&app).await;
        let (a, b) = tokio::join!(decide(&app, &id, "approve", "AGM:patrol"), decide(&app, &id, "deny", "AGM:responder"));
        let winners = [&a, &b].iter().filter(|r| r.is_ok()).count();
        assert_eq!(winners, 1, "只能有一個裁示：{a:?} / {b:?}");
        let now = store::approval(&app.db, &id).await.unwrap().unwrap();
        assert!(now.status == "approved" || now.status == "denied");
        let winner_status = if a.is_ok() { "approved" } else { "denied" };
        assert_eq!(now.status, winner_status);
    }

    /// 翻案要明講 revoke，而且歷程留著（approvals 那一列只有最後一個狀態）。
    #[tokio::test]
    async fn revoking_an_approval_keeps_the_whole_history() {
        let app = app().await;
        let id = pending(&app).await;
        decide(&app, &id, "approve", "AGM:responder").await.unwrap();
        let out = decide(&app, &id, "revoke", "AGM:patrol").await.unwrap().0;
        assert_eq!(out["status"], "revoked");
        assert_eq!(out["decided_from"], "approved");
        let listed = get_approvals(State(app.clone()), Query(ApprovalQuery::default())).await.unwrap().0;
        let decisions = listed["approvals"][0]["decisions"].as_array().cloned().unwrap_or_default();
        let pairs: Vec<(String, String)> = decisions
            .iter()
            .map(|d| (d["from"].as_str().unwrap_or("").into(), d["to"].as_str().unwrap_or("").into()))
            .collect();
        assert_eq!(pairs, vec![("pending".to_string(), "approved".to_string()), ("approved".into(), "revoked".into())]);
        // `decide` 走的是沒有 bot 標頭的呼叫端，所以自稱的 `AGM:responder` 只會被記成未驗證的
        // `user(…)`（issue #414）——歷程照樣查得到是誰說的，但讀的人一眼看得出那串字沒被驗證。
        assert_eq!(decisions[0]["actor"], "user(AGM:responder)", "誰核准的要查得到");
        assert_eq!(decisions[1]["actor"], "user(AGM:patrol)");
        // 撤銷之後不能就地再核准：要開新的一筆申請。
        assert!(decide(&app, &id, "approve", "AGM:patrol").await.is_err());
    }

    /// 稽核寫不進去時，決定本身也不能留下來：兩者同一個 transaction。否則會出現「有決定、沒紀錄」
    /// 的半套，而呼叫端重試同一個決定會撞上 idempotent 直接回成功，那筆 audit 永遠補不回來。
    #[tokio::test]
    async fn a_decision_whose_audit_cannot_be_written_is_not_written_either() {
        let app = app().await;
        let id = pending(&app).await;
        // 故障注入：稽核表不見了。
        sqlx::query("DROP TABLE supervisor_notes").execute(&app.db).await.unwrap();
        assert!(decide(&app, &id, "approve", "AGM:patrol").await.is_err(), "寫不了紀錄就不該有決定");
        assert_eq!(store::approval(&app.db, &id).await.unwrap().unwrap().status, "pending", "核准沒有被改掉");

        // 表回來之後，同一個決定照樣能做（沒有被 idempotent 擋住）。
        sqlx::query(
            "CREATE TABLE supervisor_notes (id TEXT PRIMARY KEY, supervisor_id TEXT NOT NULL, kind TEXT NOT NULL,
               body TEXT NOT NULL, version INTEGER NOT NULL DEFAULT 1, created_at TEXT NOT NULL)",
        )
        .execute(&app.db)
        .await
        .unwrap();
        assert_eq!(decide(&app, &id, "approve", "AGM:patrol").await.unwrap().0["status"], "approved");
        let history = store::approval_decisions(&app.db).await.unwrap();
        assert_eq!(history.get(&id).map(Vec::len), Some(1), "只有一筆歷程，內容對得上");
    }

    /// 重送同一個決定是冪等的（逾時重試不是新的裁示）。
    #[tokio::test]
    async fn repeating_the_same_decision_is_idempotent() {
        let app = app().await;
        let id = pending(&app).await;
        decide(&app, &id, "deny", "AGM:patrol").await.unwrap();
        let again = decide(&app, &id, "deny", "AGM:patrol").await.unwrap().0;
        assert_eq!(again["idempotent"], json!(true));
        let listed = get_approvals(State(app.clone()), Query(ApprovalQuery::default())).await.unwrap().0;
        assert_eq!(listed["approvals"][0]["decisions"].as_array().unwrap().len(), 1, "重試不留第二筆歷程");
    }
}

#[cfg(test)]
mod review_boundary_tests {
    use super::*;

    async fn app() -> Arc<App> {
        let dir = std::env::temp_dir().join(format!("agm-review-boundary-{}", crate::db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = crate::db::open(&dir.join("test.sqlite")).await.unwrap();
        let cfg = crate::config::ConfigStore::load(dir.join("config.toml")).await.unwrap();
        let client = crate::herdr::HerdrClient::new(dir.join("absent.sock"));
        let app = App::new(db, client.clone(), client, cfg, dir.clone(), dir.join("daemon"),
                           7799, "test".into(), "test".into(), false);
        store::get_or_init(&app.db).await.unwrap();
        app
    }

    /// #334：同一個 client_request_id、同 text、同 bot，但 notice／ownership／mission／review_role 不同——
    /// 不能靜默回原筆 200（呼叫端的新意圖被吃掉）；完全一樣的重送照舊回原筆。
    #[tokio::test]
    async fn a_retry_that_changes_the_settings_is_refused_not_silently_answered_with_the_original() {
        let app = app().await;
        store::insert_assignment(&app.db, None, "bot", "crid-1", "do the thing", &["a/b".to_string()], None, true).await.unwrap();
        let go = |expects_review: bool, own: &[String], mission: Option<(&str, &str)>, role: Option<crate::supervisor::roles::Role>| {
            let app = app.clone();
            let own = own.to_vec();
            let mission = mission.map(|(a, b)| (a.to_string(), b.to_string()));
            async move {
                crate::supervisor::assign(
                    &app, "bot", "do the thing", "crid-1", None, &own, None, expects_review,
                    mission.as_ref().map(|(a, b)| (a.as_str(), b.as_str())), role, None, Default::default(),
                )
                .await
            }
        };
        let own = ["a/b".to_string()];
        assert!(go(true, &own, None, None).await.is_ok(), "完全一樣的重送回原筆");
        assert!(go(true, &own, None, Some(crate::supervisor::roles::Role::Responder)).await.is_ok(), "沒寫＝協調者：明講 responder 的重送是同一件");
        for (label, r) in [
            ("expects_review", go(false, &own, None, None).await),
            ("ownership", go(true, &[], None, None).await),
            ("mission", go(true, &own, Some(("m1", "executor")), None).await),
            ("review_role", go(true, &own, None, Some(crate::supervisor::roles::Role::Patrol)).await),
        ] {
            let LcError::Conflict(d) = r.expect_err(label) else { panic!("{label}: expected 409") };
            assert_eq!(d["reason"], "assignment_request_mismatch", "{label}");
            assert_eq!(d["field"], label);
        }
        app.db.close().await;
        std::fs::remove_dir_all(&app.data_dir).unwrap();
    }

    /// #515：清單有分頁，而且頁外的未結案交辦翻得到。
    ///
    /// 以前只回最新 200 筆、沒有任何「還有更多」的訊號，`bin/agm` 在客戶端過濾 `--open`，
    /// 於是頁外那些 `blocked`（＝還在等，保持未結案，天生活得比一頁久）完全看不到。
    #[tokio::test]
    async fn the_assignment_list_pages_so_the_oldest_open_work_is_still_reachable() {
        let app = app().await;
        // 同一毫秒寫兩筆：沒有 id 當第二個排序鍵的話，翻頁會漏掉或重複其中一筆。
        for i in 0..5 {
            let a = store::insert_assignment(&app.db, None, "bot", &format!("crid-{i}"), "work", &[], None, true)
                .await
                .unwrap();
            let at = format!("2026-09-{:02}T00:00:00.000Z", 10 + i / 2);
            sqlx::query("UPDATE supervisor_assignments SET created_at=?, status='blocked' WHERE id=?")
                .bind(&at)
                .bind(&a.id)
                .execute(&app.db)
                .await
                .unwrap();
        }
        let mut seen: Vec<String> = Vec::new();
        let mut before: Option<String> = None;
        loop {
            let page = get_assignments(
                State(app.clone()),
                Query(AssignmentsQuery { before: before.clone(), limit: Some(2) }),
            )
            .await
            .unwrap()
            .0;
            for a in page["assignments"].as_array().unwrap() {
                seen.push(a["id"].as_str().unwrap().to_string());
            }
            if !page["has_more"].as_bool().unwrap() {
                assert!(page["next_cursor"].is_null(), "最後一頁不給游標");
                break;
            }
            before = Some(page["next_cursor"].as_str().unwrap().to_string());
            assert!(seen.len() <= 5, "游標沒有往前走");
        }
        let mut uniq = seen.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!((seen.len(), uniq.len()), (5, 5), "五筆各出現一次：{seen:?}");
        // 第一頁就是舊行為：拿得到最新兩筆，而且明講後面還有。
        let first = get_assignments(State(app.clone()), Query(AssignmentsQuery::default())).await.unwrap().0;
        assert_eq!(first["assignments"].as_array().unwrap().len(), 5);
        assert_eq!(first["has_more"], json!(false));
        let bad = get_assignments(
            State(app.clone()),
            Query(AssignmentsQuery { before: Some("not json".into()), limit: None }),
        )
        .await
        .expect_err("壞游標要回 400，不是靜靜回第一頁");
        assert!(matches!(bad, LcError::Bad(_)), "{bad:?}");
        app.db.close().await;
        std::fs::remove_dir_all(&app.data_dir).unwrap();
    }

    /// #515：掉出那一頁的未結案交辦，handoff 也要算得到。
    #[tokio::test]
    async fn the_handoff_counts_open_work_that_fell_off_the_recent_page() {
        let app = app().await;
        // 一筆很舊、還沒結案；再塞一整頁比它新的已結案。
        let old = store::insert_assignment(&app.db, None, "bot", "crid-old", "還在等", &[], None, true).await.unwrap();
        sqlx::query("UPDATE supervisor_assignments SET created_at='2026-09-01T00:00:00.000Z', status='blocked' WHERE id=?")
            .bind(&old.id)
            .execute(&app.db)
            .await
            .unwrap();
        for i in 0..3 {
            let a = store::insert_assignment(&app.db, None, "bot", &format!("crid-{i}"), "做完了", &[], None, true).await.unwrap();
            sqlx::query("UPDATE supervisor_assignments SET created_at=?, status='completed' WHERE id=?")
                .bind(format!("2026-09-2{i}T00:00:00.000Z"))
                .bind(&a.id)
                .execute(&app.db)
                .await
                .unwrap();
        }
        // 只看最新 2 筆的話，那筆 blocked 在頁外。
        let page = store::list_assignments(&app.db, 2).await.unwrap();
        assert!(!page.iter().any(|a| a.id == old.id), "前提：它不在最新那一頁裡");
        let with_open = store::list_assignments_with_open(&app.db, 2).await.unwrap();
        assert!(with_open.iter().any(|a| a.id == old.id), "未結案的要補進來");
        assert_eq!(with_open.iter().filter(|a| a.is_open()).count(), 1);
        // 新的在前，而且補進來的那筆不會插錯位置。
        let order: Vec<&str> = with_open.iter().map(|a| a.created_at.as_str()).collect();
        let mut sorted = order.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(order, sorted, "{order:?}");

        let handoff = get_handoff(State(app.clone())).await.unwrap().0;
        assert_eq!(handoff["open_assignments"], json!(1), "少報就等於沒有人會回頭看那一筆");
        assert!(
            handoff["assignments"].as_array().unwrap().iter().any(|a| a["id"] == json!(old.id)),
            "數字有、清單沒有的話，看的人查不到是哪一筆",
        );
        app.db.close().await;
        std::fs::remove_dir_all(&app.data_dir).unwrap();
    }

    /// #335：超長的交辦內容在入口就 422，不建列、不等派送時才靜默失敗。
    #[tokio::test]
    async fn an_oversized_assignment_is_refused_up_front_and_leaves_no_row() {
        let app = app().await;
        let big = "x".repeat(crate::lifecycle::MAX_PROVABLE_CHARS + 1);
        let err = crate::supervisor::assign(&app, "bot", &big, "crid-big", None, &[], None, true, None, None, None, Default::default())
            .await
            .expect_err("too long");
        let LcError::Unprocessable(d) = err else { panic!("expected 422, got {err:?}") };
        assert_eq!(d["error"], "text_too_long");
        assert_eq!(d["sent"], false);
        assert!(store::assignment_by_crid(&app.db, "crid-big").await.unwrap().is_none());
        app.db.close().await;
        std::fs::remove_dir_all(&app.data_dir).unwrap();
    }

    #[tokio::test]
    async fn followup_replay_requires_the_same_instruction_and_request_id() {
        let app = app().await;
        let parent = store::insert_assignment(&app.db, None, "bot", "parent", "original", &[], None, true).await.unwrap();
        store::review_with_followup(&app.db, &parent.id, "queued", "followup", "AGM", "test", None, None,
            Some(store::FollowupSpec { target_bot_id: "bot", client_request_id: "follow-1", text: "continue",
                ownership: &[], request_id: None })).await.unwrap().unwrap();
        for (crid, text, should_pass) in [("follow-1", "continue", true), ("follow-2", "continue", false),
                                        ("follow-1", "different task", false)] {
            let input: ReviewIn = serde_json::from_value(json!({"decision":"followup",
                "followup_request_id":crid,"followup_text":text})).unwrap();
            let result = post_review(State(app.clone()), Path(parent.id.clone()), HeaderMap::new(), Json(input)).await;
            assert_eq!(result.is_ok(), should_pass);
            if let Err(e) = result { assert!(format!("{e:?}").contains("followup_mismatch")); }
        }
        assert_eq!(store::list_assignments(&app.db, 20).await.unwrap().len(), 2);
        assert_eq!(store::reviews(&app.db, &parent.id).await.unwrap().len(), 1);

        // 沿用別筆交辦的 request id 當續作 id：409 followup_request_id_taken，不是 502（review 2026-09-16 c1 L2）。
        let now = crate::db::now();
        sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p',?)").bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('bot','p','bot','claude','t',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        let other = store::insert_assignment(&app.db, None, "bot", "other-crid", "another", &[], None, true).await.unwrap();
        store::settle_and_notify(&app.db, &other.id, "completed", true, Some("done"), None, "k-other", "assignment_completed", &json!({}))
            .await
            .unwrap();
        let input: ReviewIn = serde_json::from_value(json!({"decision":"followup",
            "followup_request_id":"parent","followup_text":"continue again"})).unwrap();
        let err = post_review(State(app.clone()), Path(other.id.clone()), HeaderMap::new(), Json(input)).await.unwrap_err();
        match err {
            LcError::Conflict(v) => {
                assert_eq!(v["reason"], "followup_request_id_taken", "{v}");
                assert_eq!(v["assignment_id"], parent.id);
            }
            other => panic!("expected 409, got {other:?}"),
        }
        let mismatch = store::review_with_followup(&app.db, &parent.id, "superseded", "followup", "AGM", "test", None, None,
            Some(store::FollowupSpec { target_bot_id: "bot", client_request_id: "follow-1", text: "different text",
                ownership: &[], request_id: None })).await;
        assert!(mismatch.is_err(), "the transaction also rejects adopting a different instruction");
        assert_eq!(store::reviews(&app.db, &parent.id).await.unwrap().len(), 1);
        app.db.close().await;
        std::fs::remove_dir_all(&app.data_dir).unwrap();
    }

    /// cancel 撤回了還在排隊的 turn：稽核的 evidence 不能永久寫著「turn 還在跑、取消不會停 bot」，
    /// 否則之後查歷程的人（或 AGM 下一輪）會以為 bot 還在做而不敢重派（review 2026-09-16 c1 L1）。
    /// 真的已經送出去的，警告照舊留著。
    #[tokio::test]
    async fn a_cancel_that_revoked_the_queued_turn_does_not_leave_a_still_running_warning_in_the_audit() {
        let app = app().await;
        let now = crate::db::now();
        sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p',?)").bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('bot','p','bot','claude','t',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO conversations (id,bot_id,created_at) VALUES ('c1','bot',?)").bind(&now).execute(&app.db).await.unwrap();
        for (turn, status, delivery) in [("t-queued", "queued", "pending"), ("t-sent", "in_flight", "ok")] {
            sqlx::query("INSERT INTO turns (id,conversation_id,origin,status,delivery,prompt_text,created_at) VALUES (?,'c1','web',?,?,'x',?)")
                .bind(turn).bind(status).bind(delivery).bind(&now).execute(&app.db).await.unwrap();
        }
        let evidence_after_cancel = |crid: &'static str, turn: &'static str| {
            let app = app.clone();
            async move {
                let a = store::insert_assignment(&app.db, None, "bot", crid, "做 A", &[], None, true).await.unwrap();
                store::mark_delivered(&app.db, &a.id, turn, if turn == "t-queued" { "queued" } else { "ok" }).await.unwrap();
                let input: ReviewIn = serde_json::from_value(json!({"decision": "cancel", "evidence": "改派給 w2"})).unwrap();
                let out = post_review(State(app.clone()), Path(a.id.clone()), HeaderMap::new(), Json(input)).await.unwrap().0;
                let evidence = store::reviews(&app.db, &a.id).await.unwrap()[0]["evidence"].as_str().unwrap_or("").to_string();
                (out, evidence)
            }
        };

        let (out, evidence) = evidence_after_cancel("cancel-queued", "t-queued").await;
        assert_eq!(out["revoked_turn_id"], "t-queued");
        assert!(!evidence.contains("still running"), "撤回了就不是還在跑：{evidence}");
        assert!(evidence.contains("改派給 w2") && evidence.contains("已撤回"), "{evidence}");

        let (out, evidence) = evidence_after_cancel("cancel-sent", "t-sent").await;
        assert!(out.get("revoked_turn_id").is_none());
        assert!(evidence.contains("still running") && evidence.contains("改派給 w2"), "送出去的警告要留著：{evidence}");
        app.db.close().await;
        std::fs::remove_dir_all(&app.data_dir).unwrap();
    }

    /// 使用者按的暫停要擋住 `assign --mission`；daemon 自己設的那幾種不擋——runbook 要 AGM 在那些
    /// 狀態下繼續處理（交付失敗就請執行者 rebase 再交付）。
    #[test]
    fn only_a_pause_a_person_set_stops_more_work() {
        assert_eq!(user_pause_reason(Some("user_pause")), Some("user_pause"));
        assert_eq!(user_pause_reason(Some("manual")), Some("manual"));
        assert_eq!(user_pause_reason(None), None, "沒暫停就不擋");
        assert_eq!(user_pause_reason(Some("  ")), None);
        for daemon_set in ["max_rounds", "no_fable_for_verifier", "push_main_failed", "pr_failed", "clarify"] {
            assert_eq!(user_pause_reason(Some(daemon_set)), None, "{daemon_set} 是 daemon 自己設的，不擋");
        }
    }

    #[tokio::test]
    async fn safety_query_excludes_only_requested_bots_and_matches_acquire_probe() {
        let app = app().await;
        let now = crate::db::now();
        sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        for id in ["builder", "manager", "user"] {
            sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES (?,'p',?,'claude','t',?)")
                .bind(id).bind(id).bind(&now).execute(&app.db).await.unwrap();
            sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES (?,?,'running','working',?)")
                .bind(id).bind(id).bind(&now).execute(&app.db).await.unwrap();
            sqlx::query("INSERT INTO conversations (id,bot_id,created_at) VALUES (?,?,?)")
                .bind(id).bind(id).bind(&now).execute(&app.db).await.unwrap();
            sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,created_at) VALUES (?,?,?,'web','in_flight',?)")
                .bind(id).bind(id).bind(id).bind(&now).execute(&app.db).await.unwrap();
        }
        let get = |query: &str| {
            Query::<SafetyQuery>::try_from_uri(&format!("/api/supervisor/maintenance/safety{query}").parse().unwrap()).unwrap()
        };
        let all = get_maintenance_safety(State(app.clone()), get("")).await.unwrap().0;
        assert_eq!(all["safe"], false);
        assert_eq!(all["working"].as_array().unwrap().len(), 3);
        assert_eq!(all["excluded_bot_ids"], json!([]));
        let q = "?exclude=builder%2C%20manager%20%2Cbuilder%2C%2C";
        let filtered = get_maintenance_safety(State(app.clone()), get(q)).await.unwrap().0;
        assert_eq!(filtered["excluded_bot_ids"], json!(["builder", "manager"]));
        assert_eq!(filtered["safe"], false, "user work is still protected");
        assert_eq!(filtered["working"].as_array().unwrap().len(), 1);
        assert_eq!(filtered["in_flight"][0]["bot_id"], "user");
        let acquire_probe = super::super::maintenance::safety(&app, &["builder".into(), "manager".into()]).await.unwrap();
        for field in ["safe", "working", "in_flight", "unreadable"] {
            assert_eq!(filtered[field], acquire_probe[field]);
        }
        sqlx::query("UPDATE runs SET agent_status='idle' WHERE id='user'").execute(&app.db).await.unwrap();
        sqlx::query("UPDATE turns SET status='completed' WHERE id='user'").execute(&app.db).await.unwrap();
        assert_eq!(get_maintenance_safety(State(app.clone()), get(q)).await.unwrap().0["safe"], true);
        assert_eq!(get_maintenance_safety(State(app.clone()), get("")).await.unwrap().0["safe"], false);
        assert!(store::leases(&app.db).await.unwrap().is_empty(), "preflight never acquires a lease");
        app.db.close().await;
        std::fs::remove_dir_all(&app.data_dir).unwrap();
    }

    #[tokio::test]
    async fn explicitly_stopped_autostart_bot_is_not_an_outage() {
        let app = app().await;
        let now = crate::db::now();
        sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,autostart,created_at) VALUES ('b','p','b','claude','t',1,?)")
            .bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO runs (id,bot_id,state,started_at,ended_at) VALUES ('r','b','stopped',?,?)")
            .bind(&now).bind(&now).execute(&app.db).await.unwrap();
        let cfg = app.cfg.get().await;
        let thresholds = super::super::incidents::Thresholds::from_cfg(&cfg.supervisor);
        let observed = super::super::incidents::observe(&app, &thresholds).await;
        assert!(!observed.seen.iter().any(|o| o.kind == "bot_stopped"));
        sqlx::query("UPDATE runs SET state='exited' WHERE id='r'").execute(&app.db).await.unwrap();
        let observed = super::super::incidents::observe(&app, &thresholds).await;
        assert!(observed.seen.iter().any(|o| o.kind == "bot_stopped"));
        app.db.close().await;
        std::fs::remove_dir_all(&app.data_dir).unwrap();
    }

    /// force 只用在收掉持有者已經不在的窗口；AGM 角色也不能拿公開的 owner／fence 替別人延長（review2 sup #5）。
    #[tokio::test]
    async fn renew_does_not_take_force_even_from_an_agm_role() {
        let app = app().await;
        let approval = store::create_approval(&app.db, "runner", "rebuild", "test", None, None, None).await.unwrap().approval;
        store::decide_approval(&app.db, &approval.id, "approved", "AGM", None, None).await.unwrap();
        let lease = store::acquire_lease(&app.db, "rebuild", "runner", Some(&approval.id), None, &iso_in(60), false, None, &json!({})).await.unwrap().unwrap();
        let agm = super::approval_decision_tests::agm_role_headers(&app).await;
        let input: LeaseHolderIn =
            serde_json::from_value(json!({"owner": "runner", "fence": lease.fence, "ttl_secs": 3600, "force": true, "reason": "延長一下"})).unwrap();
        let err = post_lease_renew(State(app.clone()), Path("rebuild".into()), agm, Json(input)).await.unwrap_err();
        assert!(matches!(err, LcError::Bad(_)), "{err:?}");
        assert_eq!(store::lease(&app.db, "rebuild").await.unwrap().unwrap().expires_at, lease.expires_at, "一秒都沒延長");
        app.db.close().await;
        std::fs::remove_dir_all(&app.data_dir).unwrap();
    }

    #[tokio::test]
    async fn renewal_refuses_missing_and_revoked_approval_without_extending_lease() {
        let app = app().await;
        let approval = store::create_approval(&app.db, "owner", "rebuild", "test", None, None, None).await.unwrap().approval;
        store::decide_approval(&app.db, &approval.id, "approved", "AGM", None, None).await.unwrap();
        let lease = store::acquire_lease(&app.db, "rebuild", "owner", Some(&approval.id), None,
            &iso_in(60), false, None, &json!({})).await.unwrap().unwrap();
        store::decide_approval(&app.db, &approval.id, "revoked", "AGM", None, None).await.unwrap();
        for expected in ["approval_revoked", "approval_missing"] {
            let input: LeaseHolderIn = serde_json::from_value(json!({"owner":"owner","fence":lease.fence,"ttl_secs":3600})).unwrap();
            let err = post_lease_renew(State(app.clone()), Path("rebuild".into()), HeaderMap::new(), Json(input)).await.unwrap_err();
            assert!(format!("{err:?}").contains(expected));
            assert_eq!(store::lease(&app.db, "rebuild").await.unwrap().unwrap().expires_at, lease.expires_at);
            sqlx::query("DELETE FROM supervisor_approvals WHERE id=?").bind(&approval.id).execute(&app.db).await.unwrap();
        }
        app.db.close().await;
        std::fs::remove_dir_all(&app.data_dir).unwrap();
    }
}

/// issue #84：`desired_running` 是看門狗跨重啟唯一的憑據，所以它的寫入是 start／stop 的前置條件，
/// 不是順手做的副作用。寫不進去就什麼都不做並回明確的錯誤，不能回 200 讓使用者以為停好了。
#[cfg(test)]
mod desired_running_tests {
    use super::*;
    use crate::supervisor::bot_requests::flow_tests;

    /// `UPDATE supervisors SET desired_running=…` 一律失敗（磁碟滿、SQLite 鎖逾時）。
    async fn break_intent_writes(app: &Arc<App>) {
        sqlx::query(
            "CREATE TRIGGER desired_running_unwritable BEFORE UPDATE OF desired_running ON supervisors
             BEGIN SELECT RAISE(ABORT, 'database or disk is full'); END",
        )
        .execute(&app.db)
        .await
        .unwrap();
    }

    async fn wanted(app: &Arc<App>) -> i64 {
        store::get_or_init(&app.db).await.unwrap().desired_running
    }

    /// Stop：意圖寫不進去就**不要停**。舊行為是吞掉錯誤照樣停、回 200，看門狗下一個 tick 讀到的還是
    /// 「要它跑」，於是使用者剛停掉的東西自己活回來。
    #[tokio::test]
    async fn a_stop_whose_intent_write_fails_neither_stops_nor_reports_success() {
        let app = flow_tests::app().await;
        store::set_desired_running(&app.db, true).await.unwrap();
        // 停掉時會被改寫成 `unknown`：拿它當「副作用有沒有跑過」的探針。
        sqlx::query("UPDATE supervisors SET remote_status='requested' WHERE id=?")
            .bind(store::SUPERVISOR_ID)
            .execute(&app.db)
            .await
            .unwrap();
        break_intent_writes(&app).await;

        let err = post_stop(State(app.clone())).await.unwrap_err();

        let msg = format!("{err:?}");
        assert!(msg.contains("could not persist desired_running"), "錯誤要講清楚是哪一步壞了：{msg}");
        assert_eq!(wanted(&app).await, 1, "意圖沒動：看門狗讀到的還是使用者上一次的決定");
        let remote: String = sqlx::query_scalar("SELECT remote_status FROM supervisors WHERE id=?")
            .bind(store::SUPERVISOR_ID)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(remote, "requested", "停的那段副作用一步都沒跑，原樣重試是安全的");
    }

    /// Start：意圖**先寫**。啟動本身失敗（這裡是測試環境沒有 herdr）時 `desired_running` 照樣留著，
    /// 看門狗會接手重試、健康那格也看得到；舊行為是啟動成功才寫，於是失敗的 start 留下「沒人要它跑」。
    #[tokio::test]
    async fn a_start_records_the_intent_before_launching_so_a_failed_start_is_still_wanted() {
        let app = flow_tests::app().await;
        assert_eq!(wanted(&app).await, 0);

        assert!(post_start(State(app.clone())).await.is_err(), "測試環境沒有 herdr，啟動一定失敗");

        assert_eq!(wanted(&app).await, 1, "意圖在啟動之前就寫下去了，看門狗接得下去");
    }

    /// Start：意圖寫不進去就一個 pane 都不開，並且說清楚是持久化失敗，不是「AGM 起不來」。
    #[tokio::test]
    async fn a_start_whose_intent_write_fails_launches_nothing() {
        let app = flow_tests::app().await;
        break_intent_writes(&app).await;

        let err = post_start(State(app.clone())).await.unwrap_err();

        let msg = format!("{err:?}");
        assert!(msg.contains("could not persist desired_running"), "{msg}");
        assert_eq!(wanted(&app).await, 0);
        assert!(crate::db::active_run(&app.db, "patrol").await.unwrap().is_none(), "什麼都沒啟動");
    }

    /// 還沒 setup 就 start：回 not_configured，不要留下一個沒有 bot 可以對應的「要它跑」。
    #[tokio::test]
    async fn a_start_without_a_configured_manager_leaves_no_dangling_intent() {
        let app = flow_tests::app().await;
        sqlx::query("UPDATE supervisors SET bot_id=NULL WHERE id=?")
            .bind(store::SUPERVISOR_ID)
            .execute(&app.db)
            .await
            .unwrap();

        let err = post_start(State(app.clone())).await.unwrap_err();

        assert!(format!("{err:?}").contains("not_configured"), "{err:?}");
        assert_eq!(wanted(&app).await, 0);
    }
}
