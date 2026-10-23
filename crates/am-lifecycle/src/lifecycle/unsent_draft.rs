//! 卡住回合的 idle 收尾之前，先看 composer 裡是不是還留著這一則（#1897）。
//!
//! 貼字之後 Enter 沒生效、送達證不出來（`delivery='unknown'`、`delivery_verified=0`）的回合，stall watchdog 與重送都只認
//! `delivery='ok'`，`late_proof` 只看「框已經空了」那一種，沒有任何一條路會再看框裡的字。以前閒置滿門檻就收成
//! `completed_fallback`，字留在框裡 34 小時（a-ci優化 2026-10-21 turn `01M5EAEK5Y1HX1DTTQHDGE7QY9`），之後每一則送達都因為
//! 框不是空的而被擋。
//!
//! 收尾前讀一次畫面：
//! - 框裡逐列對得上這一則（[`super::poller::composer_holds_prompt`]）：用掉唯一一次重送額度（`claim_resend`，寫在 DB，
//!   重啟也不會再按第二次），在 host 世代的 fence 裡重讀 run 與 in-flight turn 沒變才補按一次 Enter，這一輪不收、閒置重新計時；
//!   額度已經用掉、字還在：收成 failed＋系統訊息＋`ops_alert`（`undelivered_draft:<turn>`），不清框。
//! - 框裡有字、但對不上（例如 TUI 把長段貼上摺起來）：一個鍵都不按，`ops_alert`（`undelivered_draft_suspect:<turn>`，同一個
//!   key 只寫一次）之後照原本的收尾。
//! - 框是空的：照原本的收尾。
//! - 讀不到畫面：最多等 [`UNREADABLE_ROUNDS`] 輪，這段期間不收；第三輪還讀不到就記一次 WARN、照原本的收尾。

use super::s6_ports::StuckTurnContext;
use super::*;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// 讀不到畫面時最多等幾輪 sweep（含第一次）；第這麼多輪還讀不到就照原本的收尾。
pub const UNREADABLE_ROUNDS: u32 = 3;
/// 讀不到的輪數記在記憶體；超過上限時淘汰最舊的一筆。
const UNREADABLE_MAX: usize = 256;

/// 收尾前這一步的結論。
#[derive(Debug, PartialEq, Eq)]
pub enum Recovery {
    /// 照原本的收尾（`completed`／`completed_fallback`）。
    Proceed,
    /// 這一輪不收（補按了 Enter、或畫面還讀不到）。
    Wait,
    /// 已經收成 failed（字留在框裡、補按過也沒用）。
    Failed,
}

/// 讀不到的輪數（以 turn 為鍵）。表滿時新增前先淘汰最舊的一筆：回合若是被別的路收掉（例如 Stop hook），
/// 它那一筆不會再被 `forget`，不淘汰的話會永遠占著位子、把之後的 turn 擋在等待之外。
#[derive(Default)]
struct Unreadable {
    seq: u64,
    /// turn → (讀不到的輪數, 插入序號)。
    rounds: HashMap<String, (u32, u64)>,
}

impl Unreadable {
    /// 這一則又讀不到一次：回是否還要再等。
    fn again(&mut self, turn_id: &str, max: usize) -> bool {
        if !self.rounds.contains_key(turn_id) && self.rounds.len() >= max {
            if let Some(oldest) = self.rounds.iter().min_by_key(|(_, (_, at))| *at).map(|(k, _)| k.clone()) {
                self.rounds.remove(&oldest);
            }
        }
        self.seq += 1;
        let at = self.seq;
        let entry = self.rounds.entry(turn_id.to_string()).or_insert((0, at));
        entry.0 += 1;
        if entry.0 < UNREADABLE_ROUNDS {
            return true;
        }
        self.rounds.remove(turn_id);
        false
    }
}

fn unreadable_rounds() -> &'static Mutex<Unreadable> {
    static ROUNDS: OnceLock<Mutex<Unreadable>> = OnceLock::new();
    ROUNDS.get_or_init(Default::default)
}

fn unreadable_again(turn_id: &str) -> bool {
    unreadable_rounds().lock().unwrap_or_else(|e| e.into_inner()).again(turn_id, UNREADABLE_MAX)
}

fn forget_unreadable(turn_id: &str) {
    unreadable_rounds().lock().unwrap_or_else(|e| e.into_inner()).rounds.remove(turn_id);
}

