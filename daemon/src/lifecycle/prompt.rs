//! Delivering a prompt: the turn row, the pane write, and the queued path.

use super::*;

/// Close a prompt whose local setup failed after its turn was committed; `pending` must never
/// be the last state the frontend sees.
async fn fail_prompt_delivery(app: &Arc<App>, conversation_id: &str, turn_id: &str, reason: &str) {
    let updated = match sqlx::query(
        "UPDATE turns SET delivery='failed', status='failed', completed_at=? WHERE id=? AND status='in_flight'",
    )
    .bind(db::now())
    .bind(turn_id)
    .execute(&app.db)
    .await
    {
        Ok(result) => result,
        Err(e) => {
            tracing::error!(turn = %turn_id, error = %e, "could not fail prompt delivery");
            return;
        }
    };
    if updated.rows_affected() == 0 {
        return;
    }
    let _ = insert_message(
        app,
        conversation_id,
        Some(turn_id),
        "system",
        &format!("delivery failed: {reason}"),
        "system",
        false,
        None,
    )
    .await;
    emit_turn(app, turn_id).await;
}

async fn emit_prompt_message(app: &Arc<App>, bot_id: &str, message_id: &str) {
    if let Ok(Some(m)) = sqlx::query_as::<_, db::Message>("SELECT * FROM messages WHERE id=?")
        .bind(message_id)
        .fetch_optional(&app.db)
        .await
    {
        app.emit("message_added", json!({"bot_id": bot_id, "message": m})).await;
    }
}


/// 記下一則送達：`delivery`（CHECK 只認四種）＋ `delivery_verified`（有沒有證據）＋ `auto_resend`
/// （能不能自動重送）。後兩者是兩件事，見 [`crate::lifecycle::delivery::DeliveryRecord`]。
pub(crate) async fn mark_delivery(app: &Arc<App>, turn_id: &str, rec: DeliveryRecord) {
    let verified = i64::from(rec.verified);
    let auto = i64::from(rec.auto_resend);
    // 不重送的那條路照樣把重送額度用掉：回滾到不認得 `auto_resend` 的舊 binary 時，
    // 它仍然不會把同一則再打一次。
    // `delivered_at` 只記第一次：之後 poller 補證據再記一次，不能讓一則舊的看起來像剛送出（重啟補 watchdog 看它，deliv L3）。
    let _ = sqlx::query(
        "UPDATE turns SET delivery=?, delivery_verified=?, auto_resend=?,
                resend_count = CASE WHEN ? = 0 THEN MAX(resend_count, ?) ELSE resend_count END,
                delivered_at = COALESCE(delivered_at, ?)
          WHERE id=?",
    )
    .bind(rec.stored)
    .bind(verified)
    .bind(auto)
    .bind(auto)
    .bind(crate::lifecycle::MAX_PROMPT_RESENDS)
    .bind(crate::db::now())
    .bind(turn_id)
    .execute(&app.db)
    .await;
}

/// The API answer for a prompt that was not sent. `retry` → 409 (temporary: a busy box, a
/// transcript not reported yet; callers retry and assignments stay queued). Otherwise 422: this
/// prompt can never be proven on this run, and saying so beats holding it forever.
pub(crate) fn not_attempted_error(run_id: &str, not: Delivered) -> LcError {
    let Delivered::NotAttempted { reason, retry } = not else {
        return LcError::Upstream("not_attempted_error called with a delivered outcome".into());
    };
    let detail = json!({"run_id": run_id, "reason": reason, "retryable": retry, "sent": false});
    if retry {
        LcError::conflict(reason, detail)
    } else {
        LcError::Unprocessable(json!({"error": "delivery_unprovable", "reason": reason, "run_id": run_id, "sent": false}))
    }
}

/// 撤回一筆沒送出的 turn 的結果。
#[derive(Debug, PartialEq, Eq)]
enum Retraction {
    /// turn 與它的訊息都撤掉了：同一個 `client_request_id` 可以原樣重送。
    Withdrawn,
    /// turn 已經不是 `in_flight`——窄窗裡別的路徑（`mark_run_exited` → `fail_in_flight`）先把它收掉了。
    /// 一個字都沒刪，照 turn 現在的樣子回答呼叫端。
    AlreadySettled,
}

