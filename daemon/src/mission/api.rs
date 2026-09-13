//! `/api/projects/{id}/missions`、`/api/missions/*`、`/api/identities/{name}/disabled`。
//! 契約寫在 `docs/API.md` 的「群組任務」一節；這支檔案改了那一節要跟著改。

use super::{deliver, pick, store};
use crate::lifecycle::LcError;
use crate::state::App;
use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

fn up<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(e.to_string())
}

async fn emit(app: &Arc<App>, m: &store::Mission) {
    app.emit("mission_updated", json!({"mission_id": m.id, "project_id": m.project_id, "status": m.status()})).await;
}

async fn load(app: &Arc<App>, id: &str) -> Result<store::Mission, LcError> {
    store::get(&app.db, id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("mission".into()))
}

/// 任務現在在哪一段：終態與暫停看任務本身，其餘從它的交辦推——最新一件還開著的交辦的角色。
/// 不另存狀態，就不必跟 assignments 對帳。
pub fn phase(m: &store::Mission, assignments: &[crate::supervisor::store::Assignment]) -> &'static str {
    match m.status() {
        "done" => return "done",
        "cancelled" => return "cancelled",
        "paused" => return "paused",
        _ => {}
    }
    if assignments.is_empty() {
        return "planning";
    }
    let open = assignments.iter().rev().find(|a| crate::supervisor::store::OPEN_STATES.contains(&a.status.as_str()));
    match open {
        Some(a) if a.status == "quota_blocked" => "waiting_quota",
        Some(a) => match a.mission_role.as_deref() {
            Some("reviewer") => "reviewing",
            Some("verifier") => "verifying",
            _ => "executing",
        },
        // 交辦都結案了、任務還開著：輪到 AGM 決定下一步（派下一個角色、交付或完成）。
        None => "awaiting_agm",
    }
}

async fn with_phase(app: &Arc<App>, m: &store::Mission) -> Result<Value, LcError> {
    let assignments = crate::supervisor::store::mission_assignments(&app.db, &m.id).await.map_err(up)?;
    let mut out = m.json();
    out["phase"] = phase(m, &assignments).into();
    out["assignments"] = json!(assignments
        .iter()
        .map(|a| json!({
            "id": a.id,
            "role": a.mission_role,
            "status": a.status,
            "target_bot_id": a.target_bot_id,
            "turn_status": a.turn_status,
            "turn_error": a.turn_error,
            "follow_up_of": a.follow_up_of,
            "created_at": a.created_at,
            "completed_at": a.completed_at,
        }))
        .collect::<Vec<_>>());
    Ok(out)
}

/// 已結案的任務不能再**動狀態**：一律 409 `already_closed`，讓呼叫端知道要開新任務而不是重試。
///
/// 追問與回覆（`question` / `answer`）不走這裡：對著已完成的成果問一句話不會改變任何交付，
/// 擋掉它只是讓使用者沒地方問（原本連記一句都會 409）。要改東西請用 `revise` 開續作。
fn ensure_open(m: &store::Mission) -> Result<(), LcError> {
    if m.completed_at.is_some() || m.cancelled_at.is_some() {
        return Err(LcError::conflict("already_closed", json!({"mission_id": m.id, "status": m.status()})));
    }
    Ok(())
}

/// `relay_from` 跟 `POST /api/bots/{id}/prompt` 同一套：省略＝使用者本人，否則必須是存在中的 bot 或 `daemon`。
async fn check_relay_from(app: &Arc<App>, relay_from: Option<&str>) -> Result<Option<String>, LcError> {
    match relay_from.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(crate::agent_relay::DAEMON_SENDER) => Ok(Some(crate::agent_relay::DAEMON_SENDER.into())),
        Some(id) => match crate::db::bot(&app.db, id).await.map_err(up)? {
            Some(b) if b.deleted_at.is_none() => Ok(Some(b.id)),
            _ => Err(LcError::Bad(format!("relay_from must be a live bot id or `{}`", crate::agent_relay::DAEMON_SENDER))),
        },
    }
}

fn one_of(field: &str, v: &str, allowed: &[&str]) -> Result<(), LcError> {
    if allowed.contains(&v) {
        Ok(())
    } else {
        Err(LcError::Bad(format!("{field} must be one of {}", allowed.join(" | "))))
    }
}

#[derive(Deserialize)]
pub struct NewMissionIn {
    text: String,
    #[serde(default)]
    client_request_id: Option<String>,
    delivery_mode: String,
    executor_kind: String,
    on_5h_limit: String,
    #[serde(default)]
    max_rounds: Option<i64>,
}

/// 群組的「交給 AGM」。建任務、在群組時間軸記下使用者的指示，並放進 AGM 的 inbox 叫它起來調度。
pub async fn post_mission(
    State(app): State<Arc<App>>,
    Path(project_id): Path<String>,
    Json(b): Json<NewMissionIn>,
) -> Result<Json<Value>, LcError> {
    let text = b.text.trim();
    if text.is_empty() {
        return Err(LcError::Bad("text is empty".into()));
    }
    one_of("delivery_mode", &b.delivery_mode, &["push_main", "pr"])?;
    one_of("executor_kind", &b.executor_kind, &["claude", "codex", "grok"])?;
    one_of("on_5h_limit", &b.on_5h_limit, &["wait", "switch"])?;
    let max_rounds = b.max_rounds.unwrap_or(2);
    if !(0..=10).contains(&max_rounds) {
        return Err(LcError::Bad("max_rounds must be 0..=10".into()));
    }
    let project = crate::db::project(&app.db, &project_id)
        .await
        .map_err(up)?
        .filter(|p| p.deleted_at.is_none())
        .ok_or_else(|| LcError::NotFound("project".into()))?;
    if project.host != crate::config::LOCAL_HOST {
        // team 的 worktree helper 同樣本機限定；遠端要另外設計交付路徑，先明確拒絕。
        return Err(LcError::BadValue(json!({"error": "remote_not_supported", "host": project.host})));
    }
    let crid = b.client_request_id.clone().unwrap_or_else(crate::db::ulid);
    let (m, created) = store::create(
        &app.db,
        &store::NewMission {
            project_id: &project.id,
            client_request_id: &crid,
            text,
            delivery_mode: &b.delivery_mode,
            executor_kind: &b.executor_kind,
            on_5h_limit: &b.on_5h_limit,
            max_rounds,
            parent_mission_id: None,
        },
    )
    .await
    .map_err(up)?;
    if created {
        store::add_event(&app.db, &m.id, "instruction", text, None, &json!({})).await.map_err(up)?;
        let payload = json!({
            "mission_id": m.id,
            "project_id": project.id,
            "project": project.label,
            "cwd": project.path,
            "text": text,
            "delivery_mode": m.delivery_mode,
            "executor_kind": m.executor_kind,
            "on_5h_limit": m.on_5h_limit,
            "max_rounds": m.max_rounds,
        });
        crate::supervisor::store::push_inbox(&app.db, &format!("mission:{}:created", m.id), "mission_created", None, None, None, &payload)
            .await
            .map_err(up)?;
        emit(&app, &m).await;
    }
    let mut out = m.json();
    out["created"] = created.into();
    Ok(Json(out))
}

#[derive(Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
}