/// 呼叫端（`stuck_turns::close_stuck`）持 bot 鎖，`run` 是鎖內重讀的、`turn` 是它的 in-flight。
pub async fn before_idle_close(app: &impl StuckTurnContext, run: &db::Run, turn: &db::Turn) -> anyhow::Result<Recovery> {
    if turn.delivery != "unknown" || turn.delivery_verified != 0 {
        return Ok(Recovery::Proceed);
    }
    let sent = super::poller::turn_echo_texts(app, &turn.id).await?;
    if sent.is_empty() {
        return Ok(Recovery::Proceed);
    }
    let Some(bot) = db::bot(app.db(), &run.bot_id).await? else { return Ok(Recovery::Proceed) };
    let screen = match read_screen(app, run).await {
        Ok(screen) => screen,
        Err(e) => {
            if unreadable_again(&turn.id) {
                tracing::debug!(turn = %turn.id, error = %e, "cannot read the composer before closing an idle turn; waiting for the next sweep");
                return Ok(Recovery::Wait);
            }
            tracing::warn!(turn = %turn.id, bot = %bot.name, reason = "screen_unreadable", error = %e,
                           "could not check the composer for an unsent prompt; closing the idle turn as before");
            return Ok(Recovery::Proceed);
        }
    };
    forget_unreadable(&turn.id);
    if sent.iter().any(|p| super::poller::composer_holds_prompt(&bot.kind, &screen, p)) {
        return still_in_box(app, run, &bot, turn, &sent).await;
    }
    if crate::composer_parse::box_state(&bot.kind, &screen) == crate::composer_parse::BoxState::NonEmpty {
        alert(app, &bot, turn, "undelivered_draft_suspect",
              "送達證不出來的回合要收尾時，輸入框裡還有字，但比對不出是不是這一則（可能是 TUI 把長段貼上摺起來了，或是別人打的）。daemon 沒有按任何鍵。",
              "到終端看這顆 bot 的輸入框：是這一則就按 Enter，不是就自己決定要不要清掉。框裡有字的期間，之後的送達都會被擋。").await;
    }
    Ok(Recovery::Proceed)
}

async fn read_screen(app: &impl StuckTurnContext, run: &db::Run) -> anyhow::Result<String> {
    let pane = run.pane_id.as_deref().ok_or_else(|| anyhow::anyhow!("the run has no pane"))?;
    let client = client_for_run(app, run).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
    super::delivery::read_styled(&client, pane, "visible", 80).await
}

/// 字還在框裡：額度還在就補按一次 Enter；用掉了就收成 failed。
async fn still_in_box(app: &impl StuckTurnContext, run: &db::Run, bot: &db::Bot, turn: &db::Turn, sent: &[String]) -> anyhow::Result<Recovery> {
    if db::claim_resend(app.db(), &turn.id, super::poller::MAX_PROMPT_RESENDS).await? {
        return press_enter_once(app, run, &bot.kind, turn, sent).await;
    }
    fail_undelivered(app, run, bot, turn).await
}

/// 補按一次 Enter：host 世代沒換、run 與 in-flight turn 還是同一筆、框裡的字仍然是這一則才按
/// （畫面在 fence 外讀過一次，fence 內送鍵前再讀一次）。沒按就退還額度，這一輪不收。
async fn press_enter_once(app: &impl StuckTurnContext, run: &db::Run, kind: &str, turn: &db::Turn, sent: &[String]) -> anyhow::Result<Recovery> {
    let pressed = match client_for_run(app, run).await {
        Ok(client) => {
            let pane = run.pane_id.clone().unwrap_or_default();
            let fenced = app.hosts().run_if_current(&client.fence, async {
                if !still_the_same(app, run, turn).await? {
                    return anyhow::Ok(false);
                }
                // 讀不到＝無從確認字還是這一則；鍵還沒送，當成沒按（額度照退）。
                let Ok(screen) = super::delivery::read_styled(&client, &pane, "visible", 80).await else {
                    return anyhow::Ok(false);
                };
                if !sent.iter().any(|p| super::poller::composer_holds_prompt(kind, &screen, p)) {
                    return anyhow::Ok(false);
                }
                client.pane_send_keys(&pane, &["Enter"]).await?;
                anyhow::Ok(true)
            });
            fenced.await.unwrap_or(Ok(false))
        }
        Err(_) => Ok(false),
    };
    match pressed {
        Ok(true) => {
            super::stuck_turns::observe(&run.id, "working");
            tracing::warn!(turn = %turn.id, run = %run.id, "an unproven prompt was still in the composer at the idle close; pressed Enter once");
            Ok(Recovery::Wait)
        }
        Ok(false) => {
            db::refund_resend(app.db(), &turn.id).await?;
            Ok(Recovery::Wait)
        }
        // 送鍵的 RPC 出錯：不知道按了沒有，額度不退（寧可不再按第二次）。
        Err(e) => {
            tracing::warn!(turn = %turn.id, error = %e, "pressing Enter for a prompt left in the composer failed; not pressing it again");
            Ok(Recovery::Wait)
        }
    }
}

