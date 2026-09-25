//! 輸入框卡著一段沒送出的字（409 `composer_busy`）時，網頁上能做的兩件事（2026-09-26 w16T:p3：claude 的
//! 「Edit prompt and retry」把上一則放回框裡，網頁每送一則都 409，使用者只能自己去終端處理）：
//!
//! * **送出框裡那段**（[`submit`]）：對 pane 按 Enter，照一般 prompt 開回合、證明送達——不是把字重打一次。
//! * **清掉再送我這則**（[`clear`]，`prompt` 帶 `clear_draft`）：清框、重讀畫面確認框是空的，才照一般流程打字。
//!
//! 兩件事都先確認框裡**還是使用者看到的那一段**（`expect_draft`＝409 回的 `draft`）：框在這之間換了字（有人在終端打字、
//! CLI 自己放回一段），送出或清掉的就是使用者沒看過的東西——回 409 `draft_changed` 帶新的草稿，一個鍵都不按。

use super::*;

/// 回給網頁顯示的草稿最多幾個字（`draft_truncated` 說有沒有截）。`expect_draft` 比對的也是截過的這一段。
pub(crate) const DRAFT_SHOWN_CHARS: usize = 500;

/// 清框的鍵按下去之後，等 TUI 重畫多久再重讀。
const CLEAR_SETTLE_MS: u64 = 700;

/// 清框的鍵。三種 kind 都是一次 `ctrl+c`：2026-09-26 在隔離的 herdr session（`env -i` + `--session`，daemon 看不到）
/// 實測 claude 2.1.281、codex 0.155.1、grok 1.0.41，單行、多行、claude 摺起來的長段貼上，一次就清空、重讀是空框
/// （fixtures `*-draft.ansi`／`*-cleared.ansi`）。只在**框裡有字、沒有回合在跑**的時候按：空框的 `ctrl+c` 是「再按一次離開」，
/// 回合中的 `ctrl+c` 會打斷它。沒驗過的 kind 不給這個動作。
fn clear_keys(kind: &str) -> Option<&'static [&'static str]> {
    match kind {
        "claude" | "codex" | "grok" => Some(&["ctrl+c"]),
        _ => None,
    }
}

/// 「送出框裡那段」按的就是一般送出用的 Enter（[`Submit::Enter`]）：三種 kind 的打字送出本來就靠它。
fn can_submit(kind: &str) -> bool {
    matches!(kind, "claude" | "codex" | "grok")
}

/// 這個 kind 在網頁上給哪幾個動作。
pub(crate) fn actions(kind: &str) -> Vec<&'static str> {
    let mut out = Vec::new();
    if can_submit(kind) {
        out.push("submit");
    }
    if clear_keys(kind).is_some() {
        out.push("clear");
    }
    out
}

/// 顯示用的草稿：最多 [`DRAFT_SHOWN_CHARS`] 個字，第二個值＝有沒有截掉。
pub(crate) fn shown(draft: &str) -> (String, bool) {
    let cut = draft.chars().count() > DRAFT_SHOWN_CHARS;
    (draft.chars().take(DRAFT_SHOWN_CHARS).collect(), cut)
}

/// 框裡現在這段是不是使用者確認過的那一段。
fn same_draft(expect: &str, now: &str) -> bool {
    shown(now).0 == expect
}

/// 409 body 裡描述草稿的欄位。讀不出字（`None`）就不給動作：不知道框裡是什麼，就不替使用者送出或清掉。
fn draft_fields(kind: &str, draft: Option<&str>) -> Value {
    match draft {
        Some(d) => {
            let (text, truncated) = shown(d);
            json!({"draft": text, "draft_truncated": truncated, "draft_actions": actions(kind)})
        }
        None => json!({"draft": null, "draft_truncated": false, "draft_actions": []}),
    }
}

fn pane_of(run: &db::Run) -> Option<String> {
    run.pane_id.as_deref().map(str::trim).filter(|p| !p.is_empty()).map(str::to_string)
}

