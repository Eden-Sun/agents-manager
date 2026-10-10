//! `rewind` API handler 與 runner。

use std::sync::Arc;
use axum::extract::{Path, State};
use axum::Json;
use serde_json::{json, Value};

use crate::db;
use crate::lc_error::{LcError, LcResult};
use crate::rewind::{
    anchor, drive, in_rewind_ui, keys, read, rewind_dropped, same_composer, same_first_line,
    wait_for, Fail, HerdrPane, Pane, RewindIn, CLEAR_WAIT_MS, CTRL_C_HINT, HINT_WAIT_MS,
};
use crate::state::App;

fn up<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(e.to_string())
}

fn conflict(reason: &str, message: &str) -> LcError {
    LcError::conflict(reason, json!({"message": message}))
}

fn preview(s: &str, n: usize) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > n {
        format!("{}…", one.chars().take(n).collect::<String>())
    } else {
        one
    }
}

/// `POST /api/bots/{id}/rewind`
pub async fn post_rewind(State(app): State<Arc<App>>, Path(bot_id): Path<String>, Json(b): Json<RewindIn>) -> LcResult<Json<Value>> {
    rewind_with(&app, &bot_id, &b.message_id, None, b.clear_composer.then(|| b.expect_composer.unwrap_or_default())).await.map(Json)
}

/// `pane`：測試注入的假 pane；`None`＝這個 run 的真 pane。
#[cfg(test)]
pub async fn rewind(app: &Arc<App>, bot_id: &str, message_id: &str, pane: Option<Arc<dyn Pane>>) -> LcResult<Value> {
    rewind_with(app, bot_id, message_id, pane, None).await
}

/// 清掉終端輸入列裡那段（使用者在確認框看過、`expect` 是那段字）。逐字比對，只正規化 CRLF/LF 換行；有任何其他差異就不動，回 `composer_changed`。
async fn clear_composer(pane: &dyn Pane, expect: &str) -> LcResult<()> {
    let s = read(pane).await.map_err(|f| up(f.message()))?;
    if in_rewind_ui(&s) {
        return Err(conflict(Fail::UiBusy.reason(), &Fail::UiBusy.message()));
    }
    let Some(text) = crate::composer_parse::composer_text_whole("claude", &s) else { return Ok(()) };
    if !same_composer(&text, expect) {
        return Err(LcError::conflict(
            "composer_changed",
            json!({"message": "終端輸入列裡的字跟剛才不一樣了，沒有動它；再按一次倒回看看現在是什麼。", "draft": text}),
        ));
    }
    keys(pane, &["ctrl+c"]).await.map_err(|f| up(f.message()))?;
    let cleared = wait_for(pane, CLEAR_WAIT_MS, |s| crate::composer_parse::composer_text_whole("claude", s).is_none().then_some(())).await.map_err(|f| up(f.message()))?;
    if cleared.is_none() {
        return Err(conflict(Fail::ComposerBusy.reason(), "按了 ctrl+c，終端輸入列還是有字，沒有倒回。"));
    }
    let _ = wait_for(pane, HINT_WAIT_MS, |s| (!s.contains(CTRL_C_HINT)).then_some(())).await;
    Ok(())
}