/// 「已完成任務」＝ `status=done`。
pub async fn get_missions(
    State(app): State<Arc<App>>,
    Path(project_id): Path<String>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Value>, LcError> {
    let status = q.status.as_deref().unwrap_or("all");
    one_of("status", status, &["all", "open", "done", "cancelled"])?;
    let rows = store::list(&app.db, &project_id, status, q.limit.unwrap_or(100)).await.map_err(up)?;
    let mut missions = Vec::with_capacity(rows.len());
    for m in &rows {
        missions.push(with_phase(&app, m).await?);
    }
    Ok(Json(json!({"project_id": project_id, "missions": missions})))
}

pub async fn get_mission(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Json<Value>, LcError> {
    let m = load(&app, &id).await?;
    let events = store::events(&app.db, &id).await.map_err(up)?;
    let mut out = with_phase(&app, &m).await?;
    out["events"] = json!(events);
    // 兩個方向都要連得回去：成果卡要能往下走到續作，續作也要知道自己是從哪一筆來的。
    out["revisions"] = json!(store::children(&app.db, &id).await.map_err(up)?.iter().map(|c| json!({
        "id": c.id,
        "text": c.text,
        "status": c.status(),
        "created_at": c.created_at,
        "result_summary": c.result_summary,
    })).collect::<Vec<_>>());
    out["parent"] = match m.parent_mission_id.as_deref() {
        Some(pid) => match store::get(&app.db, pid).await.map_err(up)? {
            Some(p) => json!({"id": p.id, "text": p.text, "status": p.status(), "result_summary": p.result_summary}),
            // 父任務被刪掉了也不要假裝沒有過：id 留著，讓畫面說得出「來源已不在」。
            None => json!({"id": pid, "missing": true}),
        },
        None => Value::Null,
    };
    Ok(Json(out))
}

/// 追問／回覆／續作共用的重送規則。
///
/// 同一個 `client_request_id`：內容一樣就回原本那一則（`replayed`），內容不一樣是 409 —— 同一個
/// 冪等鍵配兩種內容，呼叫端一定有一邊會誤以為自己那句話送出去了。
fn reused(existing: &store::MissionEvent) -> LcError {
    LcError::conflict(
        "request_id_reused",
        json!({
            "event_id": existing.id,
            "kind": existing.kind,
            "detail": "same client_request_id, different request (text / kind / source / reply_to)",
        }),
    )
}

/// 任務現在的狀態不接受這個動作。`write_reply` 在交易裡判的，所以這時候什麼都還沒寫。
fn refused(m: &store::Mission, why: &str) -> LcError {
    let hint = match why {
        "cancelled" => "任務已取消，沒有東西在等回答",
        "already_closed" => "已完成的任務用 question 追問，或用 revise 開續作",
        "not_paused" => "任務沒有停下來問人，不需要回答；要補充資訊請用 question",
        _ => "任務狀態不允許這個動作",
    };
    LcError::conflict(why, json!({"mission_id": m.id, "status": m.status(), "hint": hint}))
}

#[derive(Deserialize)]
pub struct QuestionIn {
    text: String,
    client_request_id: String,
    /// AGM 代使用者問時帶自己的 bot id。省略＝使用者本人（契約同 prompt）。
    #[serde(default)]
    relay_from: Option<String>,
}

/// 使用者對成果追問。**已完成的任務也接受**。
///
/// 只寫一則事件並叫醒 AGM，不改任何狀態：不 resume、不碰 completed、不建任務、不產生交付。
/// 使用者想真的改東西，走 `revise`。
pub async fn post_question(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(b): Json<QuestionIn>,
) -> Result<Json<Value>, LcError> {
    let text = b.text.trim();
    let crid = b.client_request_id.trim();
    if text.is_empty() {
        return Err(LcError::Bad("text is empty".into()));
    }
    if crid.is_empty() {
        return Err(LcError::Bad("client_request_id is empty".into()));
    }
    let m = load(&app, &id).await?;
    let from = check_relay_from(&app, b.relay_from.as_deref()).await?;
    let payload = json!({
        "mission_id": m.id,
        "project_id": m.project_id,
        "status": m.status(),
        "mission_text": m.text,
        "result_summary": m.result_summary,
        "question": text,
        "asked_by": from,
        // 說清楚這是問問題，不是改東西：AGM 回答就好，不要開工。
        "expects": "answer_only",
    });
    let outcome = store::write_reply(
        &app.db,
        &id,
        "question",
        text,
        from.as_deref(),
        None,
        crid,
        store::Requires::Anything,
        false,
        Some((format!("mission:{id}:question:{crid}"), "mission_question", &payload)),
    )
    .await
    .map_err(up)?;
    let out = match outcome {
        store::ReplyOutcome::Written(w) => json!({"event": w.event, "replayed": false}),
        store::ReplyOutcome::Replayed(e) => json!({"event": e, "replayed": true}),
        store::ReplyOutcome::Mismatch(e) => return Err(reused(&e)),
        store::ReplyOutcome::Refused(why) => return Err(refused(&m, why)),
    };
    emit(&app, &load(&app, &id).await?).await;
    Ok(Json(out))
}

#[derive(Deserialize)]
pub struct AnswerIn {
    text: String,
    client_request_id: String,
    /// AGM 回覆追問時指回那一則 question 事件。
    #[serde(default)]
    reply_to: Option<String>,
    #[serde(default)]
    relay_from: Option<String>,
}

/// 回覆。兩種語意由 `relay_from` 分開，**不靠猜**：
///
/// - 沒有 `relay_from`（使用者本人）＝回答「停下來問人」的任務 → 記錄＋放行＋叫醒 AGM，一個交易。
///   任務沒有停著就不接受（`not_paused`），已取消／已完成也不接受——那些狀態下沒有東西在等答案。
/// - 有 `relay_from`（AGM／bot）＝回覆某一則追問 → **必須**帶 `reply_to`，不 resume、不推 inbox
///   （AGM 自己的回覆叫醒它自己就是通知迴圈的起點）。
pub async fn post_answer(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(b): Json<AnswerIn>,
) -> Result<Json<Value>, LcError> {
    let text = b.text.trim();
    let crid = b.client_request_id.trim();
    if text.is_empty() {
        return Err(LcError::Bad("text is empty".into()));
    }
    if crid.is_empty() {
        return Err(LcError::Bad("client_request_id is empty".into()));
    }
    let m = load(&app, &id).await?;
    let from = check_relay_from(&app, b.relay_from.as_deref()).await?;
    let is_bot_reply = from.is_some();
    // bot 的回覆一定是在回某一則追問，而且那則追問要真的屬於這筆任務。少了這個，
    // 「回覆」就變成一句沒有對象的話，UI 也串不起來。
    let reply_to = match (is_bot_reply, b.reply_to.as_deref().map(str::trim).filter(|s| !s.is_empty())) {
        (true, None) => return Err(LcError::Bad("a bot reply needs reply_to (the question event id)".into())),
        (_, Some(rt)) => {
            let known = store::events(&app.db, &id)
                .await
                .map_err(up)?
                .into_iter()
                .any(|e| e.id == rt && e.kind == "question");
            if !known {
                return Err(LcError::Bad("reply_to is not a question of this mission".into()));
            }
            Some(rt.to_string())
        }
        (false, None) => None,
    };
    let payload = json!({
        "mission_id": m.id,
        "project_id": m.project_id,
        "answer": text,
        "reply_to": reply_to,
        "from": from,
    });
    let outcome = store::write_reply(
        &app.db,
        &id,
        "answer",
        text,
        from.as_deref(),
        reply_to.as_deref(),
        crid,
        // 使用者回答只在任務真的停著時才成立；bot 回覆追問在任何狀態都可以。
        if is_bot_reply { store::Requires::Anything } else { store::Requires::Paused },
        !is_bot_reply,
        (!is_bot_reply).then(|| (format!("mission:{id}:answer:{crid}"), "mission_answered", &payload)),
    )
    .await
    .map_err(up)?;
    let (event, replayed, resumed) = match outcome {
        store::ReplyOutcome::Written(w) => (json!(w.event), false, w.resumed),
        store::ReplyOutcome::Replayed(e) => (json!(e), true, false),
        store::ReplyOutcome::Mismatch(e) => return Err(reused(&e)),
        store::ReplyOutcome::Refused(why) => return Err(refused(&m, why)),
    };
    let m = load(&app, &id).await?;
    emit(&app, &m).await;
    Ok(Json(json!({"event": event, "replayed": replayed, "resumed": resumed, "mission": m.json()})))
}

/// 「不回答，直接繼續」。
///
/// 跟 `answer` 一樣要**叫醒** AGM：以前這裡只寫事件加發 SSE，使用者按了按鈕、任務也放行了，
/// 但 AGM 不主動查就永遠不知道，等於那顆按鈕沒有接線。
///
/// 只在真的發生 `paused → open` 時推一次：`store::resume` 的 UPDATE 帶了 `paused_reason IS NOT NULL`
/// 條件，所以對一個沒有暫停的任務按下去不會產生通知——AGM 自己呼叫 resume 也就不會把自己叫醒，
/// 這是通知迴圈的防線。event_key 綁那一次轉移的時刻，重送不會變成第二則。
#[derive(Deserialize)]
pub struct EventIn {
    kind: String,
    text: String,
    #[serde(default)]
    relay_from: Option<String>,
    #[serde(default)]
    payload: Option<Value>,
}

/// AGM／bot 往群組時間軸回報（`report`、`note`），或記下驗證通過（`verified`，交付前必須有）。
pub async fn post_event(State(app): State<Arc<App>>, Path(id): Path<String>, Json(b): Json<EventIn>) -> Result<Json<Value>, LcError> {
    one_of("kind", &b.kind, &["report", "note", "verified"])?;
    if b.text.trim().is_empty() {
        return Err(LcError::Bad("text is empty".into()));
    }
    let m = load(&app, &id).await?;
    ensure_open(&m)?;
    let from = check_relay_from(&app, b.relay_from.as_deref()).await?;
    let ev = store::add_event(&app.db, &id, &b.kind, b.text.trim(), from.as_deref(), &b.payload.unwrap_or_else(|| json!({})))
        .await
        .map_err(up)?;
    emit(&app, &load(&app, &id).await?).await;
    Ok(Json(json!(ev)))
}

#[derive(Deserialize)]
pub struct PauseIn {
    reason: String,
    #[serde(default)]
    detail: Option<String>,
}

pub async fn post_pause(State(app): State<Arc<App>>, Path(id): Path<String>, Json(b): Json<PauseIn>) -> Result<Json<Value>, LcError> {
    if b.reason.trim().is_empty() {
        return Err(LcError::Bad("reason is empty".into()));
    }
    let m = load(&app, &id).await?;
    ensure_open(&m)?;
    store::pause(&app.db, &id, b.reason.trim(), b.detail.as_deref()).await.map_err(up)?;
    let text = match b.detail.as_deref() {
        Some(d) => format!("暫停：{}（{}）", b.reason.trim(), d),
        None => format!("暫停：{}", b.reason.trim()),
    };
    store::add_event(&app.db, &id, "paused", &text, Some(crate::agent_relay::DAEMON_SENDER), &json!({"reason": b.reason}))
        .await
        .map_err(up)?;
    let m = load(&app, &id).await?;
    emit(&app, &m).await;
    Ok(Json(m.json()))
}

/// 「不回答，直接繼續」。
///
/// 跟 `answer` 一樣要**叫醒** AGM：以前這裡只寫事件加發 SSE，使用者按了按鈕、任務也放行了，
/// 但 AGM 不主動查就永遠不知道，等於那顆按鈕沒有接線。
///
/// 只在真的發生 `paused → open` 時推一次：`store::resume` 的 UPDATE 帶了 `paused_reason IS NOT NULL`
/// 條件，所以對一個沒有暫停的任務按下去不會產生通知——AGM 自己呼叫 resume 也就不會把自己叫醒，
/// 這是通知迴圈的防線。event_key 綁那一次轉移的時刻，重送不會變成第二則。
pub async fn post_resume(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Json<Value>, LcError> {
    let m = load(&app, &id).await?;
    ensure_open(&m)?;
    let paused_reason = m.paused_reason.clone();
    let project_id = m.project_id.clone();
    // 放行、記事件、推 inbox 一次交易。分開寫的話，中途失敗就會留下「已經放行但沒人被叫醒」，
    // 而且沒有任何東西會回頭補送。
    store::resume_and_wake(&app.db, &id, |_ev| {
        json!({
            "mission_id": id,
            "project_id": project_id,
            "was_paused_for": paused_reason,
            "answered": false,
            // 來源中性：這支端點使用者與 AGM 都會呼叫，寫死「使用者」會在 AGM 自己續跑時說謊。
            "note": "任務被要求直接繼續（沒有附回答）",
        })
    })
    .await
    .map_err(up)?;
    let m = load(&app, &id).await?;
    emit(&app, &m).await;
    Ok(Json(m.json()))
}

#[derive(Deserialize)]
pub struct ReviseIn {
    text: String,
    client_request_id: String,
    /// AGM 代使用者開續作時帶自己的 bot id（runbook 允許代用）。省略＝使用者本人。
    #[serde(default)]
    relay_from: Option<String>,
    #[serde(default)]
    delivery_mode: Option<String>,
    #[serde(default)]
    executor_kind: Option<String>,
    #[serde(default)]
    on_5h_limit: Option<String>,
    #[serde(default)]
    max_rounds: Option<i64>,
}

/// 追加修改：從一筆**已完成**的成果開一筆新任務。
///
/// 新任務是獨立的一筆（`parent_mission_id` 指回來），所以它自己的 `verified` / `delivered` /
/// `completed` 全都要重新來過——舊的那些事件屬於舊的 mission id，交付關卡（`has_event(id,"verified")`）
/// 天生看不到它們。這正是「不沿用舊驗證」不需要額外防呆的原因。
///
/// 脈絡是**快照**：原指示、結果摘要、commit/PR、驗證摘要都在建立當下寫進新任務的 instruction 事件與
/// inbox payload，所以原本那顆臨時 bot 早就被清掉也不影響續作。快照只是參考資料，不是證據——
/// 它不會、也不能讓新任務跳過自己的驗證。
pub async fn post_revise(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(b): Json<ReviseIn>,
) -> Result<Json<Value>, LcError> {
    let text = b.text.trim();
    let crid = b.client_request_id.trim();
    if text.is_empty() {
        return Err(LcError::Bad("text is empty".into()));
    }
    if crid.is_empty() {
        return Err(LcError::Bad("client_request_id is empty".into()));
    }
    let parent = load(&app, &id).await?;
    // 契約鎖死：只有已完成的成果能續作。進行中的請直接回答／等它做完（要改方向就先 cancel），
    // 取消掉的沒有成果可以接續——兩種都回明確的理由，不要讓呼叫端猜。
    if parent.completed_at.is_none() {
        return Err(LcError::conflict(
            "not_completed",
            json!({
                "mission_id": parent.id,
                "status": parent.status(),
                "hint": if parent.cancelled_at.is_some() { "已取消的任務沒有成果可以續作，請開新任務" } else { "任務還在進行中，等它完成或先取消" },
            }),
        ));
    }

    let events = store::events(&app.db, &id).await.map_err(up)?;
    let last = |kind: &str| events.iter().rev().find(|e| e.kind == kind).cloned();
    let verified = last("verified");
    let delivered = last("delivered");
    let delivered_payload: Value =
        delivered.as_ref().and_then(|e| serde_json::from_str(&e.payload_json).ok()).unwrap_or_else(|| json!({}));
    // 原成果的交付**可能還沒進 main**（PR 還開著、或 push 失敗）。把事實原樣帶過去並講明要自己確認，
    // 不要讓續作預設「基底已經有那份改動」。
    let delivery_mode_used = delivered_payload.get("mode").and_then(Value::as_str).map(str::to_string);
    let snapshot = json!({
        "parent_mission_id": parent.id,
        "parent_text": parent.text,
        "parent_result_summary": parent.result_summary,
        "parent_completed_at": parent.completed_at,
        // 驗證事件連 payload 一起帶（截圖路徑、數字都在裡面），只留一句 text 會把證據丟掉。
        "parent_verified": verified.as_ref().map(|e| json!({
            "text": e.text,
            "at": e.created_at,
            "payload": serde_json::from_str::<Value>(&e.payload_json).unwrap_or_else(|_| json!({})),
        })),
        "parent_delivery": delivered_payload,
        // 只說得出「當初用哪種方式交付」。交付之後 main 有沒有那份改動是**現在**的事實：
        // push_main 可能後來被 revert，PR 也可能早就被合併了——都不是這裡查得到的。
        "parent_delivery_mode": delivery_mode_used,
        "parent_delivery_in_main": "unknown",
        "request": text,
        "caveat": "原成果現在在不在基底裡，這裡不知道（push 可能被 revert，PR 可能已合併）：動手前先查目前基底，不要假設",
        "evidence_note": "以上是參考脈絡，不是驗證證據；這一筆要自己重新驗證才能交付",
    });

    // 選項要跟開新任務走同一套驗證：沒驗就直接進 DB 的話，delivery_mode 可以是任何字串，
    // 而挑身分／交付那邊只認得幾個固定值，壞掉會在很後面才爆出來。
    let delivery_mode = b.delivery_mode.clone().unwrap_or_else(|| parent.delivery_mode.clone());
    let executor_kind = b.executor_kind.clone().unwrap_or_else(|| parent.executor_kind.clone());
    let on_5h_limit = b.on_5h_limit.clone().unwrap_or_else(|| parent.on_5h_limit.clone());
    let max_rounds = b.max_rounds.unwrap_or(parent.max_rounds);
    one_of("delivery_mode", &delivery_mode, &["push_main", "pr"])?;
    one_of("executor_kind", &executor_kind, &["claude", "codex", "grok"])?;
    one_of("on_5h_limit", &on_5h_limit, &["wait", "switch"])?;
    if !(0..=10).contains(&max_rounds) {
        return Err(LcError::Bad("max_rounds must be 0..=10".into()));
    }
    // 專案還在、還是本機（跟 post_mission 同一條線）——原任務的專案可能已經被刪或改成遠端。
    let project = crate::db::project(&app.db, &parent.project_id)
        .await
        .map_err(up)?
        .filter(|p| p.deleted_at.is_none())
        .ok_or_else(|| LcError::NotFound("project".into()))?;
    if project.host != crate::config::LOCAL_HOST {
        return Err(LcError::BadValue(json!({"error": "remote_not_supported", "host": project.host})));
    }

    let project_id = parent.project_id.clone();
    let parent_note = format!("追加修改：已開續作任務（{}）", text);
    // 指紋只包含請求本身（parent＋文字＋四個選項），不含會變的快照：用會變的東西當冪等鍵，
    // 同一個請求重送兩次就會被判成兩個不同的請求。
    let fingerprint = store::revise_fingerprint(&parent.id, text, &delivery_mode, &executor_kind, &on_5h_limit, max_rounds);
    let base_payload = snapshot.clone();
    let opts = (delivery_mode.clone(), executor_kind.clone(), on_5h_limit.clone());
    let outcome = store::create_child(
        &app.db,
        &store::NewMission {
            project_id: &project_id,
            client_request_id: crid,
            text,
            delivery_mode: &delivery_mode,
            executor_kind: &executor_kind,
            on_5h_limit: &on_5h_limit,
            max_rounds,
            parent_mission_id: Some(&parent.id),
        },
        &fingerprint,
        &snapshot,
        &parent_note,
        |child_id| {
            let mut payload = base_payload.clone();
            if let Some(o) = payload.as_object_mut() {
                o.insert("mission_id".into(), child_id.into());
                o.insert("project_id".into(), project_id.clone().into());
                o.insert("text".into(), text.into());
                o.insert("delivery_mode".into(), opts.0.clone().into());
                o.insert("executor_kind".into(), opts.1.clone().into());
                o.insert("on_5h_limit".into(), opts.2.clone().into());
                o.insert("max_rounds".into(), max_rounds.into());
                // AGM 的 runbook 對續作從第 2 步接手：不用重新規劃，脈絡都在這裡。
                o.insert("runbook_start_step".into(), 2.into());
            }
            payload
        },
    )
    .await
    .map_err(up)?;
    let child = match outcome {
        store::ChildCreate::Created(c) => c,
        store::ChildCreate::Replayed(c) => {
            let mut out = c.json();
            out["created"] = false.into();
            out["replayed"] = true.into();
            return Ok(Json(out));
        }
        // 同一個 crid 換了 parent、文字或選項：講明白，不要默默回別人的那一筆。
        store::ChildCreate::Mismatch(c) => {
            return Err(LcError::conflict(
                "request_id_reused",
                json!({"mission_id": c.id, "parent_mission_id": c.parent_mission_id,
                       "detail": "same client_request_id, different request (parent / text / options)"}),
            ));
        }
        // 這個 parent 已經有一輪續作還沒結束（在跑、暫停、等額度都算）。第二筆會讓兩輪同時改同一份
        // 成果，所以擋下來並指向那一筆——不是「重送」，是「你要找的在那裡」。
        store::ChildCreate::OpenChildExists(open) => {
            return Err(LcError::conflict(
                "revision_in_progress",
                json!({
                    "mission_id": open.id,
                    "status": open.status(),
                    "text": open.text,
                    "hint": "這筆成果已經有一輪續作還沒結束，先看那一筆（或等它完成／取消）",
                }),
            ));
        }
    };

    emit(&app, &child).await;
    emit(&app, &load(&app, &id).await?).await;
    let mut out = child.json();
    out["created"] = true.into();
    Ok(Json(out))
}

pub async fn post_cancel(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Json<Value>, LcError> {
    let m = load(&app, &id).await?;
    ensure_open(&m)?;
    store::cancel(&app.db, &id).await.map_err(up)?;
    store::add_event(&app.db, &id, "cancelled", "已取消", Some(crate::agent_relay::DAEMON_SENDER), &json!({})).await.map_err(up)?;
    let m = load(&app, &id).await?;
    let temp = cleanup_temp_bots(&app, &m).await;
    emit(&app, &m).await;
    let mut out = m.json();
    out["temp_bots"] = temp;
    Ok(Json(out))
}

/// 任務結案（完成或取消）時收掉 AGM 為它開的臨時 bot（§18.14 第 6 步「停止並刪除」）。
///
/// 只刪同時滿足三件事的 bot，缺一就留著、在回應與事件裡說明為什麼：
/// 1. 是這個任務某件交辦的 `target_bot_id`——不是任務派過工的 bot 一律不碰；
/// 2. 名字以 `agm-mission-<任務 id 尾碼>-` 開頭（尾 6 碼；相容 5 碼的舊命名）——
///    使用者自己的常駐 bot 也可能被派到任務裡的工作，名字是「這是臨時開的」唯一的證據；
/// 3. 沒有進行中的 run——還在跑的不能從它腳下把 bot 刪掉，AGM 停掉之後用 `agm bot delete` 收。
/// 刪除走跟 `DELETE /api/bots/{id}` 同一條路（軟刪、子 agent 一起收、對話紀錄保留）。
pub fn is_temp_bot_name(mission_id: &str, name: &str) -> bool {
    let Some(rest) = name.strip_prefix("agm-mission-") else { return false };
    let Some((tail, role)) = rest.split_once('-') else { return false };
    let id = mission_id.to_ascii_lowercase();
    let tail = tail.to_ascii_lowercase();
    (5..=6).contains(&tail.len()) && id.ends_with(&tail) && !role.is_empty()
}

async fn cleanup_temp_bots(app: &Arc<App>, m: &store::Mission) -> Value {
    let Ok(assignments) = crate::supervisor::store::mission_assignments(&app.db, &m.id).await else {
        return json!({"deleted": [], "skipped": [], "error": "could not read the mission's assignments"});
    };
    let mut seen = std::collections::BTreeSet::new();
    let (mut deleted, mut skipped) = (Vec::new(), Vec::new());
    for a in assignments {
        if !seen.insert(a.target_bot_id.clone()) {
            continue;
        }
        let Ok(Some(bot)) = crate::db::bot(&app.db, &a.target_bot_id).await else { continue };
        if bot.deleted_at.is_some() {
            continue;
        }
        if !is_temp_bot_name(&m.id, &bot.name) {
            skipped.push(json!({"bot_id": bot.id, "name": bot.name, "reason": "not_a_temp_bot"}));
            continue;
        }
        if crate::db::active_run(&app.db, &bot.id).await.ok().flatten().is_some() {
            skipped.push(json!({"bot_id": bot.id, "name": bot.name, "reason": "still_running"}));
            continue;
        }
        match crate::api::delete_bot(State(app.clone()), Path(bot.id.clone())).await {
            Ok(_) => deleted.push(json!({"bot_id": bot.id, "name": bot.name})),
            Err(e) => skipped.push(json!({"bot_id": bot.id, "name": bot.name, "reason": "delete_failed", "detail": format!("{e:?}")})),
        }
    }
    let out = json!({"deleted": deleted, "skipped": skipped});
    if !deleted.is_empty() || !skipped.is_empty() {
        let names = |v: &[Value]| v.iter().filter_map(|x| x["name"].as_str()).collect::<Vec<_>>().join("、");
        let mut text = String::new();
        if !deleted.is_empty() {
            text.push_str(&format!("已刪除臨時 bot：{}", names(&deleted)));
        }
        if !skipped.is_empty() {
            if !text.is_empty() {
                text.push('；');
            }
            text.push_str(&format!("未刪除：{}", names(&skipped)));
        }
        let _ = store::add_event(&app.db, &m.id, "note", &text, Some(crate::agent_relay::DAEMON_SENDER), &out).await;
    }
    out
}

#[derive(Deserialize)]
pub struct CompleteIn {
    result_summary: String,
    #[serde(default)]
    relay_from: Option<String>,
}

pub async fn post_complete(State(app): State<Arc<App>>, Path(id): Path<String>, Json(b): Json<CompleteIn>) -> Result<Json<Value>, LcError> {
    if b.result_summary.trim().is_empty() {
        return Err(LcError::Bad("result_summary is empty".into()));
    }
    let m = load(&app, &id).await?;
    ensure_open(&m)?;
    let from = check_relay_from(&app, b.relay_from.as_deref()).await?;
    store::complete(&app.db, &id, b.result_summary.trim()).await.map_err(up)?;
    store::add_event(&app.db, &id, "completed", b.result_summary.trim(), from.as_deref(), &json!({})).await.map_err(up)?;
    let m = load(&app, &id).await?;
    let temp = cleanup_temp_bots(&app, &m).await;
    emit(&app, &m).await;
    let mut out = m.json();
    out["temp_bots"] = temp;
    Ok(Json(out))
}

/// 用掉一輪（review 退回或驗證失敗）。到上限就把任務停下來（`max_rounds`）並回 409。
pub async fn post_round(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Json<Value>, LcError> {
    let m = load(&app, &id).await?;
    ensure_open(&m)?;
    match store::use_round(&app.db, &id).await.map_err(up)? {
        Ok(used) => {
            store::add_event(&app.db, &id, "round", &format!("第 {used} 輪退回（上限 {}）", m.max_rounds), Some(crate::agent_relay::DAEMON_SENDER), &json!({"rounds_used": used}))
                .await
                .map_err(up)?;
            let m = load(&app, &id).await?;
            emit(&app, &m).await;
            Ok(Json(m.json()))
        }
        Err(used) => {
            let detail = format!("review／驗證已退回 {used} 輪，達到上限 {}", m.max_rounds);
            store::pause(&app.db, &id, "max_rounds", Some(&detail)).await.map_err(up)?;
            store::add_event(&app.db, &id, "paused", &format!("暫停：{detail}，等使用者決定"), Some(crate::agent_relay::DAEMON_SENDER), &json!({"reason": "max_rounds"}))
                .await
                .map_err(up)?;
            emit(&app, &load(&app, &id).await?).await;
            Err(LcError::conflict("max_rounds", json!({"mission_id": id, "rounds_used": used, "max_rounds": m.max_rounds})))
        }
    }
}

/// 照任務的設定挑身分（D3–D7）。`role=verifier` 找不到 Fable 額度時回 `ask_user`，並把任務停下來（D6）。
pub async fn get_pick(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, LcError> {
    let m = load(&app, &id).await?;
    let role = q.get("role").and_then(|r| pick::Role::parse(r)).ok_or_else(|| LcError::Bad("role must be executor | reviewer | verifier".into()))?;
    let project = crate::db::project(&app.db, &m.project_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("project".into()))?;
    // 驗證者一律 claude＋Fable（D3）；reviewer 跟執行者同 kind。
    let kind = if role == pick::Role::Verifier { "claude" } else { m.executor_kind.as_str() };
    let raw = super::candidates(&app, &project.host, kind).await;
    let cands: Vec<pick::Candidate> = raw.iter().map(|(n, d, q)| pick::Candidate { name: n, disabled: *d, quota: q.as_ref() }).collect();
    let on_5h = if m.on_5h_limit == "switch" { pick::On5hLimit::Switch } else { pick::On5hLimit::Wait };
    let decision = pick::pick(role, &cands, on_5h, q.get("exclude").map(String::as_str), chrono::Utc::now());
    if role == pick::Role::Verifier && m.completed_at.is_none() && m.cancelled_at.is_none() {
        if let pick::Pick::AskUser { reason, .. } = &decision {
            if m.paused_reason.as_deref() != Some("no_fable_for_verifier") {
                store::pause(&app.db, &id, "no_fable_for_verifier", Some(reason)).await.map_err(up)?;
                store::add_event(&app.db, &id, "paused", &format!("暫停：{reason}，等使用者決定"), Some(crate::agent_relay::DAEMON_SENDER), &json!({"reason": "no_fable_for_verifier", "decision": decision}))
                    .await
                    .map_err(up)?;
                emit(&app, &load(&app, &id).await?).await;
            }
        }
    }
    Ok(Json(json!({"mission_id": id, "role": q.get("role"), "kind": kind, "pick": decision})))
}

#[derive(Deserialize)]
pub struct DeliverIn {
    /// 要交付的 worktree（本機絕對路徑），HEAD 就是要交的 commit。
    worktree: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    relay_from: Option<String>,
}

/// 依任務的 `delivery_mode` 推 main（fast-forward only）或開 PR。必須先有 `verified` 事件。
/// 任何失敗都把任務停下來問人（D8），回 409 帶機器碼。
pub async fn post_deliver(State(app): State<Arc<App>>, Path(id): Path<String>, Json(b): Json<DeliverIn>) -> Result<Json<Value>, LcError> {
    let m = load(&app, &id).await?;
    ensure_open(&m)?;
    let from = check_relay_from(&app, b.relay_from.as_deref()).await?;
    if !store::has_event(&app.db, &id, "verified").await.map_err(up)? {
        return Err(LcError::conflict("not_verified", json!({"mission_id": id})));
    }
    let dir = std::path::PathBuf::from(&b.worktree);
    if !dir.is_absolute() || !dir.is_dir() {
        return Err(LcError::Bad("worktree must be an existing absolute path".into()));
    }
    let result = if m.delivery_mode == "push_main" {
        deliver::push_main(&dir, "origin", "main").await.map(|sha| json!({"mode": "push_main", "sha": sha}))
    } else {
        let branch = format!("mission/{}", m.id.to_lowercase());
        let title = b.title.clone().unwrap_or_else(|| m.text.chars().take(72).collect());
        let body = b.body.clone().unwrap_or_else(|| format!("群組任務 {}\n\n{}", m.id, m.text));
        deliver::open_pr(&dir, "origin", "main", &branch, &title, &body).await.map(|url| json!({"mode": "pr", "branch": branch, "url": url}))
    };
    match result {
        Ok(out) => {
            let text = match out["mode"].as_str() {
                Some("push_main") => format!("已推上 main：{}", out["sha"].as_str().unwrap_or_default()),
                _ => format!("已開 PR：{}", out["url"].as_str().unwrap_or_default()),
            };
            store::add_event(&app.db, &id, "delivered", &text, from.as_deref(), &out).await.map_err(up)?;
            emit(&app, &load(&app, &id).await?).await;
            Ok(Json(out))
        }
        Err(f) => {
            let reason = if m.delivery_mode == "push_main" { "push_main_failed" } else { "pr_failed" };
            store::pause(&app.db, &id, reason, Some(&format!("{}：{}", f.code, f.detail))).await.map_err(up)?;
            store::add_event(&app.db, &id, "paused", &format!("交付失敗（{}），等使用者決定：{}", f.code, f.detail), Some(crate::agent_relay::DAEMON_SENDER), &json!({"reason": reason, "code": f.code}))
                .await
                .map_err(up)?;
            emit(&app, &load(&app, &id).await?).await;
            Err(LcError::conflict(f.code, json!({"mission_id": id, "detail": f.detail})))
        }
    }
}

#[derive(Deserialize)]
pub struct DisableIn {
    kind: String,
    disabled: bool,
    #[serde(default)]
    host: Option<String>,
}

/// 身分停用搬進 daemon（原本只在瀏覽器 localStorage），挑身分時才看得到。
pub async fn put_identity_disabled(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    Json(b): Json<DisableIn>,
) -> Result<Json<Value>, LcError> {
    one_of("kind", &b.kind, &["claude", "codex", "grok"])?;
    if name.trim().is_empty() {
        return Err(LcError::Bad("identity is empty".into()));
    }
    let host = b.host.clone().unwrap_or_else(|| crate::config::LOCAL_HOST.to_string());
    store::set_identity_disabled(&app.db, &host, &b.kind, name.trim(), b.disabled).await.map_err(up)?;
    app.emit("identity_prefs_changed", json!({"host": host, "kind": b.kind, "identity": name, "disabled": b.disabled})).await;
    Ok(Json(json!({"host": host, "kind": b.kind, "identity": name, "disabled": b.disabled})))
}

pub async fn get_identity_prefs(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let rows: Vec<(String, String, String)> =
        sqlx::query_as("SELECT host, kind, identity FROM identity_prefs WHERE disabled = 1 ORDER BY host, kind, identity")
            .fetch_all(&app.db)
            .await
            .map_err(up)?;
    Ok(Json(json!({"disabled": rows.iter().map(|(h, k, i)| json!({"host": h, "kind": k, "identity": i})).collect::<Vec<_>>()})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::{Quota, Window};

    fn new_mission(crid: &str, mode: &str) -> NewMissionIn {
        NewMissionIn {
            text: "把設定頁的錯字修掉".into(),
            client_request_id: Some(crid.into()),
            delivery_mode: mode.into(),
            executor_kind: "claude".into(),
            on_5h_limit: "switch".into(),
            max_rounds: Some(2),
        }
    }

    fn quota(fable_used: f64) -> Quota {
        let w = |u: f64| Some(Window { used_pct: u, resets_at: Some("2026-09-18T06:00:00Z".into()) });
        Quota {
            five_hour: w(10.0),
            seven_day: w(10.0),
            fable: w(fable_used),
            reset_credits: None,
            limit_hit: None,
            plan: None,
            updated_at: crate::db::now(),
            source: "test".into(),
            account: None,
            host: "local".into(),
        }
    }

    fn status(v: &Value) -> &str {
        v["status"].as_str().unwrap_or_default()
    }

    fn conflict_reason(e: LcError) -> String {
        match e {
            LcError::Conflict(v) => v["reason"].as_str().unwrap_or_default().to_string(),
            other => panic!("expected a conflict, got {other:?}"),
        }
    }

    fn q(text: &str, crid: &str) -> QuestionIn {
        QuestionIn { text: text.into(), client_request_id: crid.into(), relay_from: None }
    }

    fn ans(text: &str, crid: &str) -> AnswerIn {
        AnswerIn { text: text.into(), client_request_id: crid.into(), reply_to: None, relay_from: None }
    }

    fn rev(text: &str, crid: &str) -> ReviseIn {
        ReviseIn {
            text: text.into(),
            client_request_id: crid.into(),
            relay_from: None,
            delivery_mode: None,
            executor_kind: None,
            on_5h_limit: None,
            max_rounds: None,
        }
    }

    async fn inbox_keys(app: &Arc<App>, like: &str) -> Vec<String> {
        sqlx::query_scalar("SELECT event_key FROM supervisor_inbox WHERE event_key LIKE ? ORDER BY created_at")
            .bind(like)
            .fetch_all(&app.db)
            .await
            .unwrap()
    }

    /// 完成的任務可以被追問，而追問**不能**改變任何交付事實。這是「已完成清單不可覆寫」的底線。
    #[tokio::test]
    async fn a_finished_mission_can_be_asked_about_without_changing_anything() {
        let env = crate::team::testing::env().await;
        let app = env.app.clone();
        let pid = env.project_id.clone();
        let Json(m) = post_mission(State(app.clone()), Path(pid.clone()), Json(new_mission("r1", "pr"))).await.unwrap();
        let id = m["id"].as_str().unwrap().to_string();
        let done = CompleteIn { result_summary: "改好了".into(), relay_from: None };
        post_complete(State(app.clone()), Path(id.clone()), Json(done)).await.unwrap();
        let before = get_mission(State(app.clone()), Path(id.clone())).await.unwrap().0;

        let Json(out) = post_question(State(app.clone()), Path(id.clone()), Json(q("這個改動會影響登入嗎？", "q1"))).await.unwrap();
        assert_eq!(out["event"]["kind"], "question");
        assert!(out["event"]["relay_from"].is_null(), "使用者本人問的，不冒充 bot");

        let after = get_mission(State(app.clone()), Path(id.clone())).await.unwrap().0;
        assert_eq!(after["status"], "done", "追問不會把已完成的任務弄回進行中");
        assert_eq!(after["completed_at"], before["completed_at"], "完成時間不變");
        assert_eq!(after["result_summary"], before["result_summary"]);
        assert_eq!(inbox_keys(&app, &format!("mission:{id}:question:%")).await.len(), 1, "AGM 被叫醒一次");

        // 同一個 crid 重送：回原本那一則，不會變成第二個問題。
        let Json(replay) = post_question(State(app.clone()), Path(id.clone()), Json(q("這個改動會影響登入嗎？", "q1"))).await.unwrap();
        assert_eq!(replay["replayed"], true);
        assert_eq!(replay["event"]["id"], out["event"]["id"]);
        assert_eq!(inbox_keys(&app, &format!("mission:{id}:question:%")).await.len(), 1);
        // 同 crid 換內容是冪等鍵被重用，要講出來。
        let err = post_question(State(app.clone()), Path(id.clone()), Json(q("換一句話", "q1"))).await.unwrap_err();
        assert_eq!(conflict_reason(err), "request_id_reused");

        // 使用者自己在已完成的任務上按「回答」是沒有意義的（沒有東西在等他），要講清楚該去哪：
        let err = post_answer(State(app.clone()), Path(id.clone()), Json(ans("那就這樣", "a0"))).await.unwrap_err();
        assert_eq!(conflict_reason(err), "already_closed");

        // AGM 回覆追問：帶自己的身分、指回那一則 question，而且**不會**放行或改狀態。
        // 這裡用 `daemon` 哨符（測試環境沒有跑 AGM bot）；正式環境帶的是 AGM 自己的 bot id，
        // 兩者都會走 `check_relay_from`，也都算「不是使用者本人」。
        let qid = out["event"]["id"].as_str().unwrap().to_string();
        let reply = AnswerIn {
            text: "不會，只動到文案".into(),
            client_request_id: "a1".into(),
            reply_to: Some(qid.clone()),
            relay_from: Some(crate::agent_relay::DAEMON_SENDER.into()),
        };
        let Json(r) = post_answer(State(app.clone()), Path(id.clone()), Json(reply)).await.unwrap();
        assert_eq!(r["event"]["reply_to"], json!(qid), "答得出是回哪一句");
        assert_eq!(r["event"]["relay_from"], json!(crate::agent_relay::DAEMON_SENDER), "不是使用者的泡泡");
        assert_eq!(r["resumed"], false);
        assert_eq!(r["mission"]["status"], "done", "回覆追問不會改變交付狀態");
        assert!(inbox_keys(&app, &format!("mission:{id}:answer:%")).await.is_empty(), "AGM 自己的回覆不會叫醒它自己");

        // reply_to 必須真的是這筆任務的某則追問，不能亂指。
        let bogus = AnswerIn {
            text: "亂指".into(),
            client_request_id: "a2".into(),
            reply_to: Some("no-such-event".into()),
            relay_from: Some(crate::agent_relay::DAEMON_SENDER.into()),
        };
        assert!(post_answer(State(app.clone()), Path(id.clone()), Json(bogus)).await.is_err());
    }

    /// 使用者回答暫停的任務：回答、放行、喚醒是一筆交易，而且重送不會派出第二次續作。
    #[tokio::test]
    async fn answering_a_paused_mission_resumes_and_wakes_the_manager_once() {
        let env = crate::team::testing::env().await;
        let app = env.app.clone();
        let pid = env.project_id.clone();
        let Json(m) = post_mission(State(app.clone()), Path(pid.clone()), Json(new_mission("r1", "pr"))).await.unwrap();
        let id = m["id"].as_str().unwrap().to_string();
        post_pause(State(app.clone()), Path(id.clone()), Json(PauseIn { reason: "max_rounds".into(), detail: None }))
            .await
            .unwrap();

        let Json(out) = post_answer(State(app.clone()), Path(id.clone()), Json(ans("照你說的做", "a1"))).await.unwrap();
        assert_eq!(out["resumed"], true);
        assert_eq!(out["mission"]["status"], "open", "放行了");
        assert_eq!(inbox_keys(&app, &format!("mission:{id}:answer:%")).await, [format!("mission:{id}:answer:a1")]);
        let kinds: Vec<String> = sqlx::query_scalar("SELECT kind FROM mission_events WHERE mission_id = ? ORDER BY created_at")
            .bind(&id)
            .fetch_all(&app.db)
            .await
            .unwrap();
        assert!(kinds.contains(&"answer".to_string()) && kinds.contains(&"resumed".to_string()));

        // 重送——而且是在任務已經被放行之後。先查重放再看狀態，所以回原結果而不是 409。
        let Json(replay) = post_answer(State(app.clone()), Path(id.clone()), Json(ans("照你說的做", "a1"))).await.unwrap();
        assert_eq!(replay["replayed"], true);
        assert_eq!(replay["event"]["id"], out["event"]["id"]);
        assert_eq!(inbox_keys(&app, &format!("mission:{id}:answer:%")).await.len(), 1, "沒有第二次喚醒");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mission_events WHERE mission_id = ? AND kind = 'answer'")
            .bind(&id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(n, 1, "沒有第二則回答");
    }

    /// 「不回答直接繼續」那顆按鈕也要真的叫醒 AGM；而 AGM 自己對沒暫停的任務按 resume 不會產生通知。
    #[tokio::test]
    async fn plain_resume_wakes_the_manager_but_cannot_loop() {
        let env = crate::team::testing::env().await;
        let app = env.app.clone();
        let pid = env.project_id.clone();
        let Json(m) = post_mission(State(app.clone()), Path(pid.clone()), Json(new_mission("r1", "pr"))).await.unwrap();
        let id = m["id"].as_str().unwrap().to_string();
        post_pause(State(app.clone()), Path(id.clone()), Json(PauseIn { reason: "waiting_quota".into(), detail: None }))
            .await
            .unwrap();

        post_resume(State(app.clone()), Path(id.clone())).await.unwrap();
        assert_eq!(inbox_keys(&app, &format!("mission:{id}:resumed:%")).await.len(), 1, "按鈕有接線");

        // 已經在跑的任務再按一次：沒有 paused→open 的轉移，就不該再有通知。
        post_resume(State(app.clone()), Path(id.clone())).await.unwrap();
        post_resume(State(app.clone()), Path(id.clone())).await.unwrap();
        assert_eq!(inbox_keys(&app, &format!("mission:{id}:resumed:%")).await.len(), 1, "不會自己叫醒自己");
    }

    /// 追加修改開的是**新的一筆**任務：舊成果原封不動，而新任務拿不到舊的 verified。
    #[tokio::test]
    async fn a_revision_is_a_new_mission_that_cannot_inherit_the_old_verification() {
        let env = crate::team::testing::env().await;
        let app = env.app.clone();
        let pid = env.project_id.clone();
        let Json(m) = post_mission(State(app.clone()), Path(pid.clone()), Json(new_mission("r1", "pr"))).await.unwrap();
        let id = m["id"].as_str().unwrap().to_string();
        let ev = EventIn {
            kind: "verified".into(),
            text: "cargo test 全過".into(),
            relay_from: Some("daemon".into()),
            payload: Some(json!({"shots": ["/tmp/a.png"]})),
        };
        post_event(State(app.clone()), Path(id.clone()), Json(ev)).await.unwrap();
        post_complete(State(app.clone()), Path(id.clone()), Json(CompleteIn { result_summary: "第一版".into(), relay_from: None }))
            .await
            .unwrap();

        let Json(child) = post_revise(State(app.clone()), Path(id.clone()), Json(rev("順便把標題也改了", "rev1"))).await.unwrap();
        assert_eq!(child["created"], true);
        let cid = child["id"].as_str().unwrap().to_string();
        assert_ne!(cid, id, "是新的一筆，不是把舊的打開");
        assert_eq!(child["parent_mission_id"], json!(id));
        assert_eq!(child["status"], "open");

        // 舊那筆完全沒被動到。
        let Json(parent) = get_mission(State(app.clone()), Path(id.clone())).await.unwrap();
        assert_eq!(parent["status"], "done");
        assert_eq!(parent["result_summary"], "第一版");
        assert_eq!(parent["revisions"][0]["id"], json!(cid), "成果卡連得到續作");
        let Json(kid) = get_mission(State(app.clone()), Path(cid.clone())).await.unwrap();
        assert_eq!(kid["parent"]["id"], json!(id), "續作也連得回來源");

        // 舊的 verified 不屬於新任務：不重新驗證就交付要被擋下來。
        let deliver = DeliverIn { worktree: env.repo.to_string_lossy().to_string(), title: None, body: None, relay_from: None };
        let err = post_deliver(State(app.clone()), Path(cid.clone()), Json(deliver)).await.unwrap_err();
        assert_eq!(conflict_reason(err), "not_verified", "舊證據不能放行新交付");

        // 脈絡是持久化的快照：原指示／摘要／驗證摘要都在新任務的 instruction 事件裡，
        // 所以原本那顆臨時 bot 被清掉也不影響續作。
        let snap: String = sqlx::query_scalar(
            "SELECT payload_json FROM mission_events WHERE mission_id = ? AND kind = 'instruction'",
        )
        .bind(&cid)
        .fetch_one(&app.db)
        .await
        .unwrap();
        let snap: Value = serde_json::from_str(&snap).unwrap();
        assert_eq!(snap["parent_mission_id"], json!(id));
        assert_eq!(snap["parent_result_summary"], "第一版");
        assert!(snap["parent_verified"]["text"].as_str().unwrap().contains("cargo test"));
        assert!(snap["evidence_note"].as_str().unwrap().contains("不是驗證證據"));
        // 原成果現在在不在基底裡，daemon 查不到（push 可能被 revert、PR 可能已合併），
        // 所以只說 unknown 並要求動手前自己查——不宣稱「PR 尚未合入」。
        assert_eq!(snap["parent_delivery_in_main"], "unknown");
        // 這筆 fixture 根本沒交付過，所以連「用哪種方式」都沒有——那就誠實地留 null，
        // 不要拿任務設定的 delivery_mode 冒充「已經這樣交付過」。
        assert_eq!(snap["parent_delivery_mode"], Value::Null);
        assert!(snap["caveat"].as_str().unwrap().contains("不要假設"));
        // 驗證證據要連 payload 一起帶（截圖路徑在 payload 裡）。
        assert_eq!(snap["parent_verified"]["payload"]["shots"], json!(["/tmp/a.png"]));

        // inbox 帶 parent 與快照，AGM 的 runbook 從第 2 步接手。
        let payload: String = sqlx::query_scalar("SELECT payload_json FROM supervisor_inbox WHERE event_key = ?")
            .bind(format!("mission:{cid}:created"))
            .fetch_one(&app.db)
            .await
            .unwrap();
        let payload: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(payload["parent_mission_id"], json!(id));
        assert_eq!(payload["runbook_start_step"], 2);

        // 同 crid 重送回同一筆；換內容是 409。
        let Json(again) = post_revise(State(app.clone()), Path(id.clone()), Json(rev("順便把標題也改了", "rev1"))).await.unwrap();
        assert_eq!(again["id"], json!(cid));
        assert_eq!(again["created"], false);
        let err = post_revise(State(app.clone()), Path(id.clone()), Json(rev("不一樣的要求", "rev1"))).await.unwrap_err();
        assert_eq!(conflict_reason(err), "request_id_reused");
    }

    /// 續作只能從**已完成**的成果開。進行中與已取消各自回明確理由。
    #[tokio::test]
    async fn only_a_finished_mission_can_be_revised() {
        let env = crate::team::testing::env().await;
        let app = env.app.clone();
        let pid = env.project_id.clone();
        let Json(m) = post_mission(State(app.clone()), Path(pid.clone()), Json(new_mission("r1", "pr"))).await.unwrap();
        let id = m["id"].as_str().unwrap().to_string();
        let err = post_revise(State(app.clone()), Path(id.clone()), Json(rev("改這個", "rev1"))).await.unwrap_err();
        assert_eq!(conflict_reason(err), "not_completed");

        post_cancel(State(app.clone()), Path(id.clone())).await.unwrap();
        let err = post_revise(State(app.clone()), Path(id.clone()), Json(rev("改這個", "rev2"))).await.unwrap_err();
        assert_eq!(conflict_reason(err), "not_completed");
    }

    #[tokio::test]
    async fn a_mission_runs_through_its_gates_end_to_end() {
        let env = crate::team::testing::env().await;
        let app = env.app.clone();
        let pid = env.project_id.clone();

        // 建立，且同一個 request id 不會建第二筆。
        let Json(m) = post_mission(State(app.clone()), Path(pid.clone()), Json(new_mission("r1", "push_main"))).await.unwrap();
        assert_eq!(m["created"], true);
        assert_eq!(status(&m), "open");
        let id = m["id"].as_str().unwrap().to_string();
        let Json(again) = post_mission(State(app.clone()), Path(pid.clone()), Json(new_mission("r1", "push_main"))).await.unwrap();
        assert_eq!(again["created"], false);
        assert_eq!(again["id"], m["id"]);

        // AGM 的 inbox 收到一則，只有一則。
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE kind = 'mission_created' AND event_key = ?")
            .bind(format!("mission:{id}:created"))
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(n, 1);

        // 驗證者找不到 Fable 額度 → ask_user，任務停下來（D6）。
        let q = |role: &str| HashMap::from([("role".to_string(), role.to_string())]);
        let Json(p) = get_pick(State(app.clone()), Path(id.clone()), Query(q("verifier"))).await.unwrap();
        assert_eq!(p["pick"]["decision"], "ask_user");
        let Json(cur) = get_mission(State(app.clone()), Path(id.clone())).await.unwrap();
        assert_eq!(status(&cur), "paused");
        assert_eq!(cur["paused_reason"], "no_fable_for_verifier");

        // 有 Fable 額度之後挑得到；cc2 被停用就往下挑 cc1。
        let _ = post_resume(State(app.clone()), Path(id.clone())).await.unwrap();
        {
            let mut qs = app.quotas.lock().await;
            qs.insert("claude:cc2".into(), quota(10.0));
            qs.insert("claude:cc1".into(), quota(20.0));
        }
        let Json(p) = get_pick(State(app.clone()), Path(id.clone()), Query(q("verifier"))).await.unwrap();
        assert_eq!(p["pick"]["identity"], "cc2");
        assert_eq!(p["pick"]["model"], "fable");
        let _ = put_identity_disabled(State(app.clone()), Path("cc2".into()), Json(DisableIn { kind: "claude".into(), disabled: true, host: None }))
            .await
            .unwrap();
        let Json(p) = get_pick(State(app.clone()), Path(id.clone()), Query(q("executor"))).await.unwrap();
        assert_eq!(p["pick"]["identity"], "cc1");

        // 輪數上限：兩輪之後第三輪停下來。
        let _ = post_round(State(app.clone()), Path(id.clone())).await.unwrap();
        let _ = post_round(State(app.clone()), Path(id.clone())).await.unwrap();
        let err = post_round(State(app.clone()), Path(id.clone())).await.unwrap_err();
        assert_eq!(conflict_reason(err), "max_rounds");
        let Json(cur) = get_mission(State(app.clone()), Path(id.clone())).await.unwrap();
        assert_eq!(cur["paused_reason"], "max_rounds");
        let _ = post_resume(State(app.clone()), Path(id.clone())).await.unwrap();

        // 沒有驗證通過不能交付。
        let deliver = || DeliverIn { worktree: env.repo.to_string_lossy().to_string(), title: None, body: None, relay_from: None };
        let err = post_deliver(State(app.clone()), Path(id.clone()), Json(deliver())).await.unwrap_err();
        assert_eq!(conflict_reason(err), "not_verified");

        // 來源不能冒名；daemon 哨符可以。
        let bogus = EventIn { kind: "verified".into(), text: "ok".into(), relay_from: Some("no-such-bot".into()), payload: None };
        assert!(matches!(post_event(State(app.clone()), Path(id.clone()), Json(bogus)).await, Err(LcError::Bad(_))));
        let ok = EventIn { kind: "verified".into(), text: "cargo test 全過".into(), relay_from: Some("daemon".into()), payload: None };
        let _ = post_event(State(app.clone()), Path(id.clone()), Json(ok)).await.unwrap();

        // 測試 repo 沒有 origin：交付失敗 → 停下來問人（D8），不是靜靜吞掉。
        let err = post_deliver(State(app.clone()), Path(id.clone()), Json(deliver())).await.unwrap_err();
        assert!(matches!(err, LcError::Conflict(_)));
        let Json(cur) = get_mission(State(app.clone()), Path(id.clone())).await.unwrap();
        assert_eq!(cur["paused_reason"], "push_main_failed");

        // 完成之後就關起來；已完成任務清單查得到。
        let _ = post_complete(State(app.clone()), Path(id.clone()), Json(CompleteIn { result_summary: "修好了".into(), relay_from: None })).await.unwrap();
        let err = post_pause(State(app.clone()), Path(id.clone()), Json(PauseIn { reason: "late".into(), detail: None })).await.unwrap_err();
        assert_eq!(conflict_reason(err), "already_closed");
        let Json(done) = get_missions(State(app.clone()), Path(pid.clone()), Query(ListQuery { status: Some("done".into()), limit: None })).await.unwrap();
        assert_eq!(done["missions"].as_array().unwrap().len(), 1);

        let Json(full) = get_mission(State(app.clone()), Path(id.clone())).await.unwrap();
        let kinds: Vec<&str> = full["events"].as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds.first(), Some(&"instruction"));
        assert!(kinds.contains(&"verified") && kinds.contains(&"completed"));
        let instruction = &full["events"][0];
        assert!(instruction["relay_from"].is_null(), "使用者下的指示不帶來源標");
    }

    #[test]
    fn temp_bot_names_are_tied_to_the_mission_tail() {
        let id = "01M2CDAG4QY36YGVJMJK8Q11SA";
        assert!(is_temp_bot_name(id, "agm-mission-8q11sa-exec"));
        assert!(is_temp_bot_name(id, "agm-mission-8Q11SA-verify"));
        assert!(is_temp_bot_name(id, "agm-mission-Q11SA-review"), "相容 5 碼的舊命名");
        assert!(!is_temp_bot_name(id, "agm-mission-XXXXXX-exec"), "別的任務的臨時 bot");
        assert!(!is_temp_bot_name(id, "agm-mission-8q11sa"), "沒有角色段");
        assert!(!is_temp_bot_name(id, "c1-主要功能"));
        assert!(!is_temp_bot_name(id, "agm-mission-1sa-exec"), "尾碼太短，容易撞到別的任務");
    }

    #[tokio::test]
    async fn completing_a_mission_deletes_only_its_stopped_temp_bots() {
        let env = crate::team::testing::env().await;
        let app = env.app.clone();
        crate::supervisor::store::get_or_init(&app.db).await.unwrap();
        let Json(m) = post_mission(State(app.clone()), Path(env.project_id.clone()), Json(new_mission("cleanup", "pr"))).await.unwrap();
        let id = m["id"].as_str().unwrap().to_string();
        let tail = id[id.len() - 6..].to_ascii_lowercase();
        let now = crate::db::now();
        let bots = [
            ("t-exec", format!("agm-mission-{tail}-exec"), false),
            ("t-verify", format!("agm-mission-{tail}-verify"), true),
            ("regular", "c1-主要功能".to_string(), false),
        ];
        for (bid, name, running) in &bots {
            sqlx::query("INSERT INTO bots (id,project_id,name,kind,managed_by,hook_token,created_at) VALUES (?,?,?,'claude','child','t',?)")
                .bind(bid)
                .bind(&env.project_id)
                .bind(name)
                .bind(&now)
                .execute(&app.db)
                .await
                .unwrap();
            if *running {
                sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES (?,?,'running','idle',?)")
                    .bind(format!("run-{bid}"))
                    .bind(bid)
                    .bind(&now)
                    .execute(&app.db)
                    .await
                    .unwrap();
            }
            let a = crate::supervisor::store::insert_assignment(&app.db, None, bid, &format!("crid-{bid}"), "做 X", &[], None, true).await.unwrap();
            crate::supervisor::store::set_mission_link(&app.db, &a.id, &id, "executor").await.unwrap();
        }

        let Json(done) = post_complete(State(app.clone()), Path(id.clone()), Json(CompleteIn { result_summary: "完成".into(), relay_from: None }))
            .await
            .unwrap();
        let deleted: Vec<&str> = done["temp_bots"]["deleted"].as_array().unwrap().iter().map(|b| b["bot_id"].as_str().unwrap()).collect();
        assert_eq!(deleted, ["t-exec"]);
        let skipped: Vec<(&str, &str)> = done["temp_bots"]["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| (b["bot_id"].as_str().unwrap(), b["reason"].as_str().unwrap()))
            .collect();
        assert!(skipped.contains(&("t-verify", "still_running")));
        assert!(skipped.contains(&("regular", "not_a_temp_bot")));

        let gone = |bid: &'static str| {
            let db = app.db.clone();
            async move {
                sqlx::query_scalar::<_, Option<String>>("SELECT deleted_at FROM bots WHERE id = ?").bind(bid).fetch_one(&db).await.unwrap().is_some()
            }
        };
        assert!(gone("t-exec").await);
        assert!(!gone("t-verify").await, "還在跑的不能刪");
        assert!(!gone("regular").await, "使用者自己的 bot 不碰");
        let Json(full) = get_mission(State(app.clone()), Path(id.clone())).await.unwrap();
        assert!(full["events"].as_array().unwrap().iter().any(|e| e["kind"] == "note" && e["text"].as_str().unwrap().contains("已刪除臨時 bot")));
    }

    #[tokio::test]
    async fn bad_options_and_remote_projects_are_rejected() {
        let env = crate::team::testing::env().await;
        let app = env.app.clone();
        let mut bad = new_mission("r2", "push_main");
        bad.delivery_mode = "force".into();
        assert!(matches!(post_mission(State(app.clone()), Path(env.project_id.clone()), Json(bad)).await, Err(LcError::Bad(_))));

        sqlx::query("UPDATE projects SET host = 'build-box' WHERE id = ?").bind(&env.project_id).execute(&app.db).await.unwrap();
        match post_mission(State(app.clone()), Path(env.project_id.clone()), Json(new_mission("r3", "pr"))).await {
            Err(LcError::BadValue(v)) => assert_eq!(v["error"], "remote_not_supported"),
            other => panic!("expected remote_not_supported, got {:?}", other.map(|j| j.0)),
        }
    }
}