/// Remove a turn and its user message that never reached the agent. They were committed before the
/// delivery so an early hook could match; nothing was typed, so nothing can match them now.
///
/// 刪之前先確認 turn 還是 `in_flight`，而且跟刪訊息在同一個交易裡：`fail_in_flight` 不拿 per-bot 鎖，
/// 會在「turn 已 commit、第一個字還沒打」這個窄窗裡把它標成 failed 並插一則「run ended」說明。訊息那句
/// 原本不帶條件，於是使用者那顆泡泡跟那則說明被一起刪掉，只留下一個空的 failed 回合（review3 L4）。
async fn retract_unsent_turn(app: &Arc<App>, turn_id: &str, msg_id: &str) -> anyhow::Result<Retraction> {
    let res = async {
        let mut tx = app.db.begin().await?;
        // 訊息要先刪（`messages.turn_id` 指著 turns，反過來會踩到外鍵），turn 那句才是把關的：
        // 刪不到就 rollback，連訊息那句一起退掉——效果就是「turn 不是 in_flight 時一個字都不刪」。
        sqlx::query("DELETE FROM messages WHERE id = ? OR turn_id = ?").bind(msg_id).bind(turn_id).execute(&mut *tx).await?;
        let gone = sqlx::query("DELETE FROM turns WHERE id = ? AND status = 'in_flight'").bind(turn_id).execute(&mut *tx).await?;
        if gone.rows_affected() == 0 {
            tx.rollback().await?;
            return anyhow::Ok(Retraction::AlreadySettled);
        }
        tx.commit().await?;
        anyhow::Ok(Retraction::Withdrawn)
    }
    .await;
    let out = match res {
        Ok(Retraction::Withdrawn) => {
            tracing::info!(turn = turn_id, "retracted a prompt that was never typed into the pane");
            Ok(Retraction::Withdrawn)
        }
        Ok(Retraction::AlreadySettled) => {
            tracing::warn!(turn = turn_id, "an unsent turn had already been closed by another path; left as it is");
            Ok(Retraction::AlreadySettled)
        }
        Err(e) => {
            tracing::error!(turn = turn_id, error = %e, "could not retract an unsent turn; failing it instead");
            let _ = sqlx::query("UPDATE turns SET status='failed', delivery='failed', completed_at=? WHERE id=? AND status='in_flight'")
                .bind(db::now())
                .bind(turn_id)
                .execute(&app.db)
                .await;
            Err(e)
        }
    };
    emit_turn(app, turn_id).await;
    if out.as_ref().map(|r| *r == Retraction::Withdrawn).unwrap_or(false) {
        // 事件模型只有「新增／更新」，沒有「刪除」：`message_added` 與 `turn_updated` 已經廣播出去了，
        // 每個客戶端都收下了那顆泡泡與那筆進行中的回合，而 `emit_turn` 對已刪除的列是 no-op。
        // 不補一次重讀的話，畫面會留著一顆送不出去的幽靈泡泡與一個永遠不會結束的回合（review 2026-09-16）。
        // 沒刪成的兩條路沒有幽靈列要收，`emit_turn` 就夠了。
        app.emit("resync", json!({"reason": "turn_retracted", "turn_id": turn_id})).await;
    }
    out
}

/// 這筆 turn 現在的樣子，換成給呼叫端的答覆。冪等重送（同一個 `client_request_id`）與「撤回時發現
/// turn 已經被別的路徑收掉」都走這裡，同一筆 turn 才不會因為問法不同而拿到兩種答案（review3 L4）。
async fn answer_for_turn(app: &Arc<App>, t: &db::Turn) -> LcResult<PromptOut> {
    let message_id = sqlx::query_scalar::<_, String>("SELECT id FROM messages WHERE turn_id=? AND role='user' LIMIT 1")
        .bind(&t.id)
        .fetch_optional(&app.db)
        .await
        .map_err(up)?
        .unwrap_or_default();
    // The same request asked again reports the same outcome, including "unverified".
    // 還在排隊的那筆照第一次的回答說 `queued`：它的 `delivery` 欄位是 `pending`，原樣回的話
    // 重派的交辦會被記成 delivered/pending，從此不歸排隊保險絲管（review2 deliv L1）。
    let delivery = if t.status == "queued" {
        "queued".to_string()
    } else if t.delivery == "ok" && t.delivery_verified == 0 {
        "unverified".to_string()
    } else {
        t.delivery.clone()
    };
    Ok(PromptOut { turn_id: t.id.clone(), message_id, delivery })
}

/// [`answer_for_turn`]，但 turn 要先從 id 讀回來（撤回撤不掉時，手上只有 id）。
async fn answer_for_turn_id(app: &Arc<App>, turn_id: &str) -> LcResult<PromptOut> {
    let t = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id = ?")
        .bind(turn_id)
        .fetch_optional(&app.db)
        .await
        .map_err(up)?
        .ok_or_else(|| LcError::Upstream("the prompt was not sent and its turn is gone".into()))?;
    answer_for_turn(app, &t).await
}

#[derive(serde::Serialize)]
pub struct PromptOut {
    pub turn_id: String,
    pub message_id: String,
    pub delivery: String,
}

pub async fn prompt(app: &Arc<App>, bot_id: &str, text: &str, client_request_id: &str) -> LcResult<PromptOut> {
    prompt_grouped(app, bot_id, text, client_request_id, None, None, &[], None).await
}

/// `prompt` with images (`attach.rs`): the agent gets paths on its host; the timeline renders
/// thumbnails from `messages.attachments_json`.
pub async fn prompt_with(
    app: &Arc<App>,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    attachment_ids: &[String],
) -> LcResult<PromptOut> {
    prompt_grouped(app, bot_id, text, client_request_id, None, None, attachment_ids, None).await
}

/// 同 `prompt_with`，記下 `relay_from`（bot id 或哨符 `daemon`）；UI 靠它把泡泡畫在左邊。
pub async fn prompt_relayed(
    app: &Arc<App>,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    attachment_ids: &[String],
    relay_from: Option<&str>,
) -> LcResult<PromptOut> {
    prompt_grouped(app, bot_id, text, client_request_id, None, None, attachment_ids, relay_from).await
}