/// `clear`：先清掉輸入列裡這段再倒回（`None`＝不清）。
pub async fn rewind_with(app: &Arc<App>, bot_id: &str, message_id: &str, pane: Option<Arc<dyn Pane>>, clear: Option<String>) -> LcResult<Value> {
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.filter(|b| b.deleted_at.is_none()).ok_or_else(|| LcError::NotFound("bot".into()))?;
    if bot.kind != "claude" {
        return Err(conflict("unsupported_kind", "只有 claude 能倒回：codex／grok 沒有對應的 /rewind。"));
    }
    // 使用者自己 default session 的 pane 只觀察、不代打（SPEC §6.5.1）。
    crate::default_session::refuse_default_session(&bot)?;
    let conv = db::conversation_id(&app.db, bot_id).await.map_err(up)?;

    // 持 bot 鎖到打完字：`prompt` 拿同一把，期間不會有新的 prompt 打進這個 pane。
    // already_rewound 與 skip 都在鎖裡重讀。鎖外算過的話，第二次請求會在第一次標記之前
    // 就決定要倒，進鎖後照樣再按一次 Restore（#656）。
    #[cfg(test)]
    crate::race_point::hit("rewind_before_lock", bot_id).await;
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    #[cfg(test)]
    crate::race_point::hit("rewind_locked", bot_id).await;
    let msg: db::Message = sqlx::query_as("SELECT *, rowid AS seq FROM messages WHERE id = ? AND conversation_id = ?")
        .bind(message_id)
        .bind(&conv)
        .fetch_optional(&app.db)
        .await
        .map_err(up)?
        .ok_or_else(|| LcError::NotFound("message".into()))?;
    if msg.role != "user" {
        return Err(conflict("not_a_user_message", "只能倒回到一則使用者訊息。"));
    }
    if msg.rewound_at.is_some() {
        return Err(conflict("already_rewound", "這一則已經倒回掉了。"));
    }
    // 送出的字：排隊的網頁 prompt 記在 turn 上（跟畫面上的原文可能不一樣）。回合中補充的那句不是 turn 的 prompt，用它自己的原文。
    let prompt_text: Option<String> = match msg.turn_id.as_deref().filter(|_| msg.sent_via.as_deref() != Some("supplement")) {
        Some(t) => sqlx::query_scalar("SELECT prompt_text FROM turns WHERE id = ?").bind(t).fetch_optional(&app.db).await.map_err(up)?.flatten(),
        None => None,
    };
    let target = prompt_text.filter(|t| !t.trim().is_empty()).unwrap_or_else(|| msg.content.clone());
    // 同一行開頭的較新訊息（沒被倒掉的）要在選單上跳過幾則。排隊中被撤掉、從沒打進 pane 的不算（#1122）：選單上沒有它。
    let later: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT content, turn_id FROM messages WHERE conversation_id = ? AND role = 'user' AND rewound_at IS NULL
           AND rowid > (SELECT rowid FROM messages WHERE id = ?) ORDER BY rowid",
    )
    .bind(&conv)
    .bind(&msg.id)
    .fetch_all(&app.db)
    .await
    .map_err(up)?;
    let mut skip = 0;
    for (c, t) in &later {
        if !same_first_line(c, &target) {
            continue;
        }
        if let Some(t) = t {
            if never_delivered(app, t).await.map_err(up)? {
                continue;
            }
        }
        skip += 1;
    }

    let run = db::active_run(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| conflict("not_running", "bot 沒在跑。"))?;
    if let Some(why) = busy_reason(app, bot_id, &run).await? {
        return Err(LcError::conflict("not_idle", json!({"busy": why, "message": "它正在忙，等這一回合結束再倒回。"})));
    }
    let pane: Arc<dyn Pane> = match pane {
        Some(p) => p,
        None => {
            let pane_id = run.pane_id.clone().filter(|p| !p.trim().is_empty()).ok_or_else(|| conflict("no_pane", "這個 run 沒有 pane。"))?;
            let client = app.herdr_for_run(&run).await.ok_or_else(|| up(format!("no Herdr session is available for run `{}`", run.id)))?;
            Arc::new(HerdrPane { client, pane_id })
        }
    };
    // 直接對 pane 打過字：之後的 prompt 改走打字路線（`slash::mark_pane_typed` 的理由）。記不下來就不打。
    crate::app_ports_r2a8::mark_pane_typed(app, &run.id).await.map_err(up)?;
    if let Some(expect) = clear.as_deref() {
        clear_composer(pane.as_ref(), expect).await?;
    }
    let dropped = match msg.turn_id.as_deref() {
        Some(t) => turn_dropped(app, t).await.map_err(up)?,
        None => false,
    };
    // 真的按了 Restore 的那一則（被丟掉的那種會改倒到下一則，或根本不按）：下一次 resume 要接到它之前（`anchor`）。
    let mut restored = Some((target.clone(), skip));
    let done = match drive(pane.as_ref(), &target, skip).await {
        Ok(d) => d,
        Err(Fail::NotInMenu | Fail::ComposerBusy) if dropped => {
            let next = next_in_context(app, &conv, &msg.id).await.map_err(up)?;
            restored = next.clone();
            match rewind_dropped(pane.as_ref(), &target, next).await {
                Ok(d) => {
                    tracing::info!(bot = %bot.name, "rewind: the target was interrupted before any output and never stayed in the context");
                    d
                }
                Err(f) => return Err(rewind_failed(&bot.name, pane.as_ref(), f).await),
            }
        }
        Err(f) => return Err(rewind_failed(&bot.name, pane.as_ref(), f).await),
    };

    let now = db::now();
    let hidden = mark_rewound(app, &conv, &msg.id, &now).await.map_err(|e| {
        LcError::uncommitted("rewind_marks_uncommitted", &run.id, "已經倒回了，但對話紀錄沒標記成功；重新整理後被倒掉的訊息可能還顯示著", e)
    })?;
    let note = format!("已倒回到這則之前：「{}」。之後的 {hidden} 則不在對話脈絡裡了（紀錄保留）。", preview(&msg.content, 40));
    let _ = crate::app_ports_r2a8::insert_message(app, &conv, None, "system", &note, "system", false, None).await;
    app.emit("messages_rewound", json!({"bot_id": bot_id, "message_id": msg.id, "rewound_at": now})).await;
    tracing::info!(bot = %bot.name, hidden, pane_cleared = done.pane_cleared, "rewound the conversation");
    // 還握著鎖：下一則還沒打進去，transcript 的長度就是「倒回當時」。
    if let Some((text, skip)) = &restored {
        anchor::record(app, &bot, &run, text, *skip).await;
    }
    Ok(json!({
        "rewound": true,
        "message_id": msg.id,
        "text": msg.content,
        "hidden": hidden,
        "pane_cleared": done.pane_cleared,
    }))
}

