//! codex 的 `send_now`（issue #748）：把字打進正在忙的 codex TUI，由 codex 0.159 的 `instant_interrupt` 把它 steer 進進行中的回合。
//!
//! 跟 claude 的插隊（`send_now::deliver`）語意不同，不能硬套：
//! - claude：按 send-now 鍵打斷舊回合 → 舊回合收成 `failed`、新的字開**新回合**。
//! - codex：沒有任何鍵要按，字照一般 Enter 送出，由 codex 自己決定怎麼併進**同一個**進行中的回合。
//!   所以**不收舊回合、不建新回合**，那一句記成這個回合的補充（`sent_via = 'supplement'`，§6.3 第 10 點），
//!   回應 `send_now: "steered"` 讓 UI／呼叫端分得出這不是 claude 式的「被打斷」。
//!
//! 這是**預設關**的 canary（`[codex] instant_interrupt`）：規格寫的「working 中送進去的字會被立刻讀取、rollout 只出現一次」
//! 只能在真機驗，驗過之前不開。送達證據沿用一般打字那條（rollout 計數）；證不出來就誠實回 `unknown`，不假裝 steer 成功。

use super::*;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// 已經打進 pane 的 steer，以 (bot, client_request_id) 為鍵。steer 沒有自己的 turn 列可以拿來做冪等
/// （一般 prompt 靠 `turns.client_request_id`），所以同一個 request id 重送（呼叫端逾時重試、DB 一時寫不進去之後再試）
/// 靠 `codex_steers` 認出「字已經在 pane 裡」，**不再打第二次**（#916）。真相在 DB，daemon 重啟後照舊；
/// 這份記憶體只是快取（滿了淘汰一筆，不整張清空）。
#[derive(Clone)]
struct Steered {
    text: String,
    turn_id: String,
    delivery: &'static str,
    message_id: Option<String>,
}

/// 快取的上限：不是帳本（帳在 `codex_steers`），滿了淘汰一筆，不會連別顆 bot 還沒補記的 steer 一起丟。
const MEMO_CAP: usize = 1024;

/// `codex_steers` 的列留多久（同一個 request id 的重送只發生在這之內）。
const KEEP_STEERS_SECS: i64 = 7 * 24 * 3600;

/// steer 的持久帳（#916）：`PRIMARY KEY (bot_id, client_request_id)`。先寫 `delivery='pending'` 再打字，
/// 打完改成送達結果，補記訊息成功後填 `message_id`。`pending` 的列重送時視為 `unknown`（不知道打進去沒有，寧可少打不要重複打）。
pub async fn migrate(pool: &sqlx::SqlitePool) -> anyhow::Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS codex_steers (
           bot_id TEXT NOT NULL,
           client_request_id TEXT NOT NULL,
           turn_id TEXT NOT NULL,
           text_hash TEXT NOT NULL,
           delivery TEXT NOT NULL,
           message_id TEXT,
           created_at TEXT NOT NULL,
           PRIMARY KEY (bot_id, client_request_id)
         )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// 測試用：把這個 (bot, crid) 從記憶體快取拿掉，模擬 daemon 重啟後只剩 DB 的帳。
#[cfg(all(test, feature = "daemon-test-harness"))]
pub fn forget_memo_for_test(bot_id: &str, client_request_id: &str) {
    memo().lock().unwrap().remove(&(bot_id.to_string(), client_request_id.to_string()));
}

fn text_hash(text: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn remember(key: &(String, String), state: &Steered) {
    let mut m = memo().lock().unwrap();
    if m.len() >= MEMO_CAP && !m.contains_key(key) {
        if let Some(victim) = m.keys().next().cloned() {
            m.remove(&victim);
        }
    }
    m.insert(key.clone(), state.clone());
}

/// DB 裡這個 (bot, crid) 的帳。字不同（hash 對不上）的由呼叫端回 409。
async fn load(db: &sqlx::SqlitePool, key: &(String, String)) -> Result<Option<(String, String, String, Option<String>)>, sqlx::Error> {
    sqlx::query_as("SELECT turn_id, text_hash, delivery, message_id FROM codex_steers WHERE bot_id = ? AND client_request_id = ?")
        .bind(&key.0)
        .bind(&key.1)
        .fetch_optional(db)
        .await
}

fn delivery_of(stored: &str) -> &'static str {
    match stored {
        "ok" => "ok",
        "unverified" => "unverified",
        _ => "unknown",
    }
}

fn memo() -> &'static Mutex<HashMap<(String, String), Steered>> {
    static M: OnceLock<Mutex<HashMap<(String, String), Steered>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

