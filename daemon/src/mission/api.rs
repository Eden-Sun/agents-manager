//! `/api/projects/{id}/missions`、`/api/missions/*`、`/api/identities/{name}/disabled`。
//! 契約寫在 `docs/API.md` 的「群組任務」一節；這支檔案改了那一節要跟著改。

use super::{deliver, flow, pick, store};
use crate::lifecycle::LcError;
use crate::state::App;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
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

/// 放行之後的那一步（不看暫停）。放進 `mission_resumed`／`mission_answered`，AGM 被叫醒時就知道
/// 從哪裡接續，不必回頭翻 runbook 對「停下之前做到哪」（§18.14 第 8 步）。
async fn resume_step(app: &Arc<App>, id: &str) -> Result<Value, LcError> {
    let (assignments, events) = super::workflow::inputs(app, id).await?;
    Ok(json!(flow::derive(&assignments, &events).step()))
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
    let (assignments, events) = super::workflow::inputs(app, &m.id).await?;
    let mut out = m.json();
    out["phase"] = phase(m, &assignments).into();
    // 下一步由 daemon 從持久狀態推導（issue #74）：AGM 照 `next` 做，不必自己記 runbook 的順序。
    let f = flow::derive(&assignments, &events);
    out["next"] = json!(f.next(m));
    out["flow"] = f.summary();
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
        // 遠端要另外設計交付路徑，先明確拒絕。
        return Err(LcError::BadValue(json!({"error": "remote_not_supported", "host": project.host})));
    }
    let crid = b.client_request_id.clone().unwrap_or_else(crate::db::ulid);
    let payload_of = |m: &store::Mission| {
        json!({
            "mission_id": m.id,
            "project_id": project.id,
            "project": project.label,
            "cwd": project.path,
            "text": m.text,
            "delivery_mode": m.delivery_mode,
            "executor_kind": m.executor_kind,
            "on_5h_limit": m.on_5h_limit,
            "max_rounds": m.max_rounds,
        })
    };
    // 任務列、instruction 與 `mission_created` 同一個交易（store::create_announced）。
    let (m, created) = store::create_announced(
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
        payload_of,
    )
    .await
    .map_err(up)?;
    if created {
        emit(&app, &m).await;
    } else {
        // 重送也補推一次：交易上線以前寫一半的舊列（任務在、通知不在）靠這裡補回來。
        // event_key 相同，已經有的（含已 ack 的）不會多一筆、不會再叫醒誰。
        crate::supervisor::store::push_inbox(&app.db, &format!("mission:{}:created", m.id), "mission_created", None, None, None, &payload_of(&m))
            .await
            .map_err(up)?;
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
    let mut payload = json!({
        "mission_id": m.id,
        "project_id": m.project_id,
        "answer": text,
        "reply_to": reply_to,
        "from": from,
    });
    if !is_bot_reply {
        payload["next"] = resume_step(&app, &id).await?;
    }
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
    /// `verified` 驗的是哪個 commit（可縮寫）。跟 `worktree` 至少給一個。
    #[serde(default)]
    sha: Option<String>,
    /// `verified`：驗證者驗的那個工作樹，daemon 自己讀它的 HEAD。
    #[serde(default)]
    worktree: Option<String>,
}

/// 這個專案的本機路徑（交付與驗證都只收同一個 repo 的工作樹）。
async fn project_repo(app: &Arc<App>, m: &store::Mission) -> Result<std::path::PathBuf, LcError> {
    let p = crate::db::project(&app.db, &m.project_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("project".into()))?;
    Ok(std::path::PathBuf::from(p.path))
}

/// 呼叫端給的工作樹：本機絕對路徑、存在、而且跟任務的專案是同一個 repo。
async fn mission_worktree(app: &Arc<App>, m: &store::Mission, raw: &str) -> Result<std::path::PathBuf, LcError> {
    let dir = std::path::PathBuf::from(raw.trim());
    if !dir.is_absolute() || !dir.is_dir() {
        return Err(LcError::Bad("worktree must be an existing absolute path".into()));
    }
    let repo = project_repo(app, m).await?;
    if !deliver::same_repo(&dir, &repo).await {
        return Err(LcError::Bad(format!("worktree is not a checkout of this mission's project ({})", repo.display())));
    }
    Ok(dir)
}

/// `verified` 一定要說清楚驗的是哪個 commit，交付時才比得出「推上去的就是驗過的那一個」（review3 c1 M9）。
///
/// 以前只要有任何一則 `verified` 就放行：驗證者在 A 上驗過，執行者 rebase 成 B（可能含衝突解法），
/// B 沒經過驗證者就被推上 main。回傳完整 sha，與給了的話那個工作樹（交付的下一步會提到它）。
async fn verified_commit(app: &Arc<App>, m: &store::Mission, b: &EventIn) -> Result<(String, Option<String>), LcError> {
    let given = b
        .sha
        .as_deref()
        .or_else(|| b.payload.as_ref().and_then(|p| p.get("sha")).and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let mut worktree = None;
    let from_worktree = match b.worktree.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(w) => {
            let dir = mission_worktree(app, m, w).await?;
            worktree = Some(dir.to_string_lossy().to_string());
            Some(deliver::head_sha(&dir).await.ok_or_else(|| LcError::Bad("could not read the worktree's HEAD".into()))?)
        }
        None => None,
    };
    let repo = project_repo(app, m).await?;
    let from_sha = match given {
        Some(sha) => Some(
            deliver::resolve_commit(&repo, sha)
                .await
                .ok_or_else(|| LcError::Bad(format!("sha `{sha}` is not a commit in this project's repo")))?,
        ),
        None => None,
    };
    match (from_worktree, from_sha) {
        (Some(head), Some(sha)) if head != sha => Err(LcError::Bad(format!("sha {sha} is not the worktree's HEAD ({head})"))),
        (Some(head), _) => Ok((head, worktree)),
        (None, Some(sha)) => Ok((sha, None)),
        (None, None) => Err(LcError::Bad("a verified event needs the commit it verified: `worktree` (its HEAD) or `sha`".into())),
    }
}