/// AGM 派工專用：對方正在回合中時**排隊**而不是 409（AGM 2026-09-16 裁示）。
///
/// 使用者與 web 的 `/prompt` 不走這條——那條路的語意變更要單獨評估。排進 `queued` 之後由既有的
/// `queue::flush_queued_locked` 在回合結束時送出，送達判定與證據記錄完全沿用（`Handed`／`auto_resend` 不變）。
/// 回傳 `delivery = "queued"` 代表「已排隊、還沒送」。
pub async fn prompt_relayed_queueable(
    app: &Arc<App>,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    relay_from: Option<&str>,
) -> LcResult<PromptOut> {
    prompt_inner(app, bot_id, text, client_request_id, None, None, &[], relay_from, true).await
}

/// 排一筆 `queued` turn 等下一回合（只有 AGM 派工走這裡）。
///
/// 界線都靠 DB：`turns_one_queued`（每個對話最多一筆 queued）擋住「同一顆 bot 疊第二筆」，
/// `turns_client_req` 擋住「同一筆交辦重試疊出第二筆」——撞到就回原本的 409，讓呼叫端照舊退避。
/// `prompt_text` 存要送的內容（含附件路徑展開後的樣子），與 queue flush 用的是同一欄。
#[allow(clippy::too_many_arguments)]
async fn queue_for_next_turn(
    app: &Arc<App>,
    conv: &str,
    bot_id: &str,
    text: &str,
    deliver: &str,
    client_request_id: &str,
    group_id: Option<&str>,
    relay_from: Option<&str>,
) -> LcResult<PromptOut> {
    let turn_id = db::ulid();
    let msg_id = db::ulid();
    let mut tx = app.db.begin().await.map_err(up)?;
    let queued = sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, client_request_id, created_at, prompt_text)
         VALUES (?,?,NULL,'web','queued','pending',?,?,?)",
    )
    .bind(&turn_id)
    .bind(conv)
    .bind(client_request_id)
    .bind(db::now())
    .bind(deliver)
    .execute(&mut *tx)
    .await;
    if let Err(e) = queued {
        // 已經有一筆在排（這顆 bot 或這筆交辦）：照舊回 409，呼叫端退避後再問。
        tracing::info!(bot = %bot_id, error = %e, "另一筆 prompt 已經在排隊，這次照舊回 409");
        return Err(LcError::conflict("a turn is already queued for this bot", json!({"conversation_id": conv})));
    }
    sqlx::query(
        "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, group_id, relay_from, created_at) VALUES (?,?,?,'user',?,'web',?,?,?)",
    )
    .bind(&msg_id)
    .bind(conv)
    .bind(&turn_id)
    .bind(text)
    .bind(group_id)
    .bind(relay_from)
    .bind(db::now())
    .execute(&mut *tx)
    .await
    .map_err(up)?;
    tx.commit().await.map_err(up)?;
    emit_prompt_message(app, bot_id, &msg_id).await;
    emit_turn(app, &turn_id).await;
    tracing::info!(bot = %bot_id, turn = %turn_id, "對方回合中：prompt 排進佇列，等回合結束再送");
    Ok(PromptOut { turn_id, message_id: msg_id, delivery: "queued".into() })
}