/// 回合失敗、而且一則 assistant 回覆都沒有：claude 沒留下它（見 [`rewind_dropped`]）。
/// 排隊中被撤掉、一個字都沒打進 pane 的那一則（`retract_queued`：`status='failed'`、`delivery='failed'`）：
/// CLI 從來沒看過它，`/rewind` 選單上不會有，不能算進要跳過的則數（#1122）。
async fn never_delivered(app: &impl crate::capabilities::Db, turn_id: &str) -> anyhow::Result<bool> {
    let row: Option<(String, String)> = sqlx::query_as("SELECT status, delivery FROM turns WHERE id = ?").bind(turn_id).fetch_optional(app.db()).await?;
    Ok(matches!(row, Some((s, d)) if s == "failed" && d == "failed"))
}

async fn turn_dropped(app: &impl crate::capabilities::Db, turn_id: &str) -> anyhow::Result<bool> {
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM turns WHERE id = ?").bind(turn_id).fetch_optional(app.db()).await?;
    if status.as_deref() != Some("failed") {
        return Ok(false);
    }
    let replies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id = ? AND role = 'assistant'")
        .bind(turn_id)
        .fetch_one(app.db())
        .await?;
    Ok(replies == 0)
}

/// `message_id` 之後第一則還在 context 裡的使用者訊息（沒倒回、不是被丟掉的），連同它在選單上要跳過幾則同樣開頭的較新訊息。
async fn next_in_context(app: &impl crate::capabilities::Db, conv: &str, message_id: &str) -> anyhow::Result<Option<(String, usize)>> {
    let later: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT content, turn_id FROM messages WHERE conversation_id = ? AND role = 'user' AND rewound_at IS NULL
           AND rowid > (SELECT rowid FROM messages WHERE id = ?) ORDER BY rowid",
    )
    .bind(conv)
    .bind(message_id)
    .fetch_all(app.db())
    .await?;
    for (i, (content, turn)) in later.iter().enumerate() {
        if let Some(t) = turn {
            if turn_dropped(app, t).await? {
                continue;
            }
        }
        // 跳過的只算「真的打進過 pane」的同開頭訊息（#1122）。
        let mut skip = 0;
        for (c, t) in &later[i + 1..] {
            if !same_first_line(c, content) {
                continue;
            }
            if let Some(t) = t {
                if never_delivered(app, t).await? {
                    continue;
                }
            }
            skip += 1;
        }
        return Ok(Some((content.clone(), skip)));
    }
    Ok(None)
}

/// 倒回沒做成：記 log、轉成 API 錯誤。輸入列有字時把那段字帶回去（`draft`），網頁才能讓人看過再選「清掉再倒回」。
async fn rewind_failed(bot: &str, pane: &dyn Pane, f: Fail) -> LcError {
    let draft = if f == Fail::ComposerBusy {
        read(pane).await.ok().and_then(|s| crate::composer_parse::composer_text_whole("claude", &s))
    } else {
        None
    };
    tracing::warn!(
        bot,
        reason = f.reason(),
        composer_present = draft.is_some(),
        composer_chars = draft.as_deref().map_or(0, |d| d.chars().count()),
        "rewind did not happen"
    );
    match f {
        Fail::Pane(_) => up(f.message()),
        _ => LcError::conflict(f.reason(), json!({"message": f.message(), "draft": draft})),
    }
}

/// 這一則與之後的都標成倒回（標記不刪）。回標了幾則。
async fn mark_rewound(app: &impl crate::capabilities::Db, conv: &str, message_id: &str, now: &str) -> anyhow::Result<u64> {
    Ok(sqlx::query(
        "UPDATE messages SET rewound_at = ?
          WHERE conversation_id = ? AND rewound_at IS NULL
            AND rowid >= (SELECT rowid FROM messages WHERE id = ?)",
    )
    .bind(now)
    .bind(conv)
    .bind(message_id)
    .execute(app.db())
    .await?
    .rows_affected())
}

/// 鎖裡查：要閒著。排著的也算忙：鎖一放就會送進倒回後的對話，那不一定是使用者要的。
async fn busy_reason(app: &impl crate::capabilities::Db, bot_id: &str, run: &db::Run) -> LcResult<Option<&'static str>> {
    Ok(if run.state != "running" {
        Some("not_running")
    } else if run.agent_status != "idle" {
        Some(match run.agent_status.as_str() {
            "working" => "working",
            "blocked" => "blocked",
            _ => "unknown_status",
        })
    } else if db::in_flight_turn(app.db(), &run.id).await.map_err(up)?.is_some() {
        Some("turn_in_flight")
    } else if db::queued_turn_for_bot(app.db(), bot_id).await.map_err(up)?.is_some() {
        Some("queued_turn")
    } else {
        None
    })
}
