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
/// 靠這張表認出「字已經在 pane 裡」，**不再打第二次**。只活在記憶體：daemon 重啟後這份就沒了，
/// 重啟前回應遺失的 steer 重送會再打一次——SPEC §6.3 第 9 點寫明，真機驗證前這是 canary 的已知限制。
#[derive(Clone)]
struct Steered {
    text: String,
    turn_id: String,
    delivery: &'static str,
    message_id: Option<String>,
}

/// canary 的上限：不是帳本，滿了就清（同一個 request id 的重送本來就只發生在幾秒內）。
const MEMO_CAP: usize = 1024;

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
    let prior = memo().lock().unwrap().get(&key).cloned();
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
    let delivery = match app.execute_delivery(client, run, bot, deliver, plan).await {
        Ok(Delivered::Submitted) => "ok",
        Ok(Delivered::Handed | Delivered::Unverified) => "unverified",
        Ok(Delivered::Unproven(why)) => {
            tracing::warn!(bot = %bot.name, reason = why, "codex steer 送出了，但證不出來");
            "unknown"
        }
        // 一個字都沒進 pane：可重試的 409，不留任何紀錄。
        Ok(not @ Delivered::NotAttempted { .. }) => return Err(super::composer_draft::with_draft(client, run, bot, not_attempted_error(&run.id, not)).await),
        Err(e) if crate::herdr::never_applied(&e) => {
            return Err(not_attempted_error(&run.id, Delivered::NotAttempted { reason: "pane_send_refused", retry: true }));
        }
        Err(e) => {
            tracing::warn!(bot = %bot.name, error = %e, "codex steer 打字沒有回應，不知道送出了沒有");
            "unknown"
        }
    };
    let state = Steered { text: text.to_string(), turn_id: turn.id.clone(), delivery, message_id: None };
    {
        let mut m = memo().lock().unwrap();
        if m.len() >= MEMO_CAP {
            m.clear();
        }
        m.insert(key.clone(), state.clone());
    }
    finish(app, bot, key, state).await
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
                state.message_id = Some(m.id);
                memo().lock().unwrap().insert(key, state.clone());
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