/// AGM／bot 往群組時間軸回報（`report`、`note`），或記下驗證通過（`verified`，交付前必須有，而且要帶 commit）。
pub async fn post_event(State(app): State<Arc<App>>, Path(id): Path<String>, Json(b): Json<EventIn>) -> Result<Json<Value>, LcError> {
    one_of("kind", &b.kind, &["report", "note", "verified"])?;
    if b.text.trim().is_empty() {
        return Err(LcError::Bad("text is empty".into()));
    }
    let m = load(&app, &id).await?;
    ensure_open(&m)?;
    let from = check_relay_from(&app, b.relay_from.as_deref()).await?;
    let mut payload = b.payload.clone().unwrap_or_else(|| json!({}));
    if b.kind == "verified" {
        let (sha, worktree) = verified_commit(&app, &m, &b).await?;
        // 這則驗證屬於哪一代、寫下時最後一件交辦是誰（`flow::ANCHOR`）：之後退回或再派執行者，它就不算數了。
        let (assignments, events) = super::workflow::inputs(&app, &id).await?;
        let Some(obj) = payload.as_object_mut() else { return Err(LcError::Bad("payload must be an object".into())) };
        obj.insert("sha".into(), sha.into());
        obj.insert(flow::ANCHOR.into(), json!(assignments.last().map(|a| a.id.clone())));
        obj.insert("generation".into(), json!(events.iter().filter(|e| e.kind == "round").count()));
        if let Some(w) = worktree {
            obj.insert("worktree".into(), w.into());
        }
    }
    let ev = store::add_event(&app.db, &id, &b.kind, b.text.trim(), from.as_deref(), &payload)
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

/// 呼叫的是不是**收 mission 事件的那個 AGM 角色**自己（bot token 驗過）。是的話就不推 inbox——
/// 自己叫醒自己只是多一個空回合（同 `resume` 的防線）。有協調者時收件人是協調者；只有巡檢時是巡檢。
/// 使用者（web）、巡檢代使用者操作時都要叫醒協調者。
async fn called_by_mission_manager(app: &Arc<App>, headers: &HeaderMap) -> bool {
    use crate::supervisor::roles::{self, Role};
    match crate::supervisor::bot_requests::actor_role(app, headers).await {
        Some(Role::Responder) => true,
        Some(Role::Patrol) => !roles::responder_configured(&app.db).await.unwrap_or(true),
        None => false,
    }
}

/// 這個任務底下還開著的交辦（`supervisor::store::OPEN_STATES`），給 AGM 看的精簡形狀。
async fn open_assignments(app: &Arc<App>, mission_id: &str) -> Result<Vec<crate::supervisor::store::Assignment>, LcError> {
    Ok(crate::supervisor::store::mission_assignments(&app.db, mission_id).await.map_err(up)?.into_iter().filter(|a| a.is_open()).collect())
}

fn assignment_brief(a: &crate::supervisor::store::Assignment) -> Value {
    json!({"id": a.id, "role": a.mission_role, "status": a.status, "target_bot_id": a.target_bot_id, "turn_id": a.turn_id})
}

/// 暫停任務。使用者（或巡檢代為）按下時**同一個交易**叫醒協調者（`mission_paused`），review3 c1 M10：
/// 以前只改任務列，AGM 照 runbook 繼續 review → verify → deliver，暫停形同虛設。
///
/// 暫停不中止已經在跑的回合（daemon 不中止回合），也不取消交辦；它要 AGM 別再派新的、別交付
/// （`deliver` 在使用者暫停時回 409 `mission_paused`）。
pub async fn post_pause(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(b): Json<PauseIn>,
) -> Result<Json<Value>, LcError> {
    let reason = b.reason.trim();
    if reason.is_empty() {
        return Err(LcError::Bad("reason is empty".into()));
    }
    let m = load(&app, &id).await?;
    ensure_open(&m)?;
    let text = match b.detail.as_deref() {
        Some(d) => format!("暫停：{reason}（{d}）"),
        None => format!("暫停：{reason}"),
    };
    let paused_key = format!("mission:{id}:paused");
    let announce = if called_by_mission_manager(&app, &headers).await {
        None
    } else {
        let open = open_assignments(&app, &id).await?;
        Some(store::Announce {
            event_key_prefix: &paused_key,
            kind: "mission_paused",
            payload: json!({
                "mission_id": id,
                "project_id": m.project_id,
                "reason": reason,
                "detail": b.detail,
                "open_assignments": open.iter().map(assignment_brief).collect::<Vec<_>>(),
                "note": "任務被暫停：不要再派新交辦、不要交付（deliver 會 409 mission_paused）。已經在跑的回合不會被中止；等 mission_resumed／mission_answered 再從對應步驟接續。",
            }),
        })
    };
    if !store::pause_announced(&app.db, &id, reason, b.detail.as_deref(), &text, announce).await.map_err(up)? {
        return Err(ensure_open(&load(&app, &id).await?).err().unwrap_or_else(|| LcError::conflict("already_closed", json!({"mission_id": id}))));
    }
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
    let next = resume_step(&app, &id).await?;
    // 放行、記事件、推 inbox 一次交易。分開寫的話，中途失敗就會留下「已經放行但沒人被叫醒」，
    // 而且沒有任何東西會回頭補送。
    store::resume_and_wake(&app.db, &id, |_ev| {
        json!({
            "mission_id": id,
            "project_id": project_id,
            "was_paused_for": paused_reason,
            "answered": false,
            "next": next,
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
    let from = check_relay_from(&app, b.relay_from.as_deref()).await?;
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
        "requested_by": from,
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
    let fingerprint = json!([
        store::revise_fingerprint(&parent.id, text, &delivery_mode, &executor_kind, &on_5h_limit, max_rounds),
        from,
    ]).to_string();
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

/// 取消任務（review3 c1 M10）：
/// 1. 任務列、`cancelled` 事件、叫醒協調者（`mission_cancelled`；AGM 自己取消時不推）同一個交易；
/// 2. 底下還開著的交辦逐件走 supervisor 的 `cancel` 決定（同 `POST /supervisor/assignments/{id}/review`）：
///    排隊中的 turn 撤回、`quota_blocked` 不再被 controller 自動重送。以前原封不動，幾小時後額度回來，
///    已取消的工作又被送進臨時 bot 開始做；
/// 3. 收掉臨時 bot（還在跑的留著，AGM `bot stop` 再 `bot delete`）。
///
/// 已經在跑的回合 daemon 不會中止，回應的 `assignments[].may_still_be_running` 照實講。
pub async fn post_cancel(State(app): State<Arc<App>>, Path(id): Path<String>, headers: HeaderMap) -> Result<Json<Value>, LcError> {
    let m = load(&app, &id).await?;
    ensure_open(&m)?;
    let by_manager = called_by_mission_manager(&app, &headers).await;
    let before = open_assignments(&app, &id).await?;
    let announce = (!by_manager).then(|| store::Announce {
        event_key_prefix: "",
        kind: "mission_cancelled",
        payload: json!({
            "mission_id": id,
            "project_id": m.project_id,
            "cancelled_assignments": before.iter().map(assignment_brief).collect::<Vec<_>>(),
            "note": "使用者取消了任務：daemon 已把底下未結案的交辦取消（排隊中的撤回、等額度的不再重送）。還在跑的回合不會被中止——回應／note 裡 still_running 的臨時 bot 請 bot stop 再 bot delete。不要再派工或交付。",
        }),
    });
    let key = format!("mission:{id}:cancelled");
    let announce = announce.map(|a| store::Announce { event_key_prefix: &key, ..a });
    // 關任務這一步跟派工（`supervisor::assign`、followup）拿同一把 supervisor 鎖（issue #119）：派工在鎖裡
    // 重看任務關了沒，所以要嘛它先建好交辦、下面重讀收得到，要嘛它拿到鎖時任務已經關了、回 mission_closed。
    // 鎖只包這一步：底下逐件取消走 `post_review`，它自己會拿同一把鎖。
    let cancelled = {
        let _g = crate::supervisor::lock().await;
        store::cancel_announced(&app.db, &id, announce).await.map_err(up)?
    };
    if !cancelled {
        return Err(LcError::conflict("already_closed", json!({"mission_id": id})));
    }
    // 取消之後才重讀：鎖外排隊的派工拿到鎖時會看到任務已關，不會在這之後冒出新的交辦。
    let mut withdrawn = Vec::new();
    for a in open_assignments(&app, &id).await? {
        let review = crate::supervisor::api::ReviewIn {
            decision: "cancel".into(),
            actor: Some(if by_manager { "agm".into() } else { "user".into() }),
            source: Some("mission_cancel".into()),
            reason: Some(format!("群組任務 {id} 已取消")),
            evidence: None,
            followup_text: None,
            followup_request_id: None,
            followup_bot_id: None,
            ownership: Vec::new(),
        };
        let mut row = assignment_brief(&a);
        match crate::supervisor::api::post_review(State(app.clone()), Path(a.id.clone()), headers.clone(), Json(review)).await {
            Ok(Json(v)) => {
                row["cancelled"] = true.into();
                row["revoked_turn_id"] = v.get("revoked_turn_id").cloned().unwrap_or(Value::Null);
                row["may_still_be_running"] = v.get("may_still_be_running").cloned().unwrap_or(false.into());
            }
            Err(e) => {
                tracing::warn!(mission = %id, assignment = %a.id, error = ?e, "could not cancel a cancelled mission's assignment");
                row["cancelled"] = false.into();
                row["error"] = format!("{e:?}").into();
            }
        }
        withdrawn.push(row);
    }
    if !withdrawn.is_empty() {
        let n = withdrawn.iter().filter(|r| r["cancelled"] == true).count();
        let running = withdrawn.iter().filter(|r| r["may_still_be_running"] == true).count();
        let mut text = format!("已取消底下 {n} 件未結案的交辦");
        if running > 0 {
            text.push_str(&format!("（其中 {running} 件的回合可能還在跑，daemon 不會中止它）"));
        }
        let _ = store::add_event(&app.db, &id, "note", &text, Some(crate::agent_relay::DAEMON_SENDER), &json!({"assignments": withdrawn})).await;
    }
    let m = load(&app, &id).await?;
    let temp = cleanup_temp_bots(&app, &m).await;
    emit(&app, &m).await;
    let mut out = m.json();
    out["temp_bots"] = temp;
    out["assignments"] = json!(withdrawn);
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
    /// 沒交付就結案時必須講為什麼：`no_changes` | `user_declined`（`flow::Waiver`）。有交付時不看。
    #[serde(default)]
    no_delivery: Option<String>,
    /// `no_delivery=no_changes` 而且任務派過執行者時：執行者的工作樹，daemon 自己查它乾淨、HEAD 已在 base 裡。
    #[serde(default)]
    worktree: Option<String>,
}

/// 結案。兩道關卡（issue #74），判定結果寫進 `completed` 事件的 `payload.delivery`，時間軸上看得出
/// 「交了哪個 commit」或「為什麼沒交」：
/// 1. 底下還有開著的交辦 → 409 `assignments_open`（那顆 bot 會繼續做一件已經關掉的任務）；
/// 2. 對交付的要求（`flow::delivery_requirement`）：這一代驗過的 commit 已經交付，或 `no_delivery` 講的理由
///    對得上事實——否則 409 `not_delivered`／`has_verified_changes`／`user_not_asked`／`worktree_has_changes`。
pub async fn post_complete(State(app): State<Arc<App>>, Path(id): Path<String>, Json(b): Json<CompleteIn>) -> Result<Json<Value>, LcError> {
    if b.result_summary.trim().is_empty() {
        return Err(LcError::Bad("result_summary is empty".into()));
    }
    let waiver = match b.no_delivery.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(w) => Some(flow::Waiver::parse(w).ok_or_else(|| LcError::Bad(format!("no_delivery must be one of {}", flow::Waiver::ALL.join(" | "))))?),
        None => None,
    };
    let m = load(&app, &id).await?;
    ensure_open(&m)?;
    // 底下還有開著的交辦就不結案（issue #74）：那顆 bot 會繼續做一件已經關掉的任務，
    // 回合結束還會為它推一則沒有人要的 `assignment_completed`。取消那條路本來就會逐件收乾淨。
    crate::mission::workflow::ensure_can_complete(&app, &id).await?;
    let (mut delivery, needs_proof) = crate::mission::workflow::delivery_record(&app, &id, waiver).await?;
    if needs_proof {
        // 派過執行者卻說「沒有改東西」：拿執行者的工作樹來看，不靠一句話。
        let Some(raw) = b.worktree.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
            return Err(LcError::Bad("no_delivery=no_changes on a mission that had an executor needs `worktree` (the executor's) to show nothing changed".into()));
        };
        let dir = mission_worktree(&app, &m, raw).await?;
        let base = deliver::base_branch(&dir, "origin").await;
        match deliver::nothing_beyond_base(&dir, "origin", &base).await {
            Ok(head) => {
                delivery["worktree"] = dir.to_string_lossy().to_string().into();
                delivery["head"] = head.into();
            }
            Err(why) => {
                return Err(LcError::conflict(
                    "worktree_has_changes",
                    json!({"mission_id": id, "detail": why,
                           "hint": "執行者的工作樹有還沒交付的東西：走驗證與 `mission deliver`，或使用者決定不交付時用 `user_declined`"}),
                ));
            }
        }
    }
    let from = check_relay_from(&app, b.relay_from.as_deref()).await?;
    {
        // 關任務這一步跟派工拿同一把 supervisor 鎖，並在鎖裡重看「底下沒有開著的交辦」（issue #119）：
        // 上面的關卡（可能跑 git）是在鎖外做的，那段時間排隊的派工可能已經建了交辦。
        let _g = crate::supervisor::lock().await;
        crate::mission::workflow::ensure_can_complete(&app, &id).await?;
        // 任務列與 `completed` 事件一次交易，而且只在這一次真的把任務從開著關掉時才寫（issue #116）：
        // 上面的關卡跑完之前任務可能已經被取消、或另一個結案先落地——那就照實 409，不補一則「完成」。
        if store::complete(&app.db, &id, b.result_summary.trim(), from.as_deref(), &json!({"delivery": delivery})).await.map_err(up)?.is_none() {
            return Err(ensure_open(&load(&app, &id).await?).err().unwrap_or_else(|| LcError::conflict("already_closed", json!({"mission_id": id}))));
        }
    }
    let m = load(&app, &id).await?;
    let temp = cleanup_temp_bots(&app, &m).await;
    emit(&app, &m).await;
    let mut out = m.json();
    out["temp_bots"] = temp;
    out["delivery"] = delivery;
    Ok(Json(out))
}

/// 用掉一輪（review 退回或驗證失敗）。到上限就把任務停下來（`max_rounds`）並回 409。
///
/// 退回＝新的一代（`flow`）：事件記下當時最後一件交辦（`flow::ANCHOR`），之後派的才算這一代，
/// 之前的驗證與交付都不再放行。
pub async fn post_round(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Json<Value>, LcError> {
    let m = load(&app, &id).await?;
    ensure_open(&m)?;
    let last = crate::supervisor::store::mission_assignments(&app.db, &id).await.map_err(up)?.last().map(|a| a.id.clone());
    match store::use_round(&app.db, &id).await.map_err(up)? {
        Ok(used) => {
            store::add_event(&app.db, &id, "round", &format!("第 {used} 輪退回（上限 {}）", m.max_rounds), Some(crate::agent_relay::DAEMON_SENDER), &json!({"rounds_used": used, flow::ANCHOR: last}))
                .await
                .map_err(up)?;
            let m = load(&app, &id).await?;
            emit(&app, &m).await;
            Ok(Json(m.json()))
        }
        Err(used) => {
            let detail = format!("review／驗證已退回 {used} 輪，達到上限 {}", m.max_rounds);
            // `use_round` 在任務已經被關掉時也不加（它的 WHERE 帶「還開著」）：那不是輪數用完，停不下來就照實說
            // 任務已經關了，不要記一則「等使用者決定」、也不要回 max_rounds（issue #130）。
            if !store::pause_with_event(&app.db, &id, "max_rounds", Some(&detail), &format!("暫停：{detail}，等使用者決定"), &json!({"reason": "max_rounds"}))
                .await
                .map_err(up)?
            {
                return Err(ensure_open(&load(&app, &id).await?).err().unwrap_or_else(|| LcError::conflict("already_closed", json!({"mission_id": id}))));
            }
            emit(&app, &load(&app, &id).await?).await;
            Err(LcError::conflict(
                "max_rounds",
                json!({"mission_id": id, "rounds_used": used, "max_rounds": m.max_rounds,
                       "hint": "任務已停下來問使用者；使用者放行（answer／resume）就會多給一輪，那時再呼叫一次 round"}),
            ))
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
            // 停不下來（這中間任務被關掉了）就什麼都不記（issue #130）。
            if m.paused_reason.as_deref() != Some("no_fable_for_verifier")
                && store::pause_with_event(
                    &app.db,
                    &id,
                    "no_fable_for_verifier",
                    Some(reason),
                    &format!("暫停：{reason}，等使用者決定"),
                    &json!({"reason": "no_fable_for_verifier", "decision": decision}),
                )
                .await
                .map_err(up)?
            {
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

/// 交付失敗停下來的那兩種：交付成功就不再是事實，自動解除（不然卡片還寫著「等你決定」）。
const DELIVERY_PAUSES: [&str; 2] = ["push_main_failed", "pr_failed"];

/// 依任務的 `delivery_mode` 推 main（fast-forward only）或開 PR。
///
/// 關卡（都不改任務狀態，是呼叫端流程漏了，不是交付失敗）：
/// - 最新一則 `verified` 必須帶 commit（`not_verified`／`verified_without_sha`）；
/// - 工作樹必須是這個專案的 repo，而且 HEAD 就是驗過的那個 commit（`head_not_verified`）——
///   rebase、補改過之後要重新驗證，舊的 `verified` 不算數（review3 c1 M9）。
///
/// 過了關卡之後的失敗才把任務停下來問人（D8），回 409 帶機器碼。成功會解除先前的交付失敗暫停。
///
/// **冪等**（review3 c1 M11）：同一個 commit 已經交付過就回原本那一筆（`replayed`）。動手之前先記一則
/// `delivery_attempt`，所以「push 成功但回應斷在路上」的重試認得出來——HEAD 已經在 main 裡是成功，
/// 不是 `nothing_to_deliver`；PR 模式先問 `gh pr view`，不會被「已經有一個 PR」判成 `pr_failed`。
pub async fn post_deliver(State(app): State<Arc<App>>, Path(id): Path<String>, Json(b): Json<DeliverIn>) -> Result<Json<Value>, LcError> {
    let m = load(&app, &id).await?;
    ensure_open(&m)?;
    // 任務停著（使用者按了暫停、等人回答、輪數用完…）就不交付。只有先前那次交付失敗停下的可以重試（review3 c1 M10）。
    if let Some(reason) = m.paused_reason.as_deref().filter(|r| !DELIVERY_PAUSES.contains(r)) {
        return Err(LcError::conflict(
            "mission_paused",
            json!({"mission_id": id, "paused_reason": reason, "hint": "任務停著：等使用者回答或 resume 之後再交付"}),
        ));
    }
    let from = check_relay_from(&app, b.relay_from.as_deref()).await?;
    let (assignments, events) = super::workflow::inputs(&app, &id).await?;
    let f = flow::derive(&assignments, &events);
    let Some((verified, stale)) = f.latest_verified.clone() else {
        return Err(LcError::conflict("not_verified", json!({"mission_id": id, "next": f.step()})));
    };
    let Some(verified_sha) = verified.sha.clone() else {
        return Err(LcError::conflict(
            "verified_without_sha",
            json!({"mission_id": id, "event_id": verified.event_id,
                   "hint": "這則 verified 沒記是哪個 commit：請驗證者重驗後用 `mission event --kind verified --worktree <驗過的工作樹>` 重記"}),
        ));
    };
    // 驗證綁在它那一代的成果上（issue #74）：之後退回過、或又派了執行者，就算 HEAD 碰巧沒變也要重驗——
    // 否則被退回的那一份不必改就能靠舊驗證推上去。
    if let Some(why) = stale {
        return Err(LcError::conflict(
            "verification_stale",
            json!({"mission_id": id, "verified_sha": verified_sha, "stale_because": why,
                   "verified_generation": verified.generation, "generation": f.generation, "next": f.step(),
                   "hint": match why {
                       flow::Stale::Round => "這則 verified 之後任務被退回過（round）：那是上一代的驗證，執行者重做之後要重驗",
                       flow::Stale::NewExecutor => "這則 verified 之後又派了執行者（rebase／補改）：成果可能變了，重驗之後再交付",
                   }}),
        ));
    }
    let dir = mission_worktree(&app, &m, &b.worktree).await?;
    let head = deliver::head_sha(&dir).await.ok_or_else(|| LcError::Bad("could not read the worktree's HEAD".into()))?;
    if head != verified_sha {
        return Err(LcError::conflict(
            "head_not_verified",
            json!({"mission_id": id, "verified_sha": verified_sha, "head": head,
                   "hint": "工作樹的 HEAD 不是驗證過的那個 commit（rebase 或又改過）：回到驗證那一步重驗這個 commit，再交付"}),
        ));
    }
    // 這個 commit 已經交付過了：回原本那一筆，不要再推一次、也不要把它報成失敗。
    if let Some(done) = events.iter().rev().find(|e| e.kind == "delivered" && flow::delivered_sha(e).as_deref() == Some(head.as_str())) {
        let mut out: Value = serde_json::from_str(&done.payload_json).unwrap_or_else(|_| json!({}));
        out["replayed"] = true.into();
        out["delivered_at"] = done.created_at.clone().into();
        return Ok(Json(out));
    }
    // 動手之前先記一筆：push／PR 成功但回應（或 CLI 的 30 秒逾時）斷在路上時，重試才分得出
    // 「上一次其實成功了」與「執行者根本沒 commit」。
    let attempted_before = events.iter().any(|e| attempt_sha(e).as_deref() == Some(head.as_str()));
    if !attempted_before {
        let _ = store::add_event(
            &app.db,
            &id,
            "note",
            &format!("開始交付 {}（{}）", &head[..12.min(head.len())], m.delivery_mode),
            Some(crate::agent_relay::DAEMON_SENDER),
            &json!({"delivery_attempt": {"sha": head, "mode": m.delivery_mode}}),
        )
        .await;
    }
    // 交付的 base 從 repo 問（`origin/HEAD`），不是寫死 main：預設分支叫 master／trunk 的專案
    // 以前一定 `fetch_failed`。
    let base = deliver::base_branch(&dir, "origin").await;
    let result = if m.delivery_mode == "push_main" {
        deliver::push_main(&dir, "origin", &base, attempted_before)
            .await
            .map(|p| json!({"mode": "push_main", "sha": p.sha, "base": base, "already_in_base": p.already_in_base}))
    } else {
        let branch = format!("mission/{}", m.id.to_lowercase());
        let title = b.title.clone().unwrap_or_else(|| m.text.chars().take(72).collect());
        let body = b.body.clone().unwrap_or_else(|| format!("群組任務 {}\n\n{}", m.id, m.text));
        deliver::open_pr(&dir, "origin", &base, &branch, &title, &body)
            .await
            .map(|o| json!({"mode": "pr", "branch": branch, "base": base, "url": o.url, "sha": o.sha, "existing_pr": o.existing}))
    };
    match result {
        Ok(out) => {
            let sha = out["sha"].as_str().unwrap_or_default();
            let text = match (out["mode"].as_str(), out["already_in_base"] == json!(true), out["existing_pr"] == json!(true)) {
                // 重試時才會看到的兩種：先前那次其實做完了，只是沒記下來。
                (Some("push_main"), true, _) => format!("已在 {base} 上：{sha}（先前那次交付其實成功了，這次只補記）"),
                (Some("push_main"), false, _) => format!("已推上 {base}：{sha}"),
                (_, _, true) => format!("PR 早就開著了：{}", out["url"].as_str().unwrap_or_default()),
                _ => format!("已開 PR：{}", out["url"].as_str().unwrap_or_default()),
            };
            store::add_event(&app.db, &id, "delivered", &text, from.as_deref(), &out).await.map_err(up)?;
            // 先前那次交付失敗停下來的：現在交付成功了，那個暫停的理由已經不存在。
            if let Some(reason) = store::clear_pause_if(&app.db, &id, &DELIVERY_PAUSES).await.map_err(up)? {
                store::add_event(&app.db, &id, "resumed", &format!("交付成功，解除「{reason}」暫停"), Some(crate::agent_relay::DAEMON_SENDER), &json!({"was_paused_for": reason}))
                    .await
                    .map_err(up)?;
            }
            emit(&app, &load(&app, &id).await?).await;
            Ok(Json(out))
        }
        Err(f) => {
            let reason = if m.delivery_mode == "push_main" { "push_main_failed" } else { "pr_failed" };
            let text = format!("交付失敗（{}），等使用者決定：{}", f.code, f.detail);
            let paused = store::pause_with_event(&app.db, &id, reason, Some(&format!("{}：{}", f.code, f.detail)), &text, &json!({"reason": reason, "code": f.code}))
                .await
                .map_err(up)?;
            if !paused {
                // 交付途中任務被關掉了：沒有東西在等使用者決定，照實說任務已經關了，失敗原因一併附上（issue #130）。
                return Err(LcError::conflict("already_closed", json!({"mission_id": id, "delivery_failed": f.code, "detail": f.detail})));
            }
            emit(&app, &load(&app, &id).await?).await;
            Err(LcError::conflict(f.code, json!({"mission_id": id, "detail": f.detail})))
        }
    }
}

/// 一則 `note` 記的「開始交付」是哪個 commit。
fn attempt_sha(e: &store::MissionEvent) -> Option<String> {
    serde_json::from_str::<Value>(&e.payload_json).ok()?.pointer("/delivery_attempt/sha")?.as_str().map(str::to_string)
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


    fn done(summary: &str, no_delivery: Option<&str>, worktree: Option<&std::path::Path>) -> CompleteIn {
        CompleteIn {
            result_summary: summary.into(),
            relay_from: None,
            no_delivery: no_delivery.map(String::from),
            worktree: worktree.map(|w| w.to_string_lossy().to_string()),
        }
    }

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
        // `rowid` 當第二鍵的理由同 a4605b2：`created_at` 只到毫秒，`id` 是 ULID（同毫秒內亂數段不保證
        // 遞增）。今天每個呼叫點都只看筆數或單筆，順序翻了也不會紅——正因為如此，這裡更要寫對：
        // 下一個照抄這段的人多半會直接比整串。
        sqlx::query_scalar("SELECT event_key FROM supervisor_inbox WHERE event_key LIKE ? ORDER BY created_at, rowid")
            .bind(like)
            .fetch_all(&app.db)
            .await
            .unwrap()
    }

    /// 建任務、instruction、`mission_created` 一次交易；重送不多寫一筆，但會補推寫一半的舊列漏掉的通知（review 2026-09-16）。
    #[tokio::test]
    async fn a_resent_mission_announces_itself_once_even_if_the_first_write_was_cut_short() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let pid = env.project_id.clone();
        let count = |kind: &'static str, id: String| {
            let app = app.clone();
            async move {
                let n: i64 = if kind == "instruction" {
                    sqlx::query_scalar("SELECT COUNT(*) FROM mission_events WHERE mission_id=? AND kind='instruction'").bind(&id).fetch_one(&app.db).await.unwrap()
                } else {
                    sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE event_key=?").bind(format!("mission:{id}:created")).fetch_one(&app.db).await.unwrap()
                };
                n
            }
        };

        let Json(first) = post_mission(State(app.clone()), Path(pid.clone()), Json(new_mission("once", "pr"))).await.unwrap();
        let id = first["id"].as_str().unwrap().to_string();
        assert_eq!(first["created"], true);
        let Json(again) = post_mission(State(app.clone()), Path(pid.clone()), Json(new_mission("once", "pr"))).await.unwrap();
        assert_eq!((again["created"].as_bool(), again["id"].as_str()), (Some(false), Some(id.as_str())));
        assert_eq!((count("instruction", id.clone()).await, count("inbox", id.clone()).await), (1, 1), "重送不多寫");

        // 舊程式寫一半：任務列在、instruction 與通知都沒寫進去（三個各自 await 的語句之間掛掉）。
        let (half, created) = store::create(
            &app.db,
            &store::NewMission { project_id: &pid, client_request_id: "half", text: "寫一半的任務", delivery_mode: "pr",
                                 executor_kind: "claude", on_5h_limit: "switch", max_rounds: 2, parent_mission_id: None },
        )
        .await
        .unwrap();
        assert!(created);
        assert_eq!(count("inbox", half.id.clone()).await, 0);
        let Json(resent) = post_mission(State(app.clone()), Path(pid.clone()), Json(new_mission("half", "pr"))).await.unwrap();
        assert_eq!(resent["created"], false);
        assert_eq!(count("inbox", half.id.clone()).await, 1, "重送補推 mission_created，AGM 才知道有這件事");
        let payload: String = sqlx::query_scalar("SELECT payload_json FROM supervisor_inbox WHERE event_key=?")
            .bind(format!("mission:{}:created", half.id))
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert!(payload.contains("寫一半的任務"), "通知帶的是那筆任務自己的內容：{payload}");
    }

    /// 結案請求跑關卡的途中任務被別人關掉了（使用者按了取消、或另一個結案先落地）：這一次結案必須不成立。
    ///
    /// `post_complete` 先 `ensure_open`、中間跑完交付關卡（`no_changes` 時還要 git）才寫 `store::complete`。
    /// 以前不看 `complete` 回的 `false`：照樣補一則 `completed` 事件、清臨時 bot、回 200——時間軸上同時有
    /// 「已取消」與「完成」，或兩則說法不同的「完成」，呼叫端還以為自己結了案。
    ///
    /// 重現是確定性的：測試自己先拿寫入鎖（`BEGIN IMMEDIATE`）並把任務標成取消（還沒 commit）。結案請求的
    /// 讀取在 WAL 下照舊看到「還開著」、一路過關，卡在寫入那一步；放鎖之後它面對的就是一筆已取消的任務。
    #[tokio::test]
    async fn a_complete_that_loses_the_race_to_a_cancel_does_not_close_anything() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let Json(m) = post_mission(State(app.clone()), Path(env.project_id.clone()), Json(new_mission("complete-race", "pr"))).await.unwrap();
        let id = m["id"].as_str().unwrap().to_string();

        let mut writer = app.db.acquire().await.unwrap();
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *writer).await.unwrap();
        sqlx::query("UPDATE missions SET cancelled_at=?, updated_at=? WHERE id=?")
            .bind(crate::db::now())
            .bind(crate::db::now())
            .bind(&id)
            .execute(&mut *writer)
            .await
            .unwrap();
        let pending = tokio::spawn(post_complete(State(app.clone()), Path(id.clone()), Json(done("做完了", Some("no_changes"), None))));
        // 讓結案請求把讀取與關卡都跑完、卡在寫入（busy_timeout 10 秒，這裡遠小於它）。
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        sqlx::query("COMMIT").execute(&mut *writer).await.unwrap();
        drop(writer);

        let res = pending.await.unwrap();
        let completed: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mission_events WHERE mission_id=? AND kind='completed'")
            .bind(&id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(completed, 0, "已取消的任務不該多一則「完成」");
        let after = store::get(&app.db, &id).await.unwrap().unwrap();
        assert_eq!((after.status(), after.result_summary.as_deref()), ("cancelled", None));
        match res {
            Err(e) => assert_eq!(conflict_reason(e), "already_closed", "呼叫端要知道自己沒有結到案"),
            Ok(Json(v)) => panic!("結案沒有成立卻回了 200：{v}"),
        }
    }

    /// 派工排在 supervisor 鎖後面等（別的派送／裁示正握著它）的時候，任務被取消或結案了：
    /// 關掉的任務底下不能冒出一件開著的交辦（issue #119）。
    ///
    /// `post_assignment` 查「任務關了沒」是在拿鎖**之前**，而 `mission cancel`／`complete` 根本不拿這把鎖；
    /// 以前的順序是：派工讀到任務還開著 → 排隊等鎖 → 取消（或結案）落地、當下底下沒有交辦可收 → 派工拿到鎖、
    /// 照樣建交辦並派進臨時 bot。取消那條的註解說「這中間新開的交辦會被 mission_closed 擋掉」，擋的那一下在鎖外，擋不到。
    /// 兩種先後都要成立（先排隊的是派工、或先排隊的是關任務），followup 也會開新交辦，一樣要擋。
    #[tokio::test]
    async fn a_mission_closed_while_an_assignment_waits_for_the_lock_gets_no_new_work() {
        for (new_work, close) in [("assign", "cancel"), ("assign", "complete"), ("cancel", "assign"), ("complete", "assign"), ("followup", "cancel"), ("cancel", "followup")] {
            let label = format!("{new_work}→{close}");
            let env = crate::testing::env().await;
            let app = env.app.clone();
            crate::supervisor::store::get_or_init(&app.db).await.unwrap();
            let agm = crate::testing::claude_bot(&app, &env.project_id, "AGM").await;
            crate::supervisor::store::set_env(&app.db, &agm.id, &env.project_id, "/tmp").await.unwrap();
            let Json(m) = post_mission(State(app.clone()), Path(env.project_id.clone()), Json(new_mission(&format!("lock-{new_work}-{close}"), "pr"))).await.unwrap();
            let id = m["id"].as_str().unwrap().to_string();
            let worker = crate::testing::claude_bot(&app, &env.project_id, "worker").await;
            // followup 要有一件等裁示的原件可以接。
            let parent = if new_work == "followup" || close == "followup" {
                let out = crate::supervisor::assign(
                    &app, &worker.id, "做 X", "lock-parent", None, &[], None, true, Some((&id, "executor")), None, None,
                    crate::supervisor::bot_requests::ReplyMark::default(),
                )
                .await
                .unwrap();
                let pid = out["id"].as_str().unwrap().to_string();
                sqlx::query("UPDATE supervisor_assignments SET status='awaiting_review' WHERE id=?").bind(&pid).execute(&app.db).await.unwrap();
                Some(pid)
            } else {
                None
            };
            let spawn_step = |step: &str| {
                let (app, id, worker_id, parent) = (app.clone(), id.clone(), worker.id.clone(), parent.clone());
                match step {
                    "assign" => tokio::spawn(async move {
                        crate::supervisor::api::post_assignment(
                            State(app),
                            HeaderMap::new(),
                            Json(crate::supervisor::api::AssignIn {
                                target_bot_id: worker_id,
                                text: "做 X".into(),
                                client_request_id: "lock-exec".into(),
                                source_turn_id: None,
                                ownership: Vec::new(),
                                kind: None,
                                expects_review: None,
                                review_role: None,
                                mission_id: Some(id),
                                role: Some("executor".into()),
                                ack: false,
                                reply_to: None,
                            }),
                        )
                        .await
                    }),
                    "followup" => tokio::spawn(async move {
                        let review = crate::supervisor::api::ReviewIn {
                            decision: "followup".into(),
                            followup_text: Some("接著做".into()),
                            followup_request_id: Some("lock-followup".into()),
                            ..accept()
                        };
                        crate::supervisor::api::post_review(State(app), Path(parent.unwrap()), HeaderMap::new(), Json(review)).await
                    }),
                    "cancel" => tokio::spawn(async move { post_cancel(State(app), Path(id), HeaderMap::new()).await }),
                    _ => tokio::spawn(async move { post_complete(State(app), Path(id), Json(done("完成", Some("no_changes"), None))).await }),
                }
            };

            // 別的 supervisor 操作正握著鎖（派送一則 prompt 要走 herdr，握上幾秒很平常）。先送的那一步排到鎖後面，
            // 再送第二步，然後放鎖。
            let held = crate::supervisor::lock().await;
            let first = spawn_step(new_work);
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let second = spawn_step(close);
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            drop(held);
            let _ = first.await.unwrap();
            let _ = second.await.unwrap();

            let after = store::get(&app.db, &id).await.unwrap().unwrap();
            let open: Vec<String> = crate::mission::workflow::open_assignments(&app, &id).await.unwrap().into_iter().map(|a| a.status).collect();
            assert!(
                after.completed_at.is_none() && after.cancelled_at.is_none() || open.is_empty(),
                "{label}：任務已經是 {}，底下卻還有開著的交辦 {open:?}",
                after.status()
            );
        }
    }

    /// 請求跑到一半任務被取消了：之後的「停下來問人」不能再寫進去（`store::pause` 的 `false` 以前沒人看）。
    ///
    /// 三個地方都一樣：退回（輪數用完的那一支）、挑驗證者挑不到 Fable、交付失敗。`round` 還多一層：任務被關掉時
    /// `use_round` 也回 `Err`（它的 WHERE 帶著「還開著」），以前一律當成輪數用完，回 409 `max_rounds`——
    /// 呼叫端被叫去問使用者要不要再給一輪，而任務早就取消了。
    ///
    /// 重現同 `a_complete_that_loses_the_race_to_a_cancel_does_not_close_anything`：測試先拿寫入鎖並把任務標成
    /// 取消（未 commit），請求讀到「還開著」、卡在第一個寫入；放鎖後它面對的是已取消的任務。
    #[tokio::test]
    async fn a_mission_cancelled_mid_request_is_not_paused_afterwards() {
        for path in ["round", "pick", "deliver"] {
            let env = crate::testing::env().await;
            let app = env.app.clone();
            let Json(m) = post_mission(State(app.clone()), Path(env.project_id.clone()), Json(new_mission(&format!("late-{path}"), "push_main"))).await.unwrap();
            let id = m["id"].as_str().unwrap().to_string();
            if path == "deliver" {
                // 驗過 env.repo 的 HEAD；那個 repo 沒有 origin，交付一定失敗。
                let repo = Some(env.repo.to_string_lossy().to_string());
                let ok = EventIn { kind: "verified".into(), text: "ok".into(), relay_from: None, payload: None, sha: None, worktree: repo };
                let _ = post_event(State(app.clone()), Path(id.clone()), Json(ok)).await.unwrap();
            }

            let mut writer = app.db.acquire().await.unwrap();
            sqlx::query("BEGIN IMMEDIATE").execute(&mut *writer).await.unwrap();
            sqlx::query("UPDATE missions SET cancelled_at=?, updated_at=? WHERE id=?")
                .bind(crate::db::now())
                .bind(crate::db::now())
                .bind(&id)
                .execute(&mut *writer)
                .await
                .unwrap();
            let pending = match path {
                "round" => tokio::spawn(post_round(State(app.clone()), Path(id.clone()))),
                "pick" => {
                    let q = HashMap::from([("role".to_string(), "verifier".to_string())]);
                    tokio::spawn(get_pick(State(app.clone()), Path(id.clone()), Query(q)))
                }
                _ => {
                    let d = DeliverIn { worktree: env.repo.to_string_lossy().to_string(), title: None, body: None, relay_from: None };
                    tokio::spawn(post_deliver(State(app.clone()), Path(id.clone()), Json(d)))
                }
            };
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            sqlx::query("COMMIT").execute(&mut *writer).await.unwrap();
            drop(writer);
            let res = pending.await.unwrap();

            let paused: Vec<String> = sqlx::query_scalar("SELECT text FROM mission_events WHERE mission_id=? AND kind='paused'")
                .bind(&id)
                .fetch_all(&app.db)
                .await
                .unwrap();
            assert!(paused.is_empty(), "{path}：已取消的任務不該再記一則「暫停」：{paused:?}");
            assert_eq!(store::get(&app.db, &id).await.unwrap().unwrap().status(), "cancelled");
            if path != "pick" {
                match res {
                    Err(e) => assert_eq!(conflict_reason(e), "already_closed", "{path}：要照實說任務已經關了"),
                    Ok(Json(v)) => panic!("{path}：任務已取消卻回 200：{v}"),
                }
            }
        }
    }

    /// 上一條的另一半：派工在 supervisor 鎖裡查完「任務還開著」到寫下交辦之間還有好幾個 await，關任務的那一步
    /// 如果不拿同一把鎖，就能落在那中間（查的時候開著、寫的時候已經關了，而取消的重讀又還看不到那件）。
    /// 所以取消／結案落地的那一刻必須排在鎖後面：鎖被握著時，任務不能先關掉。
    #[tokio::test]
    async fn closing_a_mission_waits_for_the_supervisor_lock() {
        for close in ["cancel", "complete"] {
            let env = crate::testing::env().await;
            let app = env.app.clone();
            let Json(m) = post_mission(State(app.clone()), Path(env.project_id.clone()), Json(new_mission(&format!("wait-{close}"), "pr"))).await.unwrap();
            let id = m["id"].as_str().unwrap().to_string();

            let held = crate::supervisor::lock().await;
            let closing = if close == "cancel" {
                tokio::spawn(post_cancel(State(app.clone()), Path(id.clone()), HeaderMap::new()))
            } else {
                tokio::spawn(post_complete(State(app.clone()), Path(id.clone()), Json(done("完成", Some("no_changes"), None))))
            };
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            assert_eq!(store::get(&app.db, &id).await.unwrap().unwrap().status(), "open", "{close}：鎖還被握著，任務不能先關掉");
            drop(held);
            let _ = closing.await.unwrap().unwrap_or_else(|e| panic!("{close}：放鎖之後要照常關掉：{e:?}"));
            let expect = if close == "cancel" { "cancelled" } else { "done" };
            assert_eq!(store::get(&app.db, &id).await.unwrap().unwrap().status(), expect);
        }
    }

    /// 完成的任務可以被追問，而追問**不能**改變任何交付事實。這是「已完成清單不可覆寫」的底線。
    #[tokio::test]
    async fn a_finished_mission_can_be_asked_about_without_changing_anything() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let pid = env.project_id.clone();
        let Json(m) = post_mission(State(app.clone()), Path(pid.clone()), Json(new_mission("r1", "pr"))).await.unwrap();
        let id = m["id"].as_str().unwrap().to_string();
        let _ = post_complete(State(app.clone()), Path(id.clone()), Json(done("改好了", Some("no_changes"), None))).await.unwrap();
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
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let pid = env.project_id.clone();
        let Json(m) = post_mission(State(app.clone()), Path(pid.clone()), Json(new_mission("r1", "pr"))).await.unwrap();
        let id = m["id"].as_str().unwrap().to_string();
        let _ = post_pause(State(app.clone()), Path(id.clone()), HeaderMap::new(), Json(PauseIn { reason: "max_rounds".into(), detail: None }))
            .await
            .unwrap();

        let Json(out) = post_answer(State(app.clone()), Path(id.clone()), Json(ans("照你說的做", "a1"))).await.unwrap();
        assert_eq!(out["resumed"], true);
        assert_eq!(out["mission"]["status"], "open", "放行了");
        assert_eq!(inbox_keys(&app, &format!("mission:{id}:answer:%")).await, [format!("mission:{id}:answer:a1")]);
        let kinds: Vec<String> = sqlx::query_scalar("SELECT kind FROM mission_events WHERE mission_id = ? ORDER BY created_at, rowid")
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
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let pid = env.project_id.clone();
        let Json(m) = post_mission(State(app.clone()), Path(pid.clone()), Json(new_mission("r1", "pr"))).await.unwrap();
        let id = m["id"].as_str().unwrap().to_string();
        let _ = post_pause(State(app.clone()), Path(id.clone()), HeaderMap::new(), Json(PauseIn { reason: "waiting_quota".into(), detail: None }))
            .await
            .unwrap();

        let _ = post_resume(State(app.clone()), Path(id.clone())).await.unwrap();
        assert_eq!(inbox_keys(&app, &format!("mission:{id}:resumed:%")).await.len(), 1, "按鈕有接線");

        // 已經在跑的任務再按一次：沒有 paused→open 的轉移，就不該再有通知。
        let _ = post_resume(State(app.clone()), Path(id.clone())).await.unwrap();
        let _ = post_resume(State(app.clone()), Path(id.clone())).await.unwrap();
        assert_eq!(inbox_keys(&app, &format!("mission:{id}:resumed:%")).await.len(), 1, "不會自己叫醒自己");
    }

    /// 追加修改開的是**新的一筆**任務：舊成果原封不動，而新任務拿不到舊的 verified。
    #[tokio::test]
    async fn a_revision_is_a_new_mission_that_cannot_inherit_the_old_verification() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let pid = env.project_id.clone();
        let Json(m) = post_mission(State(app.clone()), Path(pid.clone()), Json(new_mission("r1", "pr"))).await.unwrap();
        let id = m["id"].as_str().unwrap().to_string();
        let ev = EventIn {
            kind: "verified".into(),
            text: "cargo test 全過".into(),
            relay_from: Some("daemon".into()),
            payload: Some(json!({"shots": ["/tmp/a.png"]})),
            sha: None,
            worktree: Some(env.repo.to_string_lossy().to_string()),
        };
        let _ = post_event(State(app.clone()), Path(id.clone()), Json(ev)).await.unwrap();
        // 這筆 fixture 刻意**沒有交付**（下面要驗快照誠實地說不出交付方式）。沒交付要結案得有使用者的決定（issue #74）。
        let _ = post_pause(State(app.clone()), Path(id.clone()), HeaderMap::new(), Json(PauseIn { reason: "clarify".into(), detail: None })).await.unwrap();
        let _ = post_answer(State(app.clone()), Path(id.clone()), Json(ans("先不要交付，直接結案", "no-deliver"))).await.unwrap();
        let _ = post_complete(State(app.clone()), Path(id.clone()), Json(done("第一版", Some("user_declined"), None))).await.unwrap();

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

    #[tokio::test]
    async fn revision_source_is_preserved_and_part_of_replay_identity() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let Json(m) = post_mission(State(app.clone()), Path(env.project_id.clone()), Json(new_mission("source-parent", "pr"))).await.unwrap();
        let id = m["id"].as_str().unwrap().to_string();
        let _ = post_complete(State(app.clone()), Path(id.clone()), Json(done("v1", Some("no_changes"), None))).await.unwrap();
        let mut input = rev("v2", "source-revise");
        input.relay_from = Some("daemon".into());
        let Json(child) = post_revise(State(app.clone()), Path(id.clone()), Json(input)).await.unwrap();
        let cid = child["id"].as_str().unwrap();
        let events = store::events(&app.db, cid).await.unwrap();
        assert_eq!(events[0].relay_from.as_deref(), Some("daemon"));
        assert_eq!(serde_json::from_str::<Value>(&events[0].payload_json).unwrap()["requested_by"], "daemon");
        let err = post_revise(State(app.clone()), Path(id.clone()), Json(rev("v2", "source-revise"))).await.unwrap_err();
        assert_eq!(conflict_reason(err), "request_id_reused");
        let mut invalid = rev("v2", "invalid-source");
        invalid.relay_from = Some("missing-bot".into());
        assert!(matches!(post_revise(State(app.clone()), Path(id), Json(invalid)).await, Err(LcError::Bad(_))));
    }

    /// 續作只能從**已完成**的成果開。進行中與已取消各自回明確理由。
    #[tokio::test]
    async fn only_a_finished_mission_can_be_revised() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let pid = env.project_id.clone();
        let Json(m) = post_mission(State(app.clone()), Path(pid.clone()), Json(new_mission("r1", "pr"))).await.unwrap();
        let id = m["id"].as_str().unwrap().to_string();
        let err = post_revise(State(app.clone()), Path(id.clone()), Json(rev("改這個", "rev1"))).await.unwrap_err();
        assert_eq!(conflict_reason(err), "not_completed");

        let _ = post_cancel(State(app.clone()), Path(id.clone()), HeaderMap::new()).await.unwrap();
        let err = post_revise(State(app.clone()), Path(id.clone()), Json(rev("改這個", "rev2"))).await.unwrap_err();
        assert_eq!(conflict_reason(err), "not_completed");
    }

    #[tokio::test]
    async fn a_mission_runs_through_its_gates_end_to_end() {
        let env = crate::testing::env().await;
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
        let repo = Some(env.repo.to_string_lossy().to_string());
        let bogus = EventIn { kind: "verified".into(), text: "ok".into(), relay_from: Some("no-such-bot".into()), payload: None, sha: None, worktree: repo.clone() };
        assert!(matches!(post_event(State(app.clone()), Path(id.clone()), Json(bogus)).await, Err(LcError::Bad(_))));
        let ok = EventIn { kind: "verified".into(), text: "cargo test 全過".into(), relay_from: Some("daemon".into()), payload: None, sha: None, worktree: repo };
        let _ = post_event(State(app.clone()), Path(id.clone()), Json(ok)).await.unwrap();

        // 測試 repo 沒有 origin：交付失敗 → 停下來問人（D8），不是靜靜吞掉。
        let err = post_deliver(State(app.clone()), Path(id.clone()), Json(deliver())).await.unwrap_err();
        assert!(matches!(err, LcError::Conflict(_)));
        let Json(cur) = get_mission(State(app.clone()), Path(id.clone())).await.unwrap();
        assert_eq!(cur["paused_reason"], "push_main_failed");

        // 驗過卻沒交付：不能就這樣結案（issue #74），也不能說成「沒有改東西」；「使用者不要」要有使用者的回答。
        let complete = |no_delivery: Option<&'static str>| post_complete(State(app.clone()), Path(id.clone()), Json(done("修好了", no_delivery, None)));
        assert_eq!(conflict_reason(complete(None).await.unwrap_err()), "not_delivered");
        assert_eq!(conflict_reason(complete(Some("no_changes")).await.unwrap_err()), "has_verified_changes");
        assert_eq!(conflict_reason(complete(Some("user_declined")).await.unwrap_err()), "user_not_asked");
        assert!(matches!(complete(Some("later")).await.unwrap_err(), LcError::Bad(_)), "不認得的理由是 400");
        // 使用者在群組回答「不用推了」之後，照實記下沒交付的理由再結案。
        let _ = post_answer(State(app.clone()), Path(id.clone()), Json(ans("不用推了，直接結案", "decline"))).await.unwrap();
        let Json(closed) = complete(Some("user_declined")).await.unwrap();
        assert_eq!((closed["delivery"]["status"].as_str(), closed["delivery"]["reason"].as_str()), (Some("waived"), Some("user_declined")));

        // 完成之後就關起來；已完成任務清單查得到。
        let err = post_pause(State(app.clone()), Path(id.clone()), HeaderMap::new(), Json(PauseIn { reason: "late".into(), detail: None })).await.unwrap_err();
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

    fn git(dir: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn commit_file(dir: &std::path::Path, name: &str) -> String {
        std::fs::write(dir.join(name), name).unwrap();
        git(dir, &["add", name]);
        git(dir, &["commit", "-q", "-m", name]);
        git(dir, &["rev-parse", "HEAD"])
    }

    /// 測試專案接上一個 bare origin（main 就是目前的 base），再開一個執行者的 worktree。
    fn with_origin(env: &crate::testing::Env) -> (std::path::PathBuf, std::path::PathBuf) {
        let origin = env.dir.join("origin.git");
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "-q", "--bare", "-b", "main"]);
        git(&env.repo, &["remote", "add", "origin", origin.to_str().unwrap()]);
        git(&env.repo, &["push", "-q", "origin", "main"]);
        git(&env.repo, &["fetch", "-q", "origin"]);
        let wt = env.dir.join("exec-wt");
        git(&env.repo, &["worktree", "add", "-q", "-b", "task", wt.to_str().unwrap()]);
        (origin, wt)
    }

    fn verified(worktree: Option<&std::path::Path>, sha: Option<&str>) -> EventIn {
        EventIn {
            kind: "verified".into(),
            text: "cargo test 全過".into(),
            relay_from: Some("daemon".into()),
            payload: None,
            sha: sha.map(String::from),
            worktree: worktree.map(|w| w.to_string_lossy().to_string()),
        }
    }

    fn deliver_from(dir: &std::path::Path) -> DeliverIn {
        DeliverIn { worktree: dir.to_string_lossy().to_string(), title: None, body: None, relay_from: None }
    }

    fn bad_text(e: LcError) -> String {
        match e {
            LcError::Bad(t) => t,
            other => panic!("expected a 400, got {other:?}"),
        }
    }

    /// 交付關卡綁 commit：驗過 A、rebase 成 B，B 沒重驗就推不上去；從專案主樹或別的 repo 交付也不行（review3 c1 M9）。
    #[tokio::test]
    async fn only_the_verified_commit_can_be_delivered() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (origin, wt) = with_origin(&env);
        let Json(m) = post_mission(State(app.clone()), Path(env.project_id.clone()), Json(new_mission("gate", "push_main"))).await.unwrap();
        let id = m["id"].as_str().unwrap().to_string();

        // verified 一定要說驗的是哪個 commit。
        let err = post_event(State(app.clone()), Path(id.clone()), Json(verified(None, None))).await.unwrap_err();
        assert!(bad_text(err).contains("commit it verified"));
        let elsewhere = env.dir.join("other-repo");
        crate::testing::git::init_repo(&elsewhere);
        let err = post_event(State(app.clone()), Path(id.clone()), Json(verified(Some(&elsewhere), None))).await.unwrap_err();
        assert!(bad_text(err).contains("not a checkout of this mission's project"), "別的 repo 的工作樹不算");

        let a = commit_file(&wt, "a.txt");
        let Json(ev) = post_event(State(app.clone()), Path(id.clone()), Json(verified(Some(&wt), None))).await.unwrap();
        let payload: Value = serde_json::from_str(ev["payload_json"].as_str().unwrap()).unwrap();
        assert_eq!(payload["sha"], json!(a), "daemon 自己讀 HEAD 記下完整 sha");
        // 縮寫 sha 也行，跟工作樹對不上就是 400。
        let err = post_event(State(app.clone()), Path(id.clone()), Json(verified(Some(&wt), Some("0123456789ab")))).await.unwrap_err();
        assert!(matches!(err, LcError::Bad(_)));

        // 執行者驗完又改（rebase／補一刀）：B 沒驗過，交付擋下來，而且**不**把任務停成交付失敗。
        let b = commit_file(&wt, "b.txt");
        let err = post_deliver(State(app.clone()), Path(id.clone()), Json(deliver_from(&wt))).await.unwrap_err();
        let LcError::Conflict(v) = err else { panic!("expected 409") };
        assert_eq!((v["reason"].as_str(), v["verified_sha"].as_str(), v["head"].as_str()), (Some("head_not_verified"), Some(a.as_str()), Some(b.as_str())));
        // 專案主樹的 HEAD 也不是驗過的那個（主樹上可能有別人還沒 review 的 commit）。
        let err = post_deliver(State(app.clone()), Path(id.clone()), Json(deliver_from(&env.repo))).await.unwrap_err();
        assert_eq!(conflict_reason(err), "head_not_verified");
        let Json(cur) = get_mission(State(app.clone()), Path(id.clone())).await.unwrap();
        assert_eq!(cur["status"], "open", "流程漏了一步不是交付失敗");
        assert_eq!(git(&origin, &["rev-parse", "main"]), git(&env.repo, &["rev-parse", "main"]), "什麼都沒推");

        // 重驗 B（用縮寫 sha 記）之後才推得上去，推上去的就是 B。
        let _ = post_event(State(app.clone()), Path(id.clone()), Json(verified(None, Some(&b[..12])))).await.unwrap();
        let Json(out) = post_deliver(State(app.clone()), Path(id.clone()), Json(deliver_from(&wt))).await.unwrap();
        assert_eq!(out["sha"], json!(b));
        assert_eq!(git(&origin, &["rev-parse", "main"]), b);
    }

    /// 交付失敗停下來之後，rebase 並重驗、再交付成功：「推 main 失敗」的暫停自動解除，卡片不再寫著等你決定（review3 c1 M9）。
    #[tokio::test]
    async fn a_successful_retry_clears_the_delivery_failure_pause() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (origin, wt) = with_origin(&env);
        let Json(m) = post_mission(State(app.clone()), Path(env.project_id.clone()), Json(new_mission("retry", "push_main"))).await.unwrap();
        let id = m["id"].as_str().unwrap().to_string();
        commit_file(&wt, "mine.txt");
        let _ = post_event(State(app.clone()), Path(id.clone()), Json(verified(Some(&wt), None))).await.unwrap();

        // 別人先推了一個 commit：不是 fast-forward，任務停下來。
        let other = env.dir.join("other-clone");
        std::process::Command::new("git").args(["clone", "-q", origin.to_str().unwrap(), other.to_str().unwrap()]).status().unwrap();
        commit_file(&other, "theirs.txt");
        git(&other, &["push", "-q", "origin", "HEAD:main"]);
        let err = post_deliver(State(app.clone()), Path(id.clone()), Json(deliver_from(&wt))).await.unwrap_err();
        assert_eq!(conflict_reason(err), "not_fast_forward");
        assert_eq!(load(&app, &id).await.unwrap().paused_reason.as_deref(), Some("push_main_failed"));

        // rebase 之後 HEAD 變了：舊的 verified 不算，要重驗。
        git(&wt, &["fetch", "-q", "origin"]);
        git(&wt, &["rebase", "-q", "origin/main"]);
        assert_eq!(conflict_reason(post_deliver(State(app.clone()), Path(id.clone()), Json(deliver_from(&wt))).await.unwrap_err()), "head_not_verified");
        let _ = post_event(State(app.clone()), Path(id.clone()), Json(verified(Some(&wt), None))).await.unwrap();
        let Json(out) = post_deliver(State(app.clone()), Path(id.clone()), Json(deliver_from(&wt))).await.unwrap();
        assert_eq!(out["sha"], json!(git(&wt, &["rev-parse", "HEAD"])));

        let m = load(&app, &id).await.unwrap();
        assert_eq!(m.status(), "open", "交付成功就不再停在 push_main_failed");
        let events = store::events(&app.db, &id).await.unwrap();
        assert!(events.iter().any(|e| e.kind == "resumed" && e.text.contains("push_main_failed")));
    }

    /// 這個版本以前記的 `verified` 沒有 sha：不能拿來放行交付，要講清楚怎麼補。
    #[tokio::test]
    async fn a_legacy_verified_without_a_commit_does_not_open_the_gate() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let Json(m) = post_mission(State(app.clone()), Path(env.project_id.clone()), Json(new_mission("legacy", "pr"))).await.unwrap();
        let id = m["id"].as_str().unwrap().to_string();
        store::add_event(&app.db, &id, "verified", "舊版記的", Some("daemon"), &json!({})).await.unwrap();
        let err = post_deliver(State(app.clone()), Path(id.clone()), Json(deliver_from(&env.repo))).await.unwrap_err();
        assert_eq!(conflict_reason(err), "verified_without_sha");
    }

    /// 交辦掛到任務上（測試夾具：直接寫 store，狀態由呼叫端決定——這裡要的是「有這件交辦」這個事實）。
    async fn mission_assignment(app: &Arc<App>, mission_id: &str, crid: &str, role: &str, status: &str) -> String {
        crate::supervisor::store::get_or_init(&app.db).await.unwrap();
        let a = crate::supervisor::store::insert_assignment(&app.db, None, "bot1", crid, "做 X", &[], None, true).await.unwrap();
        crate::supervisor::store::set_mission_link(&app.db, &a.id, mission_id, role).await.unwrap();
        sqlx::query("UPDATE supervisor_assignments SET status=? WHERE id=?").bind(status).bind(&a.id).execute(&app.db).await.unwrap();
        a.id
    }

    fn accept() -> crate::supervisor::api::ReviewIn {
        crate::supervisor::api::ReviewIn {
            decision: "accept".into(),
            actor: None,
            source: None,
            reason: None,
            evidence: None,
            followup_text: None,
            followup_request_id: None,
            followup_bot_id: None,
            ownership: Vec::new(),
        }
    }

    /// issue #74 驗收一＋二：AGM **只照 `mission get` 的 `next` 做**，就能從派工一路走到結案，不必記 runbook
    /// 的順序。每一步都重新從資料庫讀 `next`——等於每一步之間 daemon 都重啟過一次，推得出來的就是持久狀態。
    /// 每個入口都是真的（assign、review、verified、deliver、complete），回合結束那一下用夾具模擬（沒有真的 bot）。
    #[tokio::test]
    async fn following_next_alone_walks_the_happy_path_to_completion() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        crate::supervisor::store::get_or_init(&app.db).await.unwrap();
        let agm = crate::testing::claude_bot(&app, &env.project_id, "AGM").await;
        crate::supervisor::store::set_env(&app.db, &agm.id, &env.project_id, "/tmp").await.unwrap();
        let (origin, wt) = with_origin(&env);
        let Json(m) = post_mission(State(app.clone()), Path(env.project_id.clone()), Json(new_mission("happy", "push_main"))).await.unwrap();
        let id = m["id"].as_str().unwrap().to_string();
        let next = || {
            let (app, id) = (app.clone(), id.clone());
            async move { get_mission(State(app), Path(id)).await.unwrap().0["next"].clone() }
        };

        let mut steps = Vec::new();
        for turn in 0..12 {
            let n = next().await;
            let action = n["action"].as_str().unwrap().to_string();
            steps.push(format!("{action}:{}", n["role"].as_str().unwrap_or("-")));
            match action.as_str() {
                "assign" => {
                    let role = n["role"].as_str().unwrap();
                    let bot = crate::testing::claude_bot(&app, &env.project_id, &format!("agm-mission-happy-{role}")).await;
                    if role == "executor" {
                        commit_file(&wt, "fix.txt"); // 執行者的成果
                    }
                    let out = crate::supervisor::assign(
                        &app, &bot.id, "做 X", &format!("crid-happy-{turn}"), None, &[], None, true, Some((&id, role)), None, None,
                        crate::supervisor::bot_requests::ReplyMark::default(),
                    )
                    .await
                    .unwrap();
                    // 回合結束（夾具）：交辦停在等裁示。
                    sqlx::query("UPDATE supervisor_assignments SET status='awaiting_review' WHERE id=?")
                        .bind(out["id"].as_str().unwrap())
                        .execute(&app.db)
                        .await
                        .unwrap();
                }
                "review" => {
                    let aid = n["assignment_id"].as_str().unwrap().to_string();
                    let Json(r) = crate::supervisor::api::post_review(State(app.clone()), Path(aid), HeaderMap::new(), Json(accept())).await.unwrap();
                    assert_eq!(r["mission_next"]["next"], next().await, "裁示的回應直接帶下一步，就是 mission get 推出來的那一步");
                }
                "record_verification" => {
                    let Json(ev) = post_event(State(app.clone()), Path(id.clone()), Json(verified(Some(&wt), None))).await.unwrap();
                    let p: Value = serde_json::from_str(ev["payload_json"].as_str().unwrap()).unwrap();
                    assert_eq!(p[flow::ANCHOR], n["assignment_id"], "verified 記下當時最後一件交辦（驗證者那件）");
                    assert_eq!(p["worktree"].as_str(), wt.to_str());
                }
                "deliver" => {
                    assert_eq!(n["sha"], json!(git(&wt, &["rev-parse", "HEAD"])), "下一步說得出要交哪個 commit");
                    let _ = post_deliver(State(app.clone()), Path(id.clone()), Json(deliver_from(&wt))).await.unwrap();
                }
                "complete" => {
                    let Json(closed) = post_complete(State(app.clone()), Path(id.clone()), Json(done("完成", None, None))).await.unwrap();
                    assert_eq!((closed["delivery"]["status"].as_str(), closed["delivery"]["sha"].as_str()), (Some("delivered"), Some(git(&wt, &["rev-parse", "HEAD"]).as_str())));
                    break;
                }
                other => panic!("happy path 不該走到 {other}：{n}"),
            }
        }
        assert_eq!(
            steps,
            [
                "assign:executor", "review:executor", "assign:reviewer", "review:reviewer", "assign:verifier", "review:verifier",
                "record_verification:-", "deliver:-", "complete:-",
            ]
        );
        assert_eq!(git(&origin, &["rev-parse", "main"]), git(&wt, &["rev-parse", "HEAD"]), "推上去的就是驗過的那一個");
        assert_eq!(next().await["action"], "closed");
    }

    /// 驗收八：退回（新的一代）或驗完又派了執行者，舊的 `verified` 就放行不了交付——**就算 HEAD 沒變**。
    /// 被退回的那一份不必改一個字就能靠舊驗證推上去，是 commit 比對擋不到的那一半。
    #[tokio::test]
    async fn a_round_or_a_new_executor_after_verification_blocks_delivery_of_the_same_commit() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (origin, wt) = with_origin(&env);
        let Json(m) = post_mission(State(app.clone()), Path(env.project_id.clone()), Json(new_mission("stale", "push_main"))).await.unwrap();
        let id = m["id"].as_str().unwrap().to_string();
        let a = commit_file(&wt, "a.txt");
        let deliver = || post_deliver(State(app.clone()), Path(id.clone()), Json(deliver_from(&wt)));
        let stale_because = |e: LcError| match e {
            LcError::Conflict(v) => (v["reason"].as_str().unwrap_or_default().to_string(), v["stale_because"].as_str().unwrap_or_default().to_string()),
            other => panic!("expected 409, got {other:?}"),
        };

        // 驗過 A 之後退回：HEAD 還是 A，但那是上一代的驗證。
        let _ = post_event(State(app.clone()), Path(id.clone()), Json(verified(Some(&wt), None))).await.unwrap();
        let _ = post_round(State(app.clone()), Path(id.clone())).await.unwrap();
        assert_eq!(stale_because(deliver().await.unwrap_err()), ("verification_stale".into(), "round".into()));

        // 這一代重驗之後，又派了執行者（rebase／補改）：它還沒動 HEAD 也一樣，要重驗。
        let _ = post_event(State(app.clone()), Path(id.clone()), Json(verified(Some(&wt), None))).await.unwrap();
        let exec = mission_assignment(&app, &id, "stale-exec", "executor", "delivered").await;
        assert_eq!(stale_because(deliver().await.unwrap_err()), ("verification_stale".into(), "new_executor".into()));
        assert_eq!(load(&app, &id).await.unwrap().status(), "open", "流程漏了一步不是交付失敗，不停下來");

        // 執行者結案、重驗這個 commit：放行，推上去的是 A。
        sqlx::query("UPDATE supervisor_assignments SET status='completed' WHERE id=?").bind(&exec).execute(&app.db).await.unwrap();
        let _ = post_event(State(app.clone()), Path(id.clone()), Json(verified(Some(&wt), None))).await.unwrap();
        let Json(out) = deliver().await.unwrap();
        assert_eq!(out["sha"], json!(a));
        assert_eq!(git(&origin, &["rev-parse", "main"]), a);
    }

    /// 驗收七＋九：沒交付就結案要被擋；「沒有改東西」派過執行者的話要拿工作樹證明，說謊的擋下來。
    #[tokio::test]
    async fn completing_without_delivery_is_refused_unless_the_stated_reason_holds() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (_origin, wt) = with_origin(&env);
        let Json(m) = post_mission(State(app.clone()), Path(env.project_id.clone()), Json(new_mission("undelivered", "push_main"))).await.unwrap();
        let id = m["id"].as_str().unwrap().to_string();
        let _ = mission_assignment(&app, &id, "u-exec", "executor", "completed").await;
        let complete = |no_delivery: Option<&'static str>, worktree: Option<&std::path::Path>| {
            post_complete(State(app.clone()), Path(id.clone()), Json(done("完成", no_delivery, worktree)))
        };

        // 什麼都沒講：擋。
        assert_eq!(conflict_reason(complete(None, None).await.unwrap_err()), "not_delivered");
        // 說「沒有改東西」但派過執行者：要附工作樹。
        assert!(bad_text(complete(Some("no_changes"), None).await.unwrap_err()).contains("needs `worktree`"));
        // 工作樹有沒提交的改動、或有 base 以外的 commit：那就是有改東西。
        std::fs::write(wt.join("dirty.txt"), "x").unwrap();
        assert_eq!(conflict_reason(complete(Some("no_changes"), Some(&wt)).await.unwrap_err()), "worktree_has_changes");
        std::fs::remove_file(wt.join("dirty.txt")).unwrap();
        let b = commit_file(&wt, "b.txt");
        assert_eq!(conflict_reason(complete(Some("no_changes"), Some(&wt)).await.unwrap_err()), "worktree_has_changes");
        assert_eq!(load(&app, &id).await.unwrap().status(), "open", "被擋下來的結案什麼都沒寫");

        // 驗過、交付了：結案，記下交了哪個 commit。
        let _ = post_event(State(app.clone()), Path(id.clone()), Json(verified(Some(&wt), None))).await.unwrap();
        let _ = post_deliver(State(app.clone()), Path(id.clone()), Json(deliver_from(&wt))).await.unwrap();
        let Json(closed) = complete(None, None).await.unwrap();
        assert_eq!((closed["delivery"]["status"].as_str(), closed["delivery"]["sha"].as_str()), (Some("delivered"), Some(b.as_str())));
        let events = store::events(&app.db, &id).await.unwrap();
        let completed = events.iter().find(|e| e.kind == "completed").unwrap();
        let payload: Value = serde_json::from_str(&completed.payload_json).unwrap();
        assert_eq!(payload["delivery"]["sha"], json!(b), "時間軸上看得出交的是哪個 commit");
    }

    /// 放行（answer／resume）叫醒 AGM 的通知帶著放行之後的那一步：AGM 不用自己記「停下之前做到哪」。
    #[tokio::test]
    async fn waking_the_manager_after_a_pause_says_where_to_continue() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let Json(m) = post_mission(State(app.clone()), Path(env.project_id.clone()), Json(new_mission("resume-next", "pr"))).await.unwrap();
        let id = m["id"].as_str().unwrap().to_string();
        let _ = mission_assignment(&app, &id, "rn-exec", "executor", "completed").await;
        let payload_of = |kind: &'static str| {
            let app = app.clone();
            async move {
                let raw: String = sqlx::query_scalar("SELECT payload_json FROM supervisor_inbox WHERE kind=? ORDER BY rowid DESC LIMIT 1")
                    .bind(kind)
                    .fetch_one(&app.db)
                    .await
                    .unwrap();
                serde_json::from_str::<Value>(&raw).unwrap()
            }
        };
        let _ = post_pause(State(app.clone()), Path(id.clone()), HeaderMap::new(), Json(PauseIn { reason: "clarify".into(), detail: None })).await.unwrap();
        let Json(cur) = get_mission(State(app.clone()), Path(id.clone())).await.unwrap();
        assert_eq!((cur["next"]["action"].as_str(), cur["next"]["then"]["role"].as_str()), (Some("paused"), Some("reviewer")));
        let _ = post_answer(State(app.clone()), Path(id.clone()), Json(ans("照原本的做", "rn-a"))).await.unwrap();
        let p = payload_of("mission_answered").await;
        assert_eq!((p["next"]["action"].as_str(), p["next"]["role"].as_str()), (Some("assign"), Some("reviewer")));

        let _ = post_pause(State(app.clone()), Path(id.clone()), HeaderMap::new(), Json(PauseIn { reason: "user_pause".into(), detail: None })).await.unwrap();
        let _ = post_resume(State(app.clone()), Path(id.clone())).await.unwrap();
        let p = payload_of("mission_resumed").await;
        assert_eq!((p["next"]["action"].as_str(), p["next"]["role"].as_str()), (Some("assign"), Some("reviewer")));
    }

    /// runbook 第 2 步：`pick` 回 `wait` 時照樣開 bot 並 `assign --mission`——派送時 daemon 查到撞限，
    /// 交辦停在 `quota_blocked`，額度回來 controller 自己重送。什麼都不做的話沒有任何東西會叫醒 AGM，
    /// 任務永遠停在「等 AGM 接手…」（review3 c1 M12）。
    #[tokio::test]
    async fn assigning_on_a_wait_pick_parks_the_work_on_quota_instead_of_stalling_the_mission() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        crate::supervisor::store::get_or_init(&app.db).await.unwrap();
        let agm = crate::testing::claude_bot(&app, &env.project_id, "AGM").await;
        crate::supervisor::store::set_env(&app.db, &agm.id, &env.project_id, "/tmp").await.unwrap();
        let exec = crate::testing::claude_bot(&app, &env.project_id, "agm-mission-exec").await;
        sqlx::query("UPDATE bots SET identity = 'cc2' WHERE id = ?").bind(&exec.id).execute(&app.db).await.unwrap();

        let mut input = new_mission("wait-pick", "pr");
        input.on_5h_limit = "wait".into();
        let Json(m) = post_mission(State(app.clone()), Path(env.project_id.clone()), Json(input)).await.unwrap();
        let id = m["id"].as_str().unwrap().to_string();
        // cc2 的 5h 窗撞限（橫幅講明是 5h），任務選了「等重置」。
        {
            let mut q = quota(10.0);
            q.five_hour = Some(Window { used_pct: 100.0, resets_at: Some("2999-01-01T05:00:00Z".into()) });
            q.limit_hit = Some(crate::quota::LimitHit {
                message: "You've hit your 5-hour limit".into(),
                until: Some("2999-01-01T05:00:00Z".into()),
                at: crate::db::now(),
                bucket: Some("five_hour".into()),
            });
            app.quotas.lock().await.insert("claude:cc2".into(), q);
        }
        let Json(p) = get_pick(State(app.clone()), Path(id.clone()), Query(HashMap::from([("role".to_string(), "executor".to_string())]))).await.unwrap();
        assert_eq!((p["pick"]["decision"].as_str(), p["pick"]["identity"].as_str()), (Some("wait"), Some("cc2")));

        // runbook 第 2 步：照樣派。
        let out = crate::supervisor::assign(
            &app, &exec.id, "做 X", "crid-wait", None, &[], None, true, Some((&id, "executor")), None, None,
            crate::supervisor::bot_requests::ReplyMark::default(),
        )
            .await
            .unwrap();
        let a = crate::supervisor::store::assignment(&app.db, out["id"].as_str().unwrap()).await.unwrap().unwrap();
        assert_eq!(a.status, "quota_blocked", "派送時查到撞限就停在這裡，額度回來 controller 自己重送");
        assert!(a.resume_at.is_some(), "有預計重送的時間");
        let Json(card) = get_mission(State(app.clone()), Path(id.clone())).await.unwrap();
        assert_eq!(card["phase"], "waiting_quota", "卡片說得出在等額度，不是「等 AGM 接手」");
    }

    /// issue #74：兩道閘門**真的接在入口上**，不只是模組裡有那支函式。
    ///
    /// 模組自己的測試（`mission::workflow`）證明規則對；這一條證明 `assign` 與 `mission complete`
    /// 真的會去問它——少接一邊的話，規則寫得再對也沒用。
    #[tokio::test]
    async fn the_deterministic_gates_are_wired_into_both_entry_points() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        crate::supervisor::store::get_or_init(&app.db).await.unwrap();
        let agm = crate::testing::claude_bot(&app, &env.project_id, "AGM").await;
        crate::supervisor::store::set_env(&app.db, &agm.id, &env.project_id, "/tmp").await.unwrap();
        let exec = crate::testing::claude_bot(&app, &env.project_id, "exec").await;
        let other = crate::testing::claude_bot(&app, &env.project_id, "other").await;
        let Json(m) = post_mission(State(app.clone()), Path(env.project_id.clone()), Json(new_mission("gates", "pr"))).await.unwrap();
        let id = m["id"].as_str().unwrap().to_string();

        let assign = |bot: String, crid: &'static str, role: &'static str| {
            let app = app.clone();
            let id = id.clone();
            async move {
                crate::supervisor::assign(
                    &app, &bot, "做 X", crid, None, &[], None, true, Some((&id, role)), None, None,
                    crate::supervisor::bot_requests::ReplyMark::default(),
                )
                .await
            }
        };
        let first = assign(exec.id.clone(), "crid-exec", "executor").await.expect("第一件派得出去");
        let first_id = first["id"].as_str().unwrap().to_string();

        // 入口一：任務已經有一件開著，第二件要被擋。
        let err = assign(other.id.clone(), "crid-review", "reviewer").await.unwrap_err();
        assert_eq!(conflict_reason(err), "mission_busy", "assign 沒接上閘門");

        // 入口二：同一個狀態，結案也要被擋。
        // 結案時對交付的要求另外測；這裡用「沒有改東西」並附上乾淨的執行者工作樹，只量「開著的交辦」這道。
        let (_origin, wt) = with_origin(&env);
        let complete = || post_complete(State(app.clone()), Path(id.clone()), Json(done("完成", Some("no_changes"), Some(&wt))));
        assert_eq!(conflict_reason(complete().await.unwrap_err()), "assignments_open", "complete 沒接上閘門");

        // 收乾淨之後兩邊都放行——閘門不是把路堵死。
        sqlx::query("UPDATE supervisor_assignments SET status='completed' WHERE id=?")
            .bind(&first_id)
            .execute(&app.db)
            .await
            .unwrap();
        assign(other.id.clone(), "crid-review", "reviewer").await.expect("前一件結案後派得出去");
        sqlx::query("UPDATE supervisor_assignments SET status='completed' WHERE mission_id=?")
            .bind(&id)
            .execute(&app.db)
            .await
            .unwrap();
        let Json(done) = complete().await.expect("全部結案後才結得了案");
        assert_eq!(status(&done), "done");
    }

    /// 來回次數用完、使用者說「再改一輪」：放行時多給一輪，AGM 才有路可走（review3 c1 M13）。
    #[tokio::test]
    async fn releasing_a_mission_that_used_up_its_rounds_grants_one_more() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let Json(m) = post_mission(State(app.clone()), Path(env.project_id.clone()), Json(new_mission("rounds", "pr"))).await.unwrap();
        let id = m["id"].as_str().unwrap().to_string();
        let round = || post_round(State(app.clone()), Path(id.clone()));
        let _ = round().await.unwrap();
        let _ = round().await.unwrap();
        assert_eq!(conflict_reason(round().await.unwrap_err()), "max_rounds");
        let m = load(&app, &id).await.unwrap();
        assert_eq!((m.paused_reason.as_deref(), m.rounds_used, m.max_rounds), (Some("max_rounds"), 2, 2));

        // 使用者回答「再給一輪」：放行＋上限加一，AGM 的下一次 round 走得通。
        let Json(out) = post_answer(State(app.clone()), Path(id.clone()), Json(ans("再改一輪：把標題也換掉", "a1"))).await.unwrap();
        assert_eq!(out["resumed"], true);
        assert_eq!(load(&app, &id).await.unwrap().max_rounds, 3);
        let Json(third) = round().await.unwrap();
        assert_eq!((third["rounds_used"].as_i64(), third["status"].as_str()), (Some(3), Some("open")));
        assert_eq!(conflict_reason(round().await.unwrap_err()), "max_rounds", "加的是一輪，不是無上限");

        // 「不回答直接繼續」也算放行，一樣多給一輪；事件說得出來。
        let _ = post_resume(State(app.clone()), Path(id.clone())).await.unwrap();
        assert_eq!(load(&app, &id).await.unwrap().max_rounds, 4);
        let events = store::events(&app.db, &id).await.unwrap();
        assert!(events.iter().any(|e| e.kind == "resumed" && e.text.contains("加一輪")));

        // 別的原因停下來的放行不會偷偷加額度。
        let _ = post_pause(State(app.clone()), Path(id.clone()), HeaderMap::new(), Json(PauseIn { reason: "clarify".into(), detail: None })).await.unwrap();
        let _ = post_resume(State(app.clone()), Path(id.clone())).await.unwrap();
        assert_eq!(load(&app, &id).await.unwrap().max_rounds, 4);
    }

    /// 同一個 commit 交付兩次要冪等：回原本那一筆；連 `delivered` 事件都沒寫成的那種（CLI 逾時、502）
    /// 也要看得出「其實已經在 main 上」，不能報成交付失敗（review3 c1 M11）。
    #[tokio::test]
    async fn delivering_the_same_commit_twice_does_not_report_a_failure() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (origin, wt) = with_origin(&env);
        let Json(m) = post_mission(State(app.clone()), Path(env.project_id.clone()), Json(new_mission("twice", "push_main"))).await.unwrap();
        let id = m["id"].as_str().unwrap().to_string();
        let sha = commit_file(&wt, "a.txt");
        let _ = post_event(State(app.clone()), Path(id.clone()), Json(verified(Some(&wt), None))).await.unwrap();

        let Json(first) = post_deliver(State(app.clone()), Path(id.clone()), Json(deliver_from(&wt))).await.unwrap();
        assert_eq!((first["sha"].as_str(), first["already_in_base"].as_bool()), (Some(sha.as_str()), Some(false)));
        assert_eq!(git(&origin, &["rev-parse", "main"]), sha);

        // 再呼叫一次（AGM 重試、或使用者連點）：回原本那一筆。
        let Json(again) = post_deliver(State(app.clone()), Path(id.clone()), Json(deliver_from(&wt))).await.unwrap();
        assert_eq!((again["replayed"].as_bool(), again["sha"].as_str()), (Some(true), Some(sha.as_str())));

        // push 成功但 `delivered` 事件沒寫成（agm.py 30 秒逾時、add_event 回 502）：重試要認出來。
        sqlx::query("DELETE FROM mission_events WHERE mission_id = ? AND kind = 'delivered'").bind(&id).execute(&app.db).await.unwrap();
        let Json(recovered) = post_deliver(State(app.clone()), Path(id.clone()), Json(deliver_from(&wt))).await.unwrap();
        assert_eq!((recovered["sha"].as_str(), recovered["already_in_base"].as_bool()), (Some(sha.as_str()), Some(true)));
        let m = load(&app, &id).await.unwrap();
        assert_eq!(m.status(), "open", "已經在 main 上的交付不能被報成失敗、把任務停下來");
        let events = store::events(&app.db, &id).await.unwrap();
        assert!(events.iter().any(|e| e.kind == "delivered" && e.text.contains("已在 main 上")), "補記一則 delivered");
        assert_eq!(events.iter().filter(|e| e.text.contains("開始交付")).count(), 1, "同一個 commit 只記一次 delivery_attempt");
    }

    /// 使用者取消任務：AGM 被叫醒，底下還開著的交辦一併取消（等額度那件不會幾小時後自己重送）。
    #[tokio::test]
    async fn cancelling_a_mission_wakes_the_manager_and_withdraws_its_open_assignments() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        crate::supervisor::store::get_or_init(&app.db).await.unwrap();
        let Json(m) = post_mission(State(app.clone()), Path(env.project_id.clone()), Json(new_mission("cancel", "pr"))).await.unwrap();
        let id = m["id"].as_str().unwrap().to_string();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "exec").await;
        let a = crate::supervisor::store::insert_assignment(&app.db, None, &bot.id, "crid-exec", "做 X", &[], None, true).await.unwrap();
        crate::supervisor::store::set_mission_link(&app.db, &a.id, &id, "executor").await.unwrap();
        // 撞到 5h 停在 quota_blocked：controller 會在額度回來時自己重送——除非它已經被取消。
        crate::supervisor::store::park_quota_blocked(&app.db, &a.id, "2999-01-01T00:00:00Z", "撞限", "quota_blocked:test", &json!({}))
            .await
            .unwrap();

        let Json(out) = post_cancel(State(app.clone()), Path(id.clone()), HeaderMap::new()).await.unwrap();
        assert_eq!(out["status"], "cancelled");
        let after = crate::supervisor::store::assignment(&app.db, &a.id).await.unwrap().unwrap();
        assert_eq!(after.status, "cancelled", "取消任務要一併收掉還開著的交辦");
        assert!(crate::supervisor::store::quota_blocked_all(&app.db).await.unwrap().is_empty(), "不會再被自動重送");
        assert_eq!(out["assignments"][0]["id"], json!(a.id));
        assert_eq!(inbox_keys(&app, &format!("mission:{id}:cancelled%")).await.len(), 1, "AGM 要被叫醒");
        let payload: String = sqlx::query_scalar("SELECT payload_json FROM supervisor_inbox WHERE kind='mission_cancelled'")
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert!(payload.contains(&a.id), "通知裡帶著要收掉的交辦：{payload}");
        let events = store::events(&app.db, &id).await.unwrap();
        assert!(events.iter().any(|e| e.kind == "note" && e.text.contains("已取消底下 1 件")));
    }

    /// 暫停要叫醒 AGM，而且暫停期間不給交付；AGM 自己暫停不會叫醒它自己。
    #[tokio::test]
    async fn pausing_tells_the_manager_and_closes_the_delivery_gate() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        crate::supervisor::store::get_or_init(&app.db).await.unwrap();
        let (_origin, wt) = with_origin(&env);
        let Json(m) = post_mission(State(app.clone()), Path(env.project_id.clone()), Json(new_mission("pause", "push_main"))).await.unwrap();
        let id = m["id"].as_str().unwrap().to_string();
        commit_file(&wt, "done.txt");
        let _ = post_event(State(app.clone()), Path(id.clone()), Json(verified(Some(&wt), None))).await.unwrap();

        let pause = |reason: &str| PauseIn { reason: reason.into(), detail: Some("使用者按了暫停".into()) };
        let _ = post_pause(State(app.clone()), Path(id.clone()), HeaderMap::new(), Json(pause("user_pause"))).await.unwrap();
        assert_eq!(inbox_keys(&app, &format!("mission:{id}:paused%")).await.len(), 1, "AGM 要知道任務停了");
        let kind: String = sqlx::query_scalar("SELECT kind FROM supervisor_inbox WHERE event_key LIKE ?")
            .bind(format!("mission:{id}:paused%"))
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(kind, "mission_paused");

        let err = post_deliver(State(app.clone()), Path(id.clone()), Json(deliver_from(&wt))).await.unwrap_err();
        assert_eq!(conflict_reason(err), "mission_paused", "停著的任務不交付");

        // 放行之後就交得出去；交付失敗停下來的那種不算「停著」，可以重試。
        let _ = post_resume(State(app.clone()), Path(id.clone())).await.unwrap();
        let Json(out) = post_deliver(State(app.clone()), Path(id.clone()), Json(deliver_from(&wt))).await.unwrap();
        assert_eq!(out["mode"], "push_main");
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
        let env = crate::testing::env().await;
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
            // 這些交辦只是用來把臨時 bot 掛到任務上（這條測的是收尾刪哪幾顆 bot）。收尾時它們一定
            // 已經結案了：三件同時開著本來就違反「一個任務同時只有一件開著的交辦」（SPEC §18.14），
            // 而 `post_complete` 現在會擋（issue #74）。
            sqlx::query("UPDATE supervisor_assignments SET status='completed' WHERE id=?")
                .bind(&a.id)
                .execute(&app.db)
                .await
                .unwrap();
        }

        let (_origin, wt) = with_origin(&env);
        let Json(closed) = post_complete(State(app.clone()), Path(id.clone()), Json(done("完成", Some("no_changes"), Some(&wt))))
            .await
            .unwrap();
        let deleted: Vec<&str> = closed["temp_bots"]["deleted"].as_array().unwrap().iter().map(|b| b["bot_id"].as_str().unwrap()).collect();
        assert_eq!(deleted, ["t-exec"]);
        let skipped: Vec<(&str, &str)> = closed["temp_bots"]["skipped"]
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
        let env = crate::testing::env().await;
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