/// 框的狀態＋框裡的字（跟送 prompt 前的檢查同一種讀法：帶樣式，TUI 自己畫的提示不算字）。
async fn read_draft(client: &HerdrClient, pane: &str, kind: &str) -> anyhow::Result<(BoxState, Option<String>)> {
    let screen = read_styled(client, pane, SCAN_SOURCE, DELIVER_SCAN_LINES).await?;
    let state = box_state(kind, &screen);
    let text = (state == BoxState::NonEmpty).then(|| composer_text(kind, &screen)).flatten();
    Ok((state, text))
}

fn refusal(reason: &str, run: &db::Run, retryable: bool, kind: &str, draft: Option<&str>) -> LcError {
    let mut extra = json!({"run_id": run.id, "retryable": retryable, "sent": false});
    if let (Some(o), Some(f)) = (extra.as_object_mut(), draft_fields(kind, draft).as_object()) {
        o.extend(f.clone());
    }
    LcError::conflict(reason, extra)
}

fn unreadable(run: &db::Run) -> LcError {
    not_attempted_error(&run.id, Delivered::NotAttempted { reason: "composer_unreadable", retry: true })
}

/// 409 `composer_busy` 補上框裡現在的字（`draft`／`draft_truncated`／`draft_actions`），網頁才有東西給使用者看、可以處理。
/// 其他錯誤原樣回。擋下之後才讀的：這一刻的畫面，讀不到就是 `draft: null`（沒有動作），409 照回。
pub(crate) async fn with_draft(client: &HerdrClient, run: &db::Run, bot: &db::Bot, err: LcError) -> LcError {
    let LcError::Conflict(mut body) = err else { return err };
    if body.get("reason").and_then(Value::as_str) != Some("composer_busy") {
        return LcError::Conflict(body);
    }
    let draft = match pane_of(run) {
        Some(pane) => read_draft(client, &pane, &bot.kind).await.ok().and_then(|(_, d)| d),
        None => None,
    };
    if let (Some(o), Some(f)) = (body.as_object_mut(), draft_fields(&bot.kind, draft.as_deref()).as_object()) {
        o.extend(f.clone());
    }
    LcError::Conflict(body)
}

/// 清掉框裡使用者確認過的那段（`expect`），**重讀畫面確認框是空的**才回 `Ok`；清不掉就回錯，呼叫端一個字都不打。
/// 框本來就空了＝沒有東西要清。`busy`＝有回合在跑（插隊送出）：`ctrl+c` 會打斷它，不按。
pub(crate) async fn clear(client: &HerdrClient, run: &db::Run, bot: &db::Bot, expect: &str, busy: bool) -> LcResult<()> {
    let Some(keys) = clear_keys(&bot.kind) else {
        return Err(refusal("draft_clear_unsupported", run, false, &bot.kind, None));
    };
    if busy {
        return Err(LcError::conflict("draft_clear_while_busy", json!({"run_id": run.id, "retryable": true, "sent": false})));
    }
    let Some(pane) = pane_of(run) else { return Ok(()) };
    let (state, now) = read_draft(client, &pane, &bot.kind).await.map_err(|_| unreadable(run))?;
    match state {
        BoxState::Empty => return Ok(()),
        BoxState::Unready => return Err(unreadable(run)),
        BoxState::NonEmpty => {}
    }
    if !now.as_deref().is_some_and(|d| same_draft(expect, d)) {
        return Err(refusal("draft_changed", run, true, &bot.kind, now.as_deref()));
    }
    if let Err(e) = client.pane_send_keys(&pane, keys).await {
        tracing::warn!(run = %run.id, bot = %bot.name, error = %e, "herdr refused the key that clears the composer; not typing");
        return Err(refusal("draft_uncleared", run, false, &bot.kind, now.as_deref()));
    }
    #[cfg(test)]
    super::race_point::hit("draft_after_clear_key", &bot.id).await;
    tokio::time::sleep(std::time::Duration::from_millis(CLEAR_SETTLE_MS)).await;
    match read_draft(client, &pane, &bot.kind).await {
        Ok((BoxState::Empty, _)) => {
            tracing::info!(run = %run.id, bot = %bot.name, "cleared the draft the user confirmed; the composer reads empty");
            Ok(())
        }
        Ok((_, left)) => {
            tracing::warn!(run = %run.id, bot = %bot.name, "the composer is not empty after clearing; not typing");
            Err(refusal("draft_uncleared", run, false, &bot.kind, left.as_deref()))
        }
        Err(_) => Err(refusal("draft_uncleared", run, false, &bot.kind, None)),
    }
}