/// 同一把 bot 鎖裡呼叫（`prompt_inner` 已持有）。`turn` 是被 steer 的進行中回合。
#[allow(clippy::too_many_arguments)]
pub async fn steer(
    app: &impl super::s6_ports::CodexSteerContext,
    client: &RunClient,
    run: &db::Run,
    bot: &db::Bot,
    turn: &db::Turn,
    text: &str,
    deliver: &str,
    client_request_id: &str,
) -> LcResult<PromptOut> {
    let key = (bot.id.clone(), client_request_id.to_string());
    // 順手清掉過期的帳（steer 很少發生，不需要另外的巡邏）；失敗不影響這次 steer。
    if let Err(e) = sqlx::query("DELETE FROM codex_steers WHERE created_at < ?").bind(db::iso_in(-KEEP_STEERS_SECS)).execute(app.db()).await {
        tracing::debug!(error = %e, "codex_steers prune failed");
    }
    let mut prior = memo().lock().unwrap().get(&key).cloned();
    if prior.is_none() {
        // 記憶體沒有（daemon 重啟、被淘汰）：以 DB 為準。
        if let Some((turn_id, hash, delivery, message_id)) = load(app.db(), &key).await.map_err(up)? {
            if hash != text_hash(text) {
                return Err(LcError::conflict("client_request_id was already used for a different prompt", json!({"turn_id": turn_id})));
            }
            prior = Some(Steered { text: text.to_string(), turn_id, delivery: delivery_of(&delivery), message_id });
        }
    }
    if let Some(prior) = prior {
        if prior.text != text {
            return Err(LcError::conflict("client_request_id was already used for a different prompt", json!({"turn_id": prior.turn_id})));
        }
        // 字已經在 pane 裡：只補還沒寫成的那則訊息，絕不再打一次。
        return finish(app, bot, key, prior).await;
    }
    // 一般送出的同一套圍籬：框裡有人的草稿就不打（`composer_busy`），讀不到框就不打。
    let plan = match app.plan_delivery(client, run, bot, deliver, true, false).await.map_err(up)? {
        Ok(plan) => plan,
        Err(not) => return Err(super::composer_draft::with_draft(client, run, bot, not_attempted_error(&run.id, not)).await),
    };
    // 先記帳再打字：打到一半 daemon 掛了，重送看得到這一列、不會再打第二次（#916）。寫不進去就不打。
    sqlx::query("INSERT INTO codex_steers (bot_id, client_request_id, turn_id, text_hash, delivery, message_id, created_at) VALUES (?,?,?,?,'pending',NULL,?)")
        .bind(&key.0)
        .bind(&key.1)
        .bind(&turn.id)
        .bind(text_hash(text))
        .bind(db::now())
        .execute(app.db())
        .await
        .map_err(up)?;
    let delivery = match app.execute_delivery(client, run, bot, deliver, plan).await {
        Ok(Delivered::Submitted) => "ok",
        Ok(Delivered::Handed | Delivered::Unverified) => "unverified",
        Ok(Delivered::Unproven(why)) => {
            tracing::warn!(bot = %bot.name, reason = why, "codex steer 送出了，但證不出來");
            "unknown"
        }
        // 一個字都沒進 pane：可重試的 409，不留任何紀錄。
        Ok(not @ Delivered::NotAttempted { .. }) => {
            forget_pending(app, &key).await;
            return Err(super::composer_draft::with_draft(client, run, bot, not_attempted_error(&run.id, not)).await);
        }
        Err(e) if crate::herdr::never_applied(&e) => {
            forget_pending(app, &key).await;
            return Err(not_attempted_error(&run.id, Delivered::NotAttempted { reason: "pane_send_refused", retry: true }));
        }
        Err(e) => {
            tracing::warn!(bot = %bot.name, error = %e, "codex steer 打字沒有回應，不知道送出了沒有");
            "unknown"
        }
    };
    if let Err(e) = sqlx::query("UPDATE codex_steers SET delivery = ? WHERE bot_id = ? AND client_request_id = ?").bind(delivery).bind(&key.0).bind(&key.1).execute(app.db()).await {
        tracing::warn!(bot = %bot.name, error = %e, "codex steer 送達結果沒寫進帳，重送時會當成 unknown");
    }
    let state = Steered { text: text.to_string(), turn_id: turn.id.clone(), delivery, message_id: None };
    remember(&key, &state);
    finish(app, bot, key, state).await
}

/// 一個字都沒進 pane：把先寫的 pending 帳撤掉，同一個 request id 才能原樣重送。
async fn forget_pending(app: &impl super::s6_ports::CodexSteerContext, key: &(String, String)) {
    if let Err(e) = sqlx::query("DELETE FROM codex_steers WHERE bot_id = ? AND client_request_id = ? AND delivery = 'pending'").bind(&key.0).bind(&key.1).execute(app.db()).await {
        tracing::warn!(error = %e, "could not drop the pending codex steer record");
    }
}

/// 字已經打進去了：把它記成進行中回合的補充訊息（補過的不重記）。寫不進去回 503、記憶體帳留著，同一個 request id 重送只重寫訊息。
async fn finish(app: &impl super::s6_ports::CodexSteerContext, bot: &db::Bot, key: (String, String), mut state: Steered) -> LcResult<PromptOut> {
    if state.message_id.is_none() {
        let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id = ?").bind(&state.turn_id).fetch_optional(app.db()).await;
        let inserted = match turn {
            Ok(Some(t)) => super::slash::insert_supplement(app, &bot.id, &t, &state.text).await,
            Ok(None) => Err(anyhow::anyhow!("the steered turn is gone")),
            Err(e) => Err(e.into()),
        };
        match inserted {
            Ok(m) => {
                state.message_id = Some(m.id.clone());
                if let Err(e) = sqlx::query("UPDATE codex_steers SET message_id = ? WHERE bot_id = ? AND client_request_id = ?").bind(&m.id).bind(&key.0).bind(&key.1).execute(app.db()).await {
                    tracing::warn!(error = %e, "codex steer 的訊息沒記進帳，重送可能多補記一則");
                }
                remember(&key, &state);
            }
            Err(e) => {
                return Err(LcError::Uncommitted(json!({
                    "error": "send_now_state_uncommitted", "turn_id": state.turn_id, "sent": true, "retryable": true,
                    "detail": format!("{e:#}"),
                    "message": "字已經送進 codex 了，但這一句還沒記成訊息；用同一個 client_request_id 重送只會補記、不會再打一次。",
                })));
            }
        }
    }
    Ok(PromptOut {
        turn_id: state.turn_id,
        message_id: state.message_id.unwrap_or_default(),
        delivery: state.delivery.into(),
        send_now: Some("steered"),
    })
}
