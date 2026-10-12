//! 同步窗口判不出、但框已空的送達（`not_proven_submitted`，issue #1479）：背景繼續看證據。
//!
//! `confirm_submitted` 只看 `SUBMIT_CHECKS × SUBMIT_SETTLE_MS` 這一小段。字已經打進去、Enter 按了、框是空的，
//! 證據卻還沒長出來時，以前就此記成 `delivery=unknown`＋`in_flight`：網頁鎖住輸入框、叫使用者「放棄該回合」，
//! 而 bot 其實收到了、正在做——要等 Stop hook 才解開（遠端 claude 的回音列補白 bug 之外，transcript 晚一步 flush、
//! 回音被 spinner 擠出視窗也是同一種情形）。
//!
//! 這裡在寫完 `unknown` 之後接手，每隔 [`CHECK_EVERY`] 看一次，最多 [`CHECKS`] 次：
//! - 證據長出來（回音列／transcript 比基準多了這一則）→ `Submitted`：`ok`＋已驗證；
//! - 框仍是空的、而且 agent 明顯在做事（hook 有動靜、畫面有進行中的 spinner、herdr 說 working）→ `Unverified`：
//!   `ok`＋未驗證，跟「框空了但沒有無損證據」那條路同一個結果（不自動重送）；
//! - 兩種都沒有就維持 `unknown`：框是空的但 bot 沒動、也沒有證據，不放寬。
//!
//! 升級只用 CAS（`status='in_flight' AND delivery='unknown'`）：Stop hook、放棄、run 結束先到的，這裡什麼都不寫。
//! 基準（回音列數、transcript offset）只有打字的那一刻知道，所以 `confirm_submitted` 在回 `Unproven` 前記在
//! 這裡（以 run 為鍵、很快過期），呼叫端寫完 `unknown` 再用 [`arm`] 認領。

use super::delivery::{box_state_within, evidence, read_composer, same_session, typed_tail, BoxState, Delivered, Proof};
use super::*;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 每隔多久看一次。
pub const CHECK_EVERY: Duration = Duration::from_secs(3);
/// 最多看幾次（約三分鐘）：之後留給 Stop hook 與 stuck_turns。
pub const CHECKS: u32 = 60;
/// 記下來之後多久內要被認領；過了就當是別一則的殘影。
const CLAIM_WITHIN: Duration = Duration::from_secs(60);

/// 打字那一刻的證據基準，加上之後讀畫面要用的東西。
#[derive(Debug, Clone)]
pub struct Pending {
    pub kind: String,
    pub pane: String,
    pub proof: Proof,
    pub offset: u64,
    pub baseline: usize,
    pub text: String,
    /// 送出鍵按下去的時刻（`db::now()` 格式）：hook 動靜從這之後算起，不算排隊期間別人的事件。
    pub since: String,
    noted: Instant,
}

impl Pending {
    pub fn new(kind: &str, pane: &str, proof: &Proof, offset: u64, baseline: usize, text: &str, since: String) -> Pending {
        Pending {
            kind: kind.to_string(),
            pane: pane.to_string(),
            proof: proof.clone(),
            offset,
            baseline,
            text: text.to_string(),
            since,
            noted: Instant::now(),
        }
    }
}

fn registry() -> &'static Mutex<HashMap<String, Pending>> {
    static M: OnceLock<Mutex<HashMap<String, Pending>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

/// 這個 run 剛判不出來的那一則（`confirm_submitted` 呼叫）。同一個 run 只留最新的一筆。
pub fn note(run_id: &str, p: Pending) {
    registry().lock().unwrap_or_else(|e| e.into_inner()).insert(run_id.to_string(), p);
}

/// 新的一輪送達開始：上一則沒被認領的殘影不能留給這一則。
pub fn clear(run_id: &str) {
    registry().lock().unwrap_or_else(|e| e.into_inner()).remove(run_id);
}

fn take(run_id: &str) -> Option<Pending> {
    let p = registry().lock().unwrap_or_else(|e| e.into_inner()).remove(run_id)?;
    (p.noted.elapsed() <= CLAIM_WITHIN).then_some(p)
}

/// 一次檢查的結論。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Look {
    /// 證據長出來了：照 `Submitted` 記。
    Submitted,
    /// 框是空的、agent 在做事，但沒有無損證據：照 `Unverified` 記。
    Active,
    /// 還看不出來，下一輪再看。
    Wait,
    /// 不用再看了（回合已經被別的路收掉、run 換了、session 換了）。
    Stop,
}