/// The screen checks every prompt passes before text enters the pane — shared with the queue
/// flush (review 2026-09-12 #6: the flush skipped them and typed into codex's `/model` menu).
/// Refusals insert a system hint and 409 with `needs_login` / `dialog_open` / `picker_open`.
pub(crate) async fn pane_ready_for_prompt(app: &Arc<App>, bot: &db::Bot, run: &db::Run, conv: &str) -> LcResult<()> {
    // An unlogged claude opens on "Select login method" and looks idle; a prompt would type into the menu.
    if bot.kind == "claude" && crate::tui_prompts::stuck_at_login(app, run).await {
        let identity = bot.identity.clone().unwrap_or_default();
        let hint = if identity.is_empty() {
            "這個 claude 還沒登入：到「終端」分頁選 1 完成登入，或在額度那格按「登入」。".to_string()
        } else {
            format!("身份 `{identity}` 還沒登入：到「終端」分頁選 1 完成登入，或在額度那格按「登入」。")
        };
        let _ = insert_message(app, conv, None, "system", &hint, "system", false, None).await;
        return Err(LcError::conflict("needs_login", json!({"run_id": run.id, "identity": identity, "message": hint})));
    }
    // claude「Switch model?」框被 herdr 判成 idle，prompt 打進去會被吃、Enter 按了 Yes（2026-09-11
    // AGM 實測）。還看得到框＝有人在終端手動 `/model`；使用者要送訊息，按 Esc 退掉再送，退不掉就講清楚。
    if bot.kind == "claude" {
        if let Some(pane) = run.pane_id.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
            if let Ok(client) = client_for_run(app, run).await {
                if let Ok(r) = client.pane_read(pane, "visible", 60).await {
                    if crate::tui_prompts::is_switch_model_dialog(&r.text) {
                        let _ = client.pane_send_keys(pane, &["Escape"]).await;
                        tokio::time::sleep(Duration::from_millis(700)).await;
                        let still = matches!(client.pane_read(pane, "visible", 60).await,
                            Ok(r2) if crate::tui_prompts::is_switch_model_dialog(&r2.text));
                        if still {
                            let hint = "claude 的「Switch model?」確認框擋在輸入列前面，關不掉。請到「終端」分頁選 1 或 2 再送一次。";
                            let _ = insert_message(app, conv, None, "system", hint, "system", false, None).await;
                            return Err(LcError::conflict("dialog_open", json!({"run_id": run.id, "message": hint})));
                        }
                        tracing::info!(run = %run.id, "closed a leftover claude model-switch confirmation before delivering a prompt");
                    }
                }
            }
        }
    }
    // grok 1.0.34 在沒信任過的目錄開「Do you trust the contents of this directory?」，herdr 不認得，
    // prompt 會被吃掉（2026-09-17）。bot 的 cwd 本來就預先信任（claude／codex 同一套），所以寫入信任紀錄、
    // 替它按 `y`；還在就講清楚。
    if bot.kind == "grok" {
        if let Some(pane) = run.pane_id.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
            if let Ok(client) = client_for_run(app, run).await {
                if let Ok(r) = client.pane_read(pane, "visible", 60).await {
                    if crate::tui_prompts::is_grok_trust_dialog(&r.text) {
                        for w in crate::trust::pretrust_bots(app, std::slice::from_ref(bot)).await {
                            tracing::warn!(bot = %bot.name, warning = %w, "could not record grok folder trust");
                        }
                        let _ = client.pane_send_keys(pane, &["y"]).await;
                        tokio::time::sleep(Duration::from_millis(1200)).await;
                        let still = matches!(client.pane_read(pane, "visible", 60).await,
                            Ok(r2) if crate::tui_prompts::is_grok_trust_dialog(&r2.text));
                        if still {
                            let hint = "grok 在問「要不要信任這個目錄」，自動按 y 沒有關掉。請到「終端」分頁按 y 再送一次。";
                            let _ = insert_message(app, conv, None, "system", hint, "system", false, None).await;
                            return Err(LcError::conflict("dialog_open", json!({"run_id": run.id, "message": hint})));
                        }
                        tracing::info!(run = %run.id, "answered grok's folder-trust dialog before delivering a prompt");
                    }
                }
            }
        }
    }
    // codex `/model` 選單開著時，prompt 會變成選單操作、Enter 換掉模型（2026-09-10 實測）。先關掉，關不掉就講清楚。
    if bot.kind == "codex" {
        if let Some(pane) = run.pane_id.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
            if let Ok(client) = client_for_run(app, run).await {
                if !crate::codex_live::close_picker(&client, pane).await {
                    let hint = "codex 的 /model 選單擋在輸入列前面，關不掉。請到「終端」分頁按 Esc 回到輸入列再送一次。";
                    let _ = insert_message(app, conv, None, "system", hint, "system", false, None).await;
                    return Err(LcError::conflict("picker_open", json!({"run_id": run.id, "message": hint})));
                }
            }
        }
    }
    Ok(())
}

pub async fn prompt_grouped(
    app: &Arc<App>,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    group_id: Option<&str>,
    deliver: Option<&str>,
    attachment_ids: &[String],
    relay_from: Option<&str>,
) -> LcResult<PromptOut> {
    prompt_inner(app, bot_id, text, client_request_id, group_id, deliver, attachment_ids, relay_from, false).await
}