async fn still_the_same(app: &impl StuckTurnContext, run: &db::Run, turn: &db::Turn) -> anyhow::Result<bool> {
    let Some(now) = db::run(app.db(), &run.id).await? else { return Ok(false) };
    if now.state != "running" || now.agent_status != "idle" || now.pane_id != run.pane_id || now.herdr_session != run.herdr_session {
        return Ok(false);
    }
    Ok(matches!(db::in_flight_turn(app.db(), &run.id).await?, Some(t) if t.id == turn.id && t.delivery == "unknown"))
}

/// 補按過還在框裡：收成 failed（送達欄位不動）＋系統訊息＋`ops_alert`。框不清：那是人要決定的事。
async fn fail_undelivered(app: &impl StuckTurnContext, run: &db::Run, bot: &db::Bot, turn: &db::Turn) -> anyhow::Result<Recovery> {
    let mut tx = app.db().begin().await?;
    let out = super::turn_controller::fail_on(&mut tx, &turn.id, super::turn_controller::DeliveryOnFail::Keep, "字留在輸入框沒送出").await?;
    if out != super::turn_controller::Outcome::Applied {
        return Ok(Recovery::Wait);
    }
    let note = "這一則貼進了輸入框，但 Enter 沒有生效（送達證不出來）；daemon 補按過一次 Enter，字還留在框裡，所以標成失敗、沒有清掉輸入框。\
                請到終端看一下：要送就按 Enter，不要就清掉；框裡有字的期間，之後的訊息都送不進去。";
    let m = insert_message_tx(&mut tx, &turn.conversation_id, Some(&turn.id), "system", note, "system", false, None).await?;
    tx.commit().await?;
    tracing::warn!(turn = %turn.id, bot = %bot.name, run = %run.id, "an unproven prompt stayed in the composer after one Enter; turn failed");
    emit_message_added(app, &bot.id, m).await;
    app.emit_turn(&turn.id).await;
    alert(app, bot, turn, "undelivered_draft",
          "這一則貼進輸入框後 Enter 沒生效，daemon 補按一次 Enter 也沒送出；回合已標成失敗，字還留在框裡。",
          "到終端看這顆 bot 的輸入框：要送就按 Enter，不要就清掉，之後的送達才會恢復。").await;
    Ok(Recovery::Failed)
}

/// `ops_alert`，key 是 `<reason>:<turn>`：同一個 key 只寫一次（`INSERT OR IGNORE`）。寫不進去只記 log。
async fn alert(app: &impl StuckTurnContext, bot: &db::Bot, turn: &db::Turn, reason: &str, detail: &str, action: &str) {
    let key = format!("{reason}:{}", turn.id);
    let payload = serde_json::json!({
        "source": "daemon",
        "reason": reason,
        "subject": bot.name,
        "bot_id": bot.id,
        "bot_name": bot.name,
        "turn_id": turn.id,
        "detail": format!("bot `{}`（{}）turn `{}`：{detail}", bot.name, bot.id, turn.id),
        "action": action,
    });
    match crate::supervisor_inbox::push_inbox(app.db(), &key, "ops_alert", None, Some(&bot.id), Some(&turn.id), &payload).await {
        Ok(Some(_)) => app.emit("supervisor_changed", serde_json::json!({ "ops_alert": key })).await,
        Ok(None) => {}
        Err(e) => tracing::warn!(turn = %turn.id, reason, error = %e, "could not queue the undelivered draft ops_alert"),
    }
}

#[cfg(all(test, feature = "daemon-test-harness"))]
#[path = "unsent_draft_tests.rs"]
mod tests;
