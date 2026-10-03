//! 接受 claude 的「建議下一句」（prompt suggestion，`POST /bots/{id}/suggestion/accept`，2026-10-03 使用者：
//! 「要一模一樣 tab + enter 送出」）。終端裡 Tab 把灰字收進輸入框、Enter 送出；這裡**對 pane 送的就是那兩個鍵**，不是把字打進去。
//!
//! 全程握著 bot 鎖，每一步都對不上就停：
//! 1. **按 Tab 之前**（一個鍵都不按）：過一般送出的閘門（[`composer_draft::submit_gates`]：維護窗口、回合在飛、對話框…），
//!    重讀樣式畫面，建議還在、而且跟網頁帶來的字完全相同才往下；否則 409 `suggestion_gone`／`suggestion_changed`。
//! 2. **Tab**，等 TUI 重畫，重讀：框裡要變成那一句（真的文字，不再是灰字；比對去掉空白，窄 pane 折行的空白位置不可靠）。
//! 3. **Enter**：直接走「送出框裡那段」的既有流程（[`composer_draft::submit_locked`]）——回合＋使用者訊息在按鍵前寫進 DB
//!    （origin `web`、source `web`，跟網頁送出的 prompt 一樣；對話內容換成 session log 的原文）、證明送達、掛 stall／progress，
//!    所以不會變成「外部回合」，也不會記兩則。
//!
//! Tab 之後對不上時的還原（Tab 已經把那一句放進框裡了，鍵不能收回）：
//! * 框裡**還是**那一句（只是後面的閘門擋下、回合已撤回、一個字都沒送出）→ 補一個 `ctrl+c` 清框（[`composer_draft::clear`]，
//!   它自己再驗一次框裡就是那一句、沒有回合在跑，才按），讓 bot 回到乾淨的空框，不留一段會把之後每則 prompt 擋成 `composer_busy` 的草稿。
//!   代價：CLI 那句灰字建議沒了（下一回合結束才會再有）。
//! * 框裡是**別的字**（使用者剛好在終端打字）→ 一個鍵都不按，409 `draft_changed` 帶那段草稿與 token，網頁照既有的草稿流程讓人處理。
//! * Tab 沒生效（框還是空的／灰字）→ 沒有東西要還原，409 `tab_not_accepted`。
//! * Tab 之後讀不到畫面 → 409 `composer_unreadable`，不再按任何鍵。

use super::*;

/// Tab 之後等 TUI 重畫多久再讀。
const TAB_SETTLE_MS: u64 = 700;