#[allow(clippy::too_many_arguments)]
async fn prompt_inner(
    app: &Arc<App>,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    group_id: Option<&str>,
    deliver: Option<&str>,
    attachment_ids: &[String],
    // `None` = 使用者自己在畫面上打的。
    relay_from: Option<&str>,
    // 對方回合中時排隊而不是 409。只有 AGM 派工會給 true。
    queue_if_busy: bool,
) -> LcResult<PromptOut> {
    let deliver = deliver.unwrap_or(text);
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;

    if client_request_id.trim().is_empty() {
        return Err(LcError::Bad("client_request_id must not be empty".into()));
    }
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let conv = db::conversation_id(&app.db, bot_id).await.map_err(up)?;
    // Resolve first so an unknown id is a plain 400, not an undelivered turn.
    let files = crate::attach::resolve(app, bot_id, attachment_ids)
        .await
        .map_err(|e| LcError::Bad(e.to_string()))?;
    let deliver = crate::attach::deliver_text(deliver, &files);

    // 2. idempotency
    if let Some(t) = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE conversation_id=? AND client_request_id=?")
        .bind(&conv)
        .bind(client_request_id)
        .fetch_optional(&app.db)
        .await
        .map_err(up)?
    {
        return answer_for_turn(app, &t).await;
    }

    // 1. preconditions
    let run = db::active_run(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| {
        LcError::conflict("bot has no active run", json!({}))
    })?;
    if run.state != "running" {
        return Err(LcError::conflict("run is not running", json!({"run_id": run.id, "state": run.state})));
    }
    if run.agent_status == "blocked" {
        return Err(LcError::conflict("agent is blocked; answer the prompt first", json!({"run_id": run.id})));
    }
    if let Some(t) = db::in_flight_turn(&app.db, &run.id).await.map_err(up)? {
        if queue_if_busy {
            return queue_for_next_turn(app, &conv, bot_id, text, &deliver, client_request_id, group_id, relay_from).await;
        }
        return Err(LcError::conflict("a turn is already in flight", json!({"turn_id": t.id})));
    }
    // AGM 派工遇到「使用者剛按 Esc」：一樣先讓使用者拿回輸入框——排進佇列，寬限到了才送（§4.4a）。
    if queue_if_busy {
        if let Some(wait) = super::interrupt_grace::hold(app, &bot, &run, &conv).await {
            let out = queue_for_next_turn(app, &conv, bot_id, text, &deliver, client_request_id, group_id, relay_from).await?;
            schedule_flush_retry(app, bot_id, wait);
            return Ok(out);
        }
    }
    pane_ready_for_prompt(app, &bot, &run, &conv).await?;
    if let Some(t) = sqlx::query_as::<_, db::Turn>(
        "SELECT * FROM turns WHERE conversation_id=? AND delivery='unknown' AND status='in_flight' LIMIT 1",
    )
    .bind(&conv)
    .fetch_optional(&app.db)
    .await
    .map_err(up)?
    {
        return Err(LcError::conflict("a previous turn has unknown delivery; abandon it first", json!({"turn_id": t.id})));
    }
    // Resolve the client before committing: must stay a retryable 502, not a stuck `pending` turn.
    let client = client_for_run(app, &run).await?;
    // Decide how it will be delivered before a turn exists: a prompt that cannot be sent right now
    // (box busy, no way to prove it) must never become an in-flight turn nobody can release
    // (sol review round seven #2). A 409 keeps a supervisor assignment queued with backoff.
    // A direct prompt never waits inside the request: a missing codex rollout answers 409 and the
    // caller (or AGM's backoff) asks again.
    let plan = match plan_delivery(app, &client, &run, &bot, &deliver, false, false).await.map_err(up)? {
        Ok(plan) => plan,
        Err(not) => return Err(not_attempted_error(&run.id, not)),
    };

    // 3. turn + user message committed BEFORE the RPC, so an early hook can match.
    // `prompt_text` 存**實際送出**的字（群組去掉 @mention、附件路徑展開後），跟排隊那條同一欄：
    // stall 重送、畫面比對、hook 對 prompt 都讀它，不讀泡泡原文（review3 c3 M4）。
    let turn_id = db::ulid();
    let mut tx = app.db.begin().await.map_err(up)?;
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, client_request_id, created_at, prompt_text)
         VALUES (?,?,?,'web','in_flight','pending',?,?,?)",
    )
    .bind(&turn_id)
    .bind(&conv)
    .bind(&run.id)
    .bind(client_request_id)
    .bind(db::now())
    .bind(&deliver)
    .execute(&mut *tx)
    .await
    .map_err(up)?;
    let msg_id = db::ulid();
    sqlx::query(
        "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, group_id, relay_from, created_at) VALUES (?,?,?,'user',?,'web',?,?,?)",
    )
    .bind(&msg_id)
    .bind(&conv)
    .bind(&turn_id)
    .bind(text)
    .bind(group_id)
    .bind(relay_from)
    .bind(db::now())
    .execute(&mut *tx)
    .await
    .map_err(up)?;
    tx.commit().await.map_err(up)?;
    if let Err(e) = crate::attach::bind(app, &msg_id, &files).await {
        emit_prompt_message(app, bot_id, &msg_id).await;
        fail_prompt_delivery(app, &conv, &turn_id, &format!("attachment binding failed: {e}")).await;
        return Ok(PromptOut { turn_id, message_id: msg_id, delivery: "failed".into() });
    }
    emit_prompt_message(app, bot_id, &msg_id).await;
    emit_turn(app, &turn_id).await;

    // 4. deliver
    let res = execute_delivery(app, &client, &run, &bot, &deliver, plan).await;
    // `delivery` 是回給呼叫端／UI 的字；`rec` 是要寫進 DB 的兩個欄位（證據、能不能重送）。
    let mut rec = DeliveryRecord { stored: "unknown", verified: false, auto_resend: true };
    let delivery = match res {
        // 打字＋無損證據。`Handed`（agent.prompt）沒有證據，API 也照證據說「unverified」，
        // 但它照舊可以自動重送（AGM 2026-09-16：證據與重送分開）。
        Ok(d @ (Delivered::Submitted | Delivered::Handed)) => {
            rec = d.record().expect("delivered outcome records");
            if matches!(d, Delivered::Submitted) { "ok" } else { "unverified" }
        }
        // Typed and submitted on a run with no lossless evidence (grok, remote, codex before its
        // session is known): delivered as far as anyone can tell, marked for a human, never re-sent.
        Ok(d @ Delivered::Unverified) => {
            rec = d.record().expect("delivered outcome records");
            "unverified"
        }
        // The box filled between the plan and the first keystroke: nothing was sent. Take the turn
        // back out so the same request id can be sent again, and answer 409 like the plan would.
        Ok(not @ Delivered::NotAttempted { .. }) => {
            match retract_unsent_turn(app, &turn_id, &msg_id).await {
                // 撤掉了：同一個 request id 原樣重送會重來一次，照計畫階段的答案回 409／422。
                Ok(Retraction::Withdrawn) => return Err(not_attempted_error(&run.id, not)),
                // 窄窗裡別的路徑（`mark_run_exited` → `fail_in_flight`）已經把這筆收掉並插了說明：
                // 一個字都沒刪，照 turn 現況回。回可重試的 409 只會叫呼叫端用同一個 request id 重送，
                // 而重送從冪等分支拿到的就是這一筆收掉的回合——兩次答案不一致，交辦還被記成送達失敗
                // （review3 L4）。
                Ok(Retraction::AlreadySettled) => return answer_for_turn_id(app, &turn_id).await,
                // If the turn cannot be taken back, the same request id would only ever find a failed
                // turn: answer 5xx (final for this id), not a retryable 409.
                Err(e) => return Err(LcError::Upstream(format!("prompt was not sent and its turn could not be withdrawn: {e}"))),
            }
        }
        // Keys were sent and the result cannot be proven: that is what `unknown` means (§6.3).
        Ok(Delivered::Unproven(why)) => {
            tracing::warn!(bot = %bot_id, reason = why, "prompt delivery could not be proven");
            "unknown"
        }
        Err(e) => {
            let blocked = e.downcast_ref::<HerdrError>().map(|h| h.code == "agent_blocked").unwrap_or(false);
            if blocked {
                let _ = sqlx::query("UPDATE turns SET delivery='failed', status='failed', completed_at=? WHERE id=?")
                    .bind(db::now())
                    .bind(&turn_id)
                    .execute(&app.db)
                    .await;
                let _ = insert_message(app, &conv, Some(&turn_id), "system", &format!("delivery failed: {e}"), "system", false, None).await;
                emit_turn(app, &turn_id).await;
                return Ok(PromptOut { turn_id, message_id: msg_id, delivery: "failed".into() });
            }
            tracing::warn!(error = %e, "agent.prompt delivery unknown");
            "unknown"
        }
    };
    mark_delivery(app, &turn_id, rec).await;
    emit_turn(app, &turn_id).await;
    if delivery == "ok" || delivery == "unverified" {
        arm_stall(app, &run.id, bot_id, &turn_id).await;
        arm_progress(app, &run.id, bot_id, &turn_id).await;
    }
    Ok(PromptOut { turn_id, message_id: msg_id, delivery: delivery.into() })
}