/// 送出框裡那段的證據（跟一般 prompt 同一張表，SPEC §4.4a）：本機 claude／codex 的 session log 在基準之後
/// 出現**一則新的使用者訊息**。框裡的字是從畫面讀的，不能拿來逐字比對（`delivery` 開頭：軟換行、空白、摺起來的貼上），
/// 所以比的是「多了一則」而不是「多了這一則」——閒著的 bot、框裡只有這一段，Enter 送出去的就是它。
/// 其他情況沒有無損證據：框在 Enter 後清空就是 `Unverified`。
async fn draft_proof(app: &Arc<App>, client: &HerdrClient, run: &db::Run, bot: &db::Bot, pane: &str, draft: &str) -> LcResult<Proof> {
    let host_is_local = match db::project(&app.db, &bot.project_id).await {
        Ok(p) => p.is_some_and(|p| p.host == LOCAL_HOST),
        Err(_) => return Err(not_attempted_error(&run.id, Delivered::NotAttempted { reason: "host_unreadable", retry: true })),
    };
    let codex_log = match (bot.kind.as_str(), host_is_local, run.native_session_id.as_deref()) {
        ("codex", true, Some(session)) => codex_home(app, bot).await.and_then(|h| codex_session_log(&h, session)),
        _ => None,
    };
    let inputs = ProofInputs {
        kind: &bot.kind,
        host_is_local,
        hooks: bot.inject_hooks != 0,
        session_id: run.native_session_id.as_deref(),
        transcript_path: run.transcript_path.as_deref(),
        codex_log,
        waited_for_log: false,
        pane_cols: client.pane_size(pane).await.ok().flatten().map(|(w, _)| w),
    };
    match choose_proof(&inputs, draft) {
        Ok(p @ Proof::Transcript { .. }) => Ok(p),
        // 一列回音要逐字等於送出的字，畫面讀來的草稿做不到；框清空就是送出去了，照沒有證據記。
        Ok(Proof::EchoRow | Proof::Unverified) => Ok(Proof::Unverified),
        Err(not) => Err(not_attempted_error(&run.id, not)),
    }
}

/// `path` 在 `offset` 之後多出來的使用者訊息（原文，CLI 包起來的貼上已還原）。
fn new_user_entries(format: LogFormat, path: &std::path::Path, offset: u64) -> std::io::Result<Vec<String>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    if f.metadata()?.len() < offset {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "session log shrank below the baseline"));
    }
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let body = String::from_utf8_lossy(&buf);
    Ok(body.lines().filter_map(|l| log_user_text(format, l)).map(|t| super::pasted_content::original(&t).into_owned()).collect())
}