fn squash(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// 409 的共同欄位：這次沒送出去（`sent: false`）、按過 Tab 沒有（`tab_sent`）。
fn refused(reason: &str, run: &db::Run, tab_sent: bool, extra: Value) -> LcError {
    let mut body = json!({"run_id": run.id, "retryable": true, "sent": false, "tab_sent": tab_sent});
    if let (Some(o), Some(e)) = (body.as_object_mut(), extra.as_object()) {
        o.extend(e.clone());
    }
    LcError::conflict(reason, body)
}

/// 現在畫面上的建議（帶樣式的讀）；`None` 也包含框裡是別的字。
fn suggestion_on(screen: &str) -> Option<String> {
    prompt_suggestion("claude", screen)
}

/// `expect_text`＝網頁看到的那句；`expect_run_id`＝網頁看到它的那個 run。
pub async fn accept(
    app: &Arc<App>,
    bot_id: &str,
    expect_text: &str,
    expect_run_id: Option<&str>,
    client_request_id: &str,
) -> LcResult<PromptOut> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let expect = expect_text.trim();
    if expect.is_empty() {
        return Err(LcError::Bad("suggestion must not be empty".into()));
    }
    if client_request_id.trim().is_empty() {
        return Err(LcError::Bad("client_request_id must not be empty".into()));
    }
    // 跟 `submit` 一樣：欠著的收尾先補，不然那筆已經停掉、只是沒寫成的回合會把這一次擋成「回合在飛」。
    if let Err(e) = super::interruption::settle_locked(app, bot_id, super::interruption::Evidence::Nothing).await {
        tracing::warn!(bot = %bot_id, error = %e, "上一次打斷欠著的收尾還是寫不進去");
    }
    if let Err(e) = super::owed_delivery::settle_locked(app, bot_id).await {
        tracing::warn!(bot = %bot_id, error = %e, "欠著的送達結果還是寫不進去");
    }
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.filter(|b| b.deleted_at.is_none()).ok_or_else(|| LcError::NotFound("bot".into()))?;
    if bot.kind != "claude" {
        return Err(LcError::conflict("suggestion_unsupported", json!({"kind": bot.kind, "sent": false})));
    }
    let conv = db::conversation_id(&app.db, bot_id).await.map_err(up)?;
    // 同一個 client_request_id 重送：回原本那一筆（網頁逾時重試不會多送一次）。
    if let Some(t) = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE conversation_id=? AND client_request_id=?")
        .bind(&conv)
        .bind(client_request_id)
        .fetch_optional(&app.db)
        .await
        .map_err(up)?
    {
        return answer_for_turn(app, &t).await;
    }
    // ---- 按 Tab 之前：全是看、一個鍵都不按 ----
    let run = composer_draft::submit_gates(app, bot_id, &bot, &conv).await?;
    if expect_run_id.is_some_and(|id| id != run.id) {
        return Err(LcError::conflict("run mismatch", json!({"run_id": run.id, "sent": false})));
    }
    let client = client_for_run(app, &run).await?;
    let Some(pane) = composer_draft::pane_of(&run) else {
        return Err(not_attempted_error(&run.id, Delivered::NotAttempted { reason: "no_pane_to_type_into", retry: true }));
    };
    let screen = read_styled_snapshot(&client, &pane, SCAN_SOURCE, DELIVER_SCAN_LINES)
        .await
        .map_err(|_| composer_draft::unreadable(&run))?
        .text;
    match suggestion_on(&screen) {
        Some(now) if squash(&now) == squash(expect) => {}
        Some(now) => {
            // 畫面上的建議換了：記下新的、讓網頁換字，這次不送。
            if crate::prompt_suggestion::set(&run.id, Some(now.clone())) {
                app.emit_bot_status(bot_id).await;
            }
            return Err(refused("suggestion_changed", &run, false, json!({"suggestion": now})));
        }
        None => {
            if crate::prompt_suggestion::forget(&run.id) {
                app.emit_bot_status(bot_id).await;
            }
            // 框裡若是使用者打的字，一併給草稿與動作，網頁才有東西讓人處理。
            let draft = (box_state("claude", &screen) == BoxState::NonEmpty).then(|| composer_text_whole("claude", &screen)).flatten();
            let mut err = composer_draft::refusal("suggestion_gone", &run, true, "claude", draft.as_deref());
            if let LcError::Conflict(o) = &mut err {
                o["tab_sent"] = json!(false);
            }
            return Err(err);
        }
    }
    #[cfg(test)]
    super::race_point::hit("suggestion_before_tab", bot_id).await;

    // ---- Tab：把灰字收進輸入框 ----
    if let Err(e) = client.pane_send_keys(&pane, &["tab"]).await {
        tracing::warn!(bot = %bot_id, error = %e, "herdr refused the Tab key; nothing was accepted");
        return Err(LcError::Upstream(format!("herdr refused the Tab key: {e}")));
    }
    // 之後每一種收法都要把頁面上的建議忘掉：Tab 不管成不成，那句灰字都不在原位了。
    if crate::prompt_suggestion::forget(&run.id) {
        app.emit_bot_status(bot_id).await;
    }
    let mut taken = None;
    for attempt in 0..2 {
        tokio::time::sleep(std::time::Duration::from_millis(TAB_SETTLE_MS)).await;
        let (state, text, _) = match composer_draft::read_draft(&client, &pane, "claude").await {
            Ok(r) => r,
            Err(_) => return Err(refused("composer_unreadable", &run, true, json!({"detail": "Tab 已經按了，之後讀不到畫面；沒有再按任何鍵"}))),
        };
        match state {
            BoxState::Unready => return Err(refused("composer_unreadable", &run, true, json!({"detail": "Tab 已經按了，輸入框讀不出來；沒有再按任何鍵"}))),
            // 框還是空的（或灰字還在）：TUI 可能還沒重畫，再等一次；兩次都空就是 Tab 沒生效。
            BoxState::Empty if attempt == 0 => continue,
            BoxState::Empty => return Err(refused("tab_not_accepted", &run, true, json!({"detail": "按了 Tab，輸入框沒有收下那一句；沒有再按任何鍵"}))),
            BoxState::NonEmpty => {
                taken = text;
                break;
            }
        }
    }
    let Some(draft) = taken.filter(|d| squash(d) == squash(expect)) else {
        // 框裡是別的字（使用者剛在終端打字、或 Tab 做了別的事）：不認得、不碰，照草稿流程讓人處理。
        let now = composer_draft::read_draft(&client, &pane, "claude").await.ok().and_then(|(_, d, _)| d);
        let mut err = composer_draft::refusal("draft_changed", &run, true, "claude", now.as_deref());
        if let LcError::Conflict(o) = &mut err {
            o["tab_sent"] = json!(true);
        }
        return Err(err);
    };

    // ---- Enter：框裡就是那一句，走「送出框裡那段」的既有流程 ----
    let token = composer_draft::draft_token(&run.id, &pane, &draft);
    match composer_draft::submit_locked(app, bot_id, &token, client_request_id).await {
        Ok(out) => Ok(out),
        Err(LcError::Conflict(mut body)) if body.get("sent").and_then(Value::as_bool) != Some(true) => {
            // 閘門在 Tab 之後才擋下：一個字都沒送出，那一句還在框裡。清掉還原成乾淨的空框（`clear` 自己再驗一次框裡就是這一句）。
            let restored = match composer_draft::clear(app, &client, &run, &bot, &token, false).await {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!(bot = %bot_id, error = ?e, "accepted the suggestion with Tab but could not send it, and could not clear it either");
                    false
                }
            };
            if let Some(o) = body.as_object_mut() {
                o.insert("tab_sent".into(), json!(true));
                o.insert("suggestion_restored".into(), json!(restored));
            }
            Err(LcError::Conflict(body))
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
#[path = "suggestion_tests.rs"]
mod tests;