/// 看一次。只讀，不寫任何東西、不按任何鍵。
pub async fn look(app: &impl LcHost, bot_id: &str, turn_id: &str, p: &Pending) -> Look {
    let row: Option<(String, String, Option<String>)> =
        match sqlx::query_as("SELECT status, delivery, run_id FROM turns WHERE id = ?").bind(turn_id).fetch_optional(app.db()).await {
            Ok(r) => r,
            Err(_) => return Look::Wait,
        };
    let Some((status, delivery, run_id)) = row else { return Look::Stop };
    let Some(run_id) = run_id else { return Look::Stop };
    if status != "in_flight" || delivery != "unknown" {
        return Look::Stop;
    }
    let run = match db::run(app.db(), &run_id).await {
        Ok(Some(r)) => r,
        Ok(None) => return Look::Stop,
        Err(_) => return Look::Wait,
    };
    if run.state != "running" || run.pane_id.as_deref() != Some(p.pane.as_str()) || !same_session(app, &run_id, &p.proof).await {
        return Look::Stop;
    }
    let Ok(client) = client_for_run(app, &run).await else { return Look::Wait };
    let Ok(screen) = read_composer(&client, &p.pane).await else { return Look::Wait };
    if evidence(&p.kind, &p.proof, p.offset, &screen, &p.text).is_ok_and(|n| n > p.baseline) {
        return Look::Submitted;
    }
    // 沒有證據時只看「框空了＋agent 在忙」：框裡還有字（字還沒送出去）或畫面讀不準，都不算。
    if box_state_within(&p.kind, &screen, typed_tail(&p.text)) != BoxState::Empty {
        return Look::Wait;
    }
    let working = run.agent_status == "working";
    let hooks = super::poller::has_hook_activity(app.db(), bot_id, &p.since, turn_id).await;
    let spinner = !working
        && !hooks
        && client.pane_read(&p.pane, "visible", 60).await.is_ok_and(|read| super::poller::is_screen_busy(&read.text));
    if working || hooks || spinner {
        Look::Active
    } else {
        Look::Wait
    }
}

/// 升級：只有還在飛、還是 `unknown` 的那一筆。`Ok(false)`＝別的路先收掉了。
pub async fn promote(app: &impl LcHost, turn_id: &str, how: Delivered) -> anyhow::Result<bool> {
    let rec = how.record().expect("Submitted／Unverified 都有記錄");
    let moved = sqlx::query(
        "UPDATE turns SET delivery = ?, delivery_verified = ?, auto_resend = ?,
                resend_count = MAX(resend_count, ?)
          WHERE id = ? AND status = 'in_flight' AND delivery = 'unknown'",
    )
    .bind(rec.stored)
    .bind(i64::from(rec.verified))
    .bind(i64::from(rec.auto_resend))
    .bind(super::MAX_PROMPT_RESENDS)
    .bind(turn_id)
    .execute(app.db())
    .await?
    .rows_affected();
    Ok(moved > 0)
}

/// 一輪一輪看到有結論或看完為止。回升級成什麼（沒升級＝`None`）。`checks`／`every` 給測試用。
pub async fn watch(app: &impl LcHost, run_id: &str, bot_id: &str, turn_id: &str, p: &Pending, checks: u32, every: Duration) -> Option<Delivered> {
    for _ in 0..checks {
        tokio::time::sleep(every).await;
        let how = match look(app, bot_id, turn_id, p).await {
            Look::Submitted => Delivered::Submitted,
            Look::Active => Delivered::Unverified,
            Look::Wait => continue,
            Look::Stop => return None,
        };
        match promote(app, turn_id, how).await {
            Ok(true) => {
                tracing::info!(run = %run_id, turn = %turn_id, how = ?how, "送達狀態未知的回合背景補到了證據：升成 ok、解鎖");
                emit_turn(app, turn_id).await;
                // 跟一般 ok／unverified 的送達同一套看門狗（`prompt.rs` 寫完送達結果之後做的事）。
                super::poller::arm_stall(app, run_id, bot_id, turn_id).await;
                super::poller::arm_progress(app, run_id, bot_id, turn_id).await;
                return Some(how);
            }
            Ok(false) => return None,
            Err(e) => tracing::warn!(turn = %turn_id, error = %e, "送達狀態未知的回合升級寫不進去，下一輪再試"),
        }
    }
    None
}

#[cfg(test)]
pub(crate) fn armed_in_tests() -> &'static Mutex<Vec<(String, Pending)>> {
    static M: OnceLock<Mutex<Vec<(String, Pending)>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

/// 呼叫端寫完 `delivery=unknown` 之後認領這一筆（沒有記著＝不是 `not_proven_submitted`，什麼都不做）。
/// 背景任務不擋呼叫端；測試裡不開背景任務（跟 `owed_delivery::schedule_retry` 一樣），只留下記號，行為由
/// [`watch`]／[`look`] 直接驗。
pub fn arm(app: &impl LcHost, run_id: &str, bot_id: &str, turn_id: &str) {
    let Some(p) = take(run_id) else { return };
    #[cfg(test)]
    {
        let _ = (app, bot_id);
        armed_in_tests().lock().unwrap_or_else(|e| e.into_inner()).push((turn_id.to_string(), p));
    }
    #[cfg(not(test))]
    {
        let app = (*app).clone();
        let (run_id, bot_id, turn_id) = (run_id.to_string(), bot_id.to_string(), turn_id.to_string());
        tokio::spawn(async move {
            watch(&app, &run_id, &bot_id, &turn_id, &p, CHECKS, CHECK_EVERY).await;
        });
    }
}