/// Enter 按下去之後：等證據。框裡還是**同一段**就再按一次（跟一般送出一樣）；換了字就不按——那是別人剛打的。
/// 第二個值是 session log 裡那一則的原文（只有一則新的時候）：對話裡記它，不記畫面讀來的字。
#[allow(clippy::too_many_arguments)]
async fn confirm(
    app: &Arc<App>,
    client: &HerdrClient,
    run: &db::Run,
    bot: &db::Bot,
    pane: &str,
    proof: &Proof,
    offset: u64,
    draft: &str,
) -> anyhow::Result<(Delivered, Option<String>)> {
    let mut pressed_again = false;
    for _ in 0..SUBMIT_CHECKS {
        tokio::time::sleep(std::time::Duration::from_millis(SUBMIT_SETTLE_MS)).await;
        let screen = read_styled(client, pane, SCAN_SOURCE, DELIVER_SCAN_LINES).await?;
        if !same_session(app, &run.id, proof).await {
            return Ok((Delivered::Unproven("session_changed"), None));
        }
        match (box_state(&bot.kind, &screen), proof) {
            (BoxState::Empty, Proof::Transcript { format, path, .. }) => {
                let got = new_user_entries(*format, path, offset)?;
                if !got.is_empty() {
                    tracing::info!(run = %run.id, bot = %bot.name, "submitted the draft in the composer; the session log shows it");
                    let exact = (got.len() == 1).then(|| got[0].clone()).filter(|t| !t.trim().is_empty());
                    return Ok((Delivered::Submitted, exact));
                }
            }
            (BoxState::Empty, _) => {
                tracing::warn!(run = %run.id, bot = %bot.name, "submitted the draft in the composer; no lossless evidence on this run");
                return Ok((Delivered::Unverified, None));
            }
            (BoxState::NonEmpty, _) if !pressed_again && composer_text(&bot.kind, &screen).as_deref() == Some(draft) => {
                tracing::warn!(run = %run.id, bot = %bot.name, "the draft is still in the composer after Enter; pressing it again");
                client.pane_send_keys(pane, Submit::Enter.keys()).await?;
                pressed_again = true;
            }
            _ => {}
        }
    }
    let why = match box_state(&bot.kind, &read_styled(client, pane, SCAN_SOURCE, DELIVER_SCAN_LINES).await?) {
        BoxState::NonEmpty => "still_in_box",
        BoxState::Unready => "composer_unreadable",
        BoxState::Empty => "not_proven_submitted",
    };
    Ok((Delivered::Unproven(why), None))
}