#[cfg(test)]
mod prompt_tests {
    use super::*;
    use crate::testing as tt;

    struct Fixture {
        env: tt::Env,
        bot_id: String,
        conv: String,
        run_id: String,
    }

    async fn fixture(kind: &str, session: &str) -> Fixture {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'prompt-test',?,'[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(kind)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?)",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .bind(session)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        // These exercise the agent.prompt path, which needs an agent herdr has a session bound to.
        env.herdr.set_agent("prompt-test", "pane-prompt-test", true);
        Fixture { env, bot_id, conv, run_id }
    }

    async fn attachment(app: &Arc<App>, bot_id: &str) -> String {
        let id = db::ulid();
        sqlx::query(
            "INSERT INTO attachments (id, bot_id, name, mime, size, local_path, agent_path, host, created_at)
             VALUES (?,?,'image.png','image/png',1,'/tmp/image.png','/tmp/image.png','local',?)",
        )
        .bind(&id)
        .bind(bot_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        id
    }

    /// 在對方回合中派工：AGM 那條路排隊（queued turn），使用者的 `/prompt` 仍是 409（AGM 2026-09-16 裁示）。
    #[tokio::test]
    async fn an_agm_dispatch_queues_behind_an_in_flight_turn_but_a_user_prompt_still_gets_409() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        // 先佔住一個回合。
        let busy = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at)
             VALUES (?,?,?,'web','in_flight','ok',?)",
        )
        .bind(&busy)
        .bind(&f.conv)
        .bind(&f.run_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();

        // 使用者：照舊 409，不排隊。
        let user = prompt(&app, &f.bot_id, "使用者自己打的", "user-1").await;
        assert!(matches!(user, Err(LcError::Conflict(_))), "使用者的 /prompt 仍是 409");

        // AGM 派工：排一筆 queued，內容存在 prompt_text 等 flush。
        let out = prompt_relayed_queueable(&app, &f.bot_id, "AGM 派的工作", "agm-1", Some("agm-bot")).await.unwrap();
        assert_eq!(out.delivery, "queued");
        let (status, run_id, text): (String, Option<String>, Option<String>) =
            sqlx::query_as("SELECT status, run_id, prompt_text FROM turns WHERE id=?")
                .bind(&out.turn_id)
                .fetch_one(&app.db)
                .await
                .unwrap();
        assert_eq!((status.as_str(), run_id, text.as_deref()), ("queued", None, Some("AGM 派的工作")));

        // 同一筆交辦重試：回原本那一筆，不會疊第二筆。
        let again = prompt_relayed_queueable(&app, &f.bot_id, "AGM 派的工作", "agm-1", Some("agm-bot")).await.unwrap();
        assert_eq!(again.turn_id, out.turn_id);
        assert_eq!(again.delivery, "queued", "重問一筆還在排隊的，回答跟第一次一樣（不是 turn 欄位上的 pending）");
        // 另一筆交辦想排第二筆：擋下來，照舊 409（每個對話只留一筆 queued）。
        let other = prompt_relayed_queueable(&app, &f.bot_id, "另一件事", "agm-2", Some("agm-bot")).await;
        assert!(matches!(other, Err(LcError::Conflict(_))), "每個對話只留一筆 queued");
        let queued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=? AND status='queued'")
            .bind(&f.conv)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(queued, 1);
    }

    /// 群組訊息：泡泡留原文（含 @mention），turn 上記實際送出的字，stall 重送與畫面比對讀它（review3 c3 M4）。
    #[tokio::test]
    async fn a_direct_prompt_records_the_text_actually_delivered_on_its_turn() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        let out = prompt_grouped(&app, &f.bot_id, "@prompt-test 跑一次測試", "group-1:b", Some("group-1"), Some("跑一次測試"), &[], None)
            .await
            .unwrap();
        let (bubble, delivered): (String, Option<String>) = sqlx::query_as(
            "SELECT m.content, t.prompt_text FROM turns t JOIN messages m ON m.turn_id = t.id AND m.role = 'user' WHERE t.id = ?",
        )
        .bind(&out.turn_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
        assert_eq!(bubble, "@prompt-test 跑一次測試", "泡泡照使用者打的");
        assert_eq!(delivered.as_deref(), Some("跑一次測試"), "turn 記的是送給 bot 的字");
    }

    /// 打字前 `runs.pane_typed` 寫不進去（SQLite 鎖逾時、磁碟滿）：一個字都沒打，所以是 409＋撤回 turn，
    /// 不能留一筆 `unknown` 的 in_flight 擋住之後每一則、5 分鐘後又被收成 completed_fallback（review3 c4 L5）。
    #[tokio::test]
    async fn a_pane_typed_marker_that_cannot_be_written_is_a_retryable_409_with_no_turn_left() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id = 'pane-prompt-test', pane_typed = 1 WHERE id = ?")
            .bind(&f.run_id)
            .execute(&app.db)
            .await
            .unwrap();
        f.env.herdr.live_pane("pane-prompt-test", tt::LivePane { width: Some(120), ..Default::default() });
        sqlx::query(
            "CREATE TRIGGER pane_typed_unwritable BEFORE UPDATE OF pane_typed ON runs
             BEGIN SELECT RAISE(ABORT, 'database or disk is full'); END",
        )
        .execute(&app.db)
        .await
        .unwrap();

        let out = prompt(&app, &f.bot_id, "Reply with PONG please", "prompt-pane-typed").await;
        let Err(LcError::Conflict(body)) = out else { panic!("expected a retryable 409, got {:?}", out.map(|o| o.delivery)) };
        assert_eq!(body["reason"], "pane_typed_unwritable", "{body}");
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=?")
            .bind(&f.conv)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(turns, 0, "沒送出的 turn 被撤回，不是留成 unknown");
        assert_eq!(f.env.herdr.methods().iter().filter(|m| *m == "pane.send_text").count(), 0, "一個字都沒打");
    }

    /// 窄窗：turn 已經 commit、第一個字還沒打的時候 pane 死掉，事件路徑 `mark_run_exited` →
    /// `fail_in_flight`（不拿 per-bot 鎖）把這筆標 failed 並插了「run ended」說明。撤回這時候
    /// 一個字都不能刪——訊息那句原本不帶條件，會把使用者的泡泡跟那則說明一起刪掉，只留一個空的
    /// failed 回合，還回可重試的 409 叫呼叫端重送，重送又從冪等分支拿到那筆失敗的回合（review3 L4）。
    ///
    /// `BEFORE UPDATE OF pane_typed` 的 trigger 正好落在那個窗裡：`RAISE(FAIL)` 只中止外層那句
    /// UPDATE，trigger 自己先做的兩筆寫入留著，等於另一條路徑插進來的效果。
    #[tokio::test]
    async fn a_turn_the_run_end_already_failed_is_left_alone_and_answered_like_a_resend() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id = 'pane-prompt-test', pane_typed = 1 WHERE id = ?")
            .bind(&f.run_id)
            .execute(&app.db)
            .await
            .unwrap();
        f.env.herdr.live_pane("pane-prompt-test", tt::LivePane { width: Some(120), ..Default::default() });
        sqlx::query(
            "CREATE TRIGGER the_run_ends_mid_delivery BEFORE UPDATE OF pane_typed ON runs
             BEGIN
               INSERT INTO messages (id, conversation_id, turn_id, role, content, source, created_at)
                 SELECT 'msg-run-ended', t.conversation_id, t.id, 'system', 'run ended: pane died', 'system', datetime('now')
                   FROM turns t WHERE t.run_id = NEW.id AND t.status = 'in_flight';
               UPDATE turns SET status = 'failed', completed_at = datetime('now')
                 WHERE run_id = NEW.id AND status = 'in_flight';
               SELECT RAISE(FAIL, 'run ended mid-delivery');
             END",
        )
        .execute(&app.db)
        .await
        .unwrap();

        let out = prompt(&app, &f.bot_id, "Reply with PONG please", "prompt-raced").await.expect("不是可重試的 409");

        // 那一筆回合留著，使用者的訊息與「run ended」說明都在。
        let t: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE id = ?").bind(&out.turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(t.status, "failed", "收掉它的是 run 結束那條路，不是撤回");
        let msgs: Vec<(String, String)> = sqlx::query_as("SELECT role, content FROM messages WHERE turn_id = ? ORDER BY role")
            .bind(&out.turn_id)
            .fetch_all(&app.db)
            .await
            .unwrap();
        assert_eq!(
            msgs,
            vec![("system".to_string(), "run ended: pane died".to_string()), ("user".to_string(), "Reply with PONG please".to_string())],
            "使用者的泡泡與那則說明都不該被刪掉",
        );
        assert!(!out.message_id.is_empty(), "回的是那顆還在的泡泡");
        assert_eq!(f.env.herdr.methods().iter().filter(|m| *m == "pane.send_text").count(), 0, "一個字都沒打");

        // 同一個 request id 重送：走冪等分支，答案必須跟剛剛那次一模一樣。
        let again = prompt(&app, &f.bot_id, "Reply with PONG please", "prompt-raced").await.unwrap();
        assert_eq!(
            (again.turn_id, again.message_id, again.delivery),
            (out.turn_id, out.message_id, out.delivery),
            "撤不掉時的回答要跟同一個 request id 重送拿到的一致",
        );
    }

    /// A missing run session is rejected before writing, so a retry reports the same upstream problem.
    #[tokio::test]
    async fn an_unavailable_run_session_does_not_create_a_turn() {
        let f = fixture("codex", "no-such-session").await;
        let app = f.env.app.clone();

        assert!(matches!(prompt(&app, &f.bot_id, "first", "prompt-1").await, Err(LcError::Upstream(_))));
        assert!(db::in_flight_turn(&app.db, &f.run_id).await.unwrap().is_none());
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=?")
            .bind(&f.conv)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(turns, 0, "the unavailable client was checked before INSERT");

        assert!(matches!(prompt(&app, &f.bot_id, "second", "prompt-2").await, Err(LcError::Upstream(_))));
        assert!(db::in_flight_turn(&app.db, &f.run_id).await.unwrap().is_none());
    }

    /// 別的 bot／排程送進來的 prompt 要留 `relay_from`，UI 才分得出來源（2026-09-12 使用者）。
    #[tokio::test]
    async fn a_relayed_prompt_records_who_sent_it() {
        // 一顆 bot 同時只有一個回合在飛：各用一個 fixture。
        let user = fixture("codex", "test").await;
        let user_app = user.env.app.clone();
        let mine = prompt_with(&user_app, &user.bot_id, "使用者自己打的", "prompt-user", &[]).await.unwrap();

        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        let relayed = prompt_relayed(&app, &f.bot_id, "排程派的", "prompt-daemon", &[], Some(crate::agent_relay::DAEMON_SENDER))
            .await
            .unwrap();

        let from = |db: sqlx::SqlitePool, id: &str| {
            let db = db.clone();
            let id = id.to_string();
            async move {
                sqlx::query_scalar::<_, Option<String>>("SELECT relay_from FROM messages WHERE id = ?")
                    .bind(&id)
                    .fetch_one(&db)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(from(user_app.db.clone(), &mine.message_id).await, None, "使用者自己打的不該有來源標");
        assert_eq!(
            from(app.db.clone(), &relayed.message_id).await,
            Some(crate::agent_relay::DAEMON_SENDER.to_string())
        );
    }

    /// Binding can fail after the turn commits; the UI must still get a terminal turn event.
    #[tokio::test]
    async fn an_attachment_bind_failure_closes_the_pending_turn() {
        let success = fixture("codex", "test").await;
        let success_app = success.env.app.clone();
        let success_attachment = attachment(&success_app, &success.bot_id).await;
        let mut success_events = success_app.subscribe();
        let success_out = prompt_with(&success_app, &success.bot_id, "look", "prompt-attachments-ok", &[success_attachment])
            .await
            .unwrap();
        let success_event = tokio::time::timeout(std::time::Duration::from_secs(1), success_events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(success_event.kind, "message_added");
        assert_eq!(success_event.data["message"]["id"], success_out.message_id);
        assert!(!success_event.data["message"]["attachments_json"].is_null());

        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        let attachment_id = attachment(&app, &f.bot_id).await;
        sqlx::query(
            "CREATE TRIGGER fail_prompt_attachment_bind
             BEFORE UPDATE OF message_id ON attachments
             BEGIN SELECT RAISE(ABORT, 'bind failed'); END",
        )
        .execute(&app.db)
        .await
        .unwrap();
        let mut ws_events = app.subscribe();
        let mut turn_events = app.subscribe_turns();

        let out = prompt_with(&app, &f.bot_id, "look", "prompt-attachments", &[attachment_id]).await.unwrap();
        assert_eq!(out.delivery, "failed");
        let user_event = tokio::time::timeout(std::time::Duration::from_secs(1), ws_events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(user_event.kind, "message_added");
        assert_eq!(user_event.data["message"]["id"], out.message_id);
        assert_eq!(user_event.data["message"]["role"], "user");
        assert!(db::in_flight_turn(&app.db, &f.run_id).await.unwrap().is_none());
        let turn: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE id=?")
            .bind(&out.turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!((turn.status.as_str(), turn.delivery.as_str()), ("failed", "failed"));
        let system: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'")
            .bind(&out.turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert!(system.contains("attachment binding failed"));
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), turn_events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.turn_id, out.turn_id);
        assert_eq!((event.status.as_str(), event.delivery.as_str()), ("failed", "failed"));
    }
}