/// 對話裡記 session log 的原文（畫面讀來的草稿可能是摺起來的 `[Pasted text …]`、少了空白）。
async fn record_exact_text(app: &Arc<App>, turn_id: &str, msg_id: &str, text: &str) -> anyhow::Result<()> {
    let mut tx = app.db.begin().await?;
    sqlx::query("UPDATE messages SET content = ? WHERE id = ?").bind(text).bind(msg_id).execute(&mut *tx).await?;
    sqlx::query("UPDATE turns SET prompt_text = ? WHERE id = ?").bind(text).bind(turn_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// `POST /bots/{id}/prompt` 帶 `submit_draft`：送出框裡那段。跟一般 prompt 同一套前提（冪等、維護窗口、回合在飛、
/// 接回未驗證、畫面上開著的選單、unknown 回合）；差別只在不打字、改按 Enter，訊息內容是框裡那段。
pub async fn submit(app: &Arc<App>, bot_id: &str, expect: &str, client_request_id: &str) -> LcResult<PromptOut> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    if let Err(e) = super::interruption::settle_locked(app, bot_id, super::interruption::Evidence::Nothing).await {
        tracing::warn!(bot = %bot_id, error = %e, "上一次打斷欠著的收尾還是寫不進去");
    }
    if let Err(e) = super::owed_delivery::settle_locked(app, bot_id).await {
        tracing::warn!(bot = %bot_id, error = %e, "欠著的送達結果還是寫不進去");
    }
    if client_request_id.trim().is_empty() {
        return Err(LcError::Bad("client_request_id must not be empty".into()));
    }
    if expect.trim().is_empty() {
        return Err(LcError::Bad("submit_draft needs the expect_draft the 409 showed".into()));
    }
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.filter(|b| b.deleted_at.is_none()).ok_or_else(|| LcError::NotFound("bot".into()))?;
    let conv = db::conversation_id(&app.db, bot_id).await.map_err(up)?;
    if let Some(t) = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE conversation_id=? AND client_request_id=?")
        .bind(&conv)
        .bind(client_request_id)
        .fetch_optional(&app.db)
        .await
        .map_err(up)?
    {
        return answer_for_turn(app, &t).await;
    }
    if let Some(refusal) = maintenance_refusal(app, Admission::Gated).await {
        return Err(refusal);
    }
    let run = db::active_run(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::conflict("bot has no active run", json!({})))?;
    if run.state != "running" {
        return Err(LcError::conflict("run is not running", json!({"run_id": run.id, "state": run.state})));
    }
    if run.agent_status == "blocked" {
        return Err(LcError::conflict("agent is blocked; answer the prompt first", json!({"run_id": run.id})));
    }
    if let Some(t) = db::in_flight_turn(&app.db, &run.id).await.map_err(up)? {
        return Err(LcError::conflict("a turn is already in flight", json!({"turn_id": t.id})));
    }
    if let super::resume_gate::Gate::Waiting { expected, left } = super::resume_gate::check(app, &bot, &run, &conv).await {
        return Err(LcError::conflict(
            "resume_unverified",
            json!({"run_id": run.id, "session_id": expected, "retry_after_s": left.as_secs().max(1)}),
        ));
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
    let client = client_for_run(app, &run).await?;
    let Some(pane) = pane_of(&run) else {
        return Err(not_attempted_error(&run.id, Delivered::NotAttempted { reason: "no_pane_to_type_into", retry: true }));
    };
    let (state, now) = read_draft(&client, &pane, &bot.kind).await.map_err(|_| unreadable(&run))?;
    match state {
        BoxState::Empty => return Err(refusal("draft_gone", &run, false, &bot.kind, None)),
        BoxState::Unready => return Err(unreadable(&run)),
        BoxState::NonEmpty => {}
    }
    let Some(draft) = now.filter(|d| same_draft(expect, d)) else {
        let fresh = read_draft(&client, &pane, &bot.kind).await.ok().and_then(|(_, d)| d);
        return Err(refusal("draft_changed", &run, true, &bot.kind, fresh.as_deref()));
    };
    let proof = draft_proof(app, &client, &run, &bot, &pane, &draft).await?;
    let offset = match &proof {
        Proof::Transcript { path, .. } => transcript_len(path)
            .map_err(|_| not_attempted_error(&run.id, Delivered::NotAttempted { reason: "transcript_unreadable", retry: true }))?,
        _ => 0,
    };

    // 回合＋使用者訊息在按鍵之前寫進 DB（跟一般 prompt 一樣，早到的 hook 才對得上）。`auto_resend=0`：框裡那段不是我們打的，
    // 不會有「再打一次」這回事。
    let turn_id = db::ulid();
    let msg_id = db::ulid();
    let mut tx = app.db.begin().await.map_err(up)?;
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, client_request_id, created_at, prompt_text, auto_resend)
         VALUES (?,?,?,'web','in_flight','pending',?,?,?,0)",
    )
    .bind(&turn_id)
    .bind(&conv)
    .bind(&run.id)
    .bind(client_request_id)
    .bind(db::now())
    .bind(&draft)
    .execute(&mut *tx)
    .await
    .map_err(up)?;
    sqlx::query("INSERT INTO messages (id, conversation_id, turn_id, role, content, source, created_at) VALUES (?,?,?,'user',?,'web',?)")
        .bind(&msg_id)
        .bind(&conv)
        .bind(&turn_id)
        .bind(&draft)
        .bind(db::now())
        .execute(&mut *tx)
        .await
        .map_err(up)?;
    tx.commit().await.map_err(up)?;
    emit_turn(app, &turn_id).await;

    // 打字前的那幾道閘門（維護窗口在 commit 之間被拿走、記不下「這個 pane 要打字」）：一個鍵都還沒按，撤回再回錯。
    let before_key = if let Some(refusal) = maintenance_refusal(app, Admission::Gated).await {
        Some(refusal)
    } else {
        crate::lifecycle::remember_pane_typed(&run.id);
        db::set_pane_typed(&app.db, &run.id)
            .await
            .err()
            .map(|_| not_attempted_error(&run.id, Delivered::NotAttempted { reason: "pane_typed_unwritable", retry: true }))
    };
    if let Some(refusal) = before_key {
        return match retract_unsent_turn(app, bot_id, &turn_id, &msg_id).await {
            Ok(Retraction::Withdrawn) => Err(refusal),
            Ok(Retraction::AlreadySettled) => answer_for_turn_id(app, &turn_id).await,
            Err(u) => Err(u.answer(&run.id, &turn_id, &msg_id, "the draft was not submitted and its turn could not be withdrawn")),
        };
    }

    #[cfg(test)]
    super::race_point::hit("draft_before_enter", bot_id).await;
    let res = match client.pane_send_keys(&pane, Submit::Enter.keys()).await {
        Ok(()) => confirm(app, &client, &run, &bot, &pane, &proof, offset, &draft).await,
        Err(e) => {
            // herdr 沒收下 Enter：框裡還是那一段＝沒送出去，撤回這一筆。看不出來才記成 unknown。
            let still = read_draft(&client, &pane, &bot.kind).await.ok().and_then(|(_, d)| d);
            if still.as_deref() == Some(draft.as_str()) {
                return match retract_unsent_turn(app, bot_id, &turn_id, &msg_id).await {
                    Ok(Retraction::Withdrawn) => Err(LcError::Upstream(format!("herdr refused the Enter key; the draft is still in the composer: {e}"))),
                    Ok(Retraction::AlreadySettled) => answer_for_turn_id(app, &turn_id).await,
                    Err(u) => Err(u.answer(&run.id, &turn_id, &msg_id, "the draft was not submitted and its turn could not be withdrawn")),
                };
            }
            Err(e)
        }
    };
    let (delivery, rec) = match res {
        Ok((d @ (Delivered::Submitted | Delivered::Unverified), exact)) => {
            if let Some(text) = exact.filter(|t| *t != draft) {
                if let Err(e) = record_exact_text(app, &turn_id, &msg_id, &text).await {
                    tracing::warn!(turn = %turn_id, error = %e, "could not replace the screen-read draft with the session log's text");
                }
            }
            (if matches!(d, Delivered::Submitted) { "ok" } else { "unverified" }, d.record().expect("delivered outcome records"))
        }
        Ok((other, _)) => {
            tracing::warn!(bot = %bot_id, outcome = ?other, "submitting the draft could not be proven");
            ("unknown", DeliveryRecord { stored: "unknown", verified: false, auto_resend: false })
        }
        Err(e) => {
            tracing::warn!(bot = %bot_id, error = %e, "submitting the draft: result unknown");
            ("unknown", DeliveryRecord { stored: "unknown", verified: false, auto_resend: false })
        }
    };
    // 內容定稿（可能換成了 session log 的原文）才推訊息：`message_added` 以 id 去重，推了就改不回來。
    emit_prompt_message(app, bot_id, &msg_id).await;
    let written = super::owed_delivery::delivered(app, bot_id, &turn_id, rec).await;
    if delivery == "ok" || delivery == "unverified" {
        arm_stall(app, &run.id, bot_id, &turn_id).await;
        arm_progress(app, &run.id, bot_id, &turn_id).await;
    }
    if let Err(e) = written {
        return Err(super::owed_delivery::uncommitted(Some(&run.id), &turn_id, &msg_id, delivery, Some(&e)));
    }
    Ok(PromptOut { turn_id, message_id: msg_id, delivery: delivery.into(), send_now: None })
}

#[cfg(test)]
#[path = "composer_draft_tests.rs"]
mod tests;
