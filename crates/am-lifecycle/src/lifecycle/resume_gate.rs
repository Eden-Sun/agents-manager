//! `--resume` 接回之後、驗證之前，prompt 不准進去（issue #92）。
//!
//! 換身份（撞額度 → 換帳號）或閒置叫醒用 `--resume <session>` 把原本那段對話接回來。CLI 接不接得回，
//! 要等它自己回報 session 才知道：claude 起來時的 `SessionStart` hook 帶的 session 跟要接的一樣就是接回了
//! （`hookrecv::consume_resume_session` 記成 `verified`）；不一樣就是 CLI 默默開了新對話（`mismatch`，
//! 聊天室裡會有一則 `context_lost` 說明）。以前兩件事都在，**中間沒有閘門**：run 一變成 running＋idle，
//! 排隊的派工就被 flush 進去——`SessionStart` 若還在路上（遠端 hook 走 spool、daemon 剛好在重啟、hook 背景
//! worker 還拿不到 bot 鎖），那一則就送進一段還沒確認是不是原本那段的對話裡。
//!
//! 這裡是那道閘門，規則只住在這一個檔案：
//! - **誰要等**：claude、有注入 hook（`inject_hooks`，沒有 hook 就沒有驗證來源）、這個 run 是 `resume_native`
//!   起的（`runs.resume_session_id` 還在），而且還沒有結論（`runs.resume_outcome` 是 NULL）。
//!   codex／grok 本來就要等第一個回合結束才回報 session（`hookrecv` 的既有規則），不在這裡等——等下去
//!   只會死結：沒有回合就沒有回報，沒有回報就不給回合。agy 同理不等：對話是第一則 prompt 才建立，`PreInvocation` 才帶得出
//!   conversationId（`hookrecv` 照樣比對，對不上＝`resume_mismatch`；沒有 hook 的子 agent 由 `agy_session` 讀 pane 開著的對話資料庫比對）。
//! - **等到什麼時候**：`SessionStart` 來了就放行（對上或對不上都是結論，對不上的那則說明會先進聊天室）；
//!   最多等 [`VERIFY_WINDOW`]，從 `runs.started_at` 算——但 pane 停在要人回答的提示（信任目錄）時那段不算，
//!   人按掉之後還要再給 [`UNBLOCK_GRACE`]（遠端回報要走一輪 spool 掃描）。到期是**刻意的退路**：記 `resume_outcome='unverified'`、
//!   在聊天室留一則看得見的說明、然後放行——hook 壞掉的 bot 不能因此永遠收不到訊息，但也不能假裝驗過了。
//!   之後才到的 `SessionStart` 照樣會被比對，對不上一樣會留 `context_lost`。
//! - **擋的是什麼**：排隊的 flush（`queue::flush_queued_locked`，不花重試額度，掛 timer 到期再來）與直接送入
//!   （`prompt::prompt_inner`：AGM 派工排進佇列；使用者回 409 `resume_unverified` 帶 `retry_after_s`）。
//! - **重啟**：到期時間就是 `runs.started_at + VERIFY_WINDOW`，存在 DB 裡，不另外養 timer 狀態。行程內的 timer
//!   沒了也不用另外接：`rearm_queue_retries` 開機時本來就把每一筆 queued 叫醒一次，flush 走到這裡會照原本的
//!   到期時間重新掛上（SPEC「到期動作不靠行程內的 timer 當唯一真相」）。

use super::*;

/// 從 run 開始到 `SessionStart` 進到 daemon 最多等多久。本機通常幾秒；遠端 hook 寫在那台的 spool，
/// 要等 30 秒一輪的掃描（SPEC §11.4.4）撈回來，再加上 CLI 自己的啟動時間（`start_inner` 最多等 60 秒 ready）。
pub const VERIFY_WINDOW: Duration = Duration::from_secs(120);

/// pane 停在要人回答的提示（信任目錄／權限）時，CLI 還沒真的開始跑，`SessionStart` 不可能送出來——那段
/// 時間不算進 [`VERIFY_WINDOW`]。人按下去之後，回報還要走一趟遠端 spool 掃描（30 秒一輪）才回得到 daemon，
/// 所以狀態真的變了之後至少再等這麼久才肯走退路。
///
/// #92 的 live-SSH 端到端撞到的就是這個：換身份之後的新設定目錄沒信任過專案目錄，遠端的 claude 停在
/// 「❯ No, exit」等了四分半（`agent_status='blocked'`）。窗口是從 `runs.started_at` 算的，早就過了；
/// 人一按下去、`blocked → idle` 那一瞬 flush 回來，閘門會當場記 `unverified`、在聊天室留一則說錯話的
/// 提醒，然後把排著的派工送進一段還沒確認的對話——而真正的 `SessionStart` 下一輪掃描就到了。
pub const UNBLOCK_GRACE: Duration = Duration::from_secs(60);

/// 這一刻該不該讓 prompt 進去。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// 沒在等：沒要求接回、已經有結論、或這顆 bot 沒有驗證來源。
    Open,
    /// 還在等 CLI 回報 session；最多再等 `left`。
    Waiting { expected: String, left: Duration },
    /// 等滿了還沒回報：要走刻意的退路（[`check`] 會記下來並留說明，再當成 `Open`）。
    Expired { expected: String },
}

/// 純規則：所有輸入都由呼叫端給，測得到。
pub fn decide(bot: &db::Bot, run: &db::Run, now: chrono::DateTime<chrono::Utc>) -> Gate {
    if bot.kind != "claude" || bot.inject_hooks == 0 {
        return Gate::Open;
    }
    let Some(expected) = run.resume_session_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
        return Gate::Open;
    };
    if run.resume_outcome.is_some() {
        return Gate::Open;
    }
    let left = [
        remaining(&run.started_at, VERIFY_WINDOW, now),
        (run.agent_status == "blocked").then_some(UNBLOCK_GRACE),
        run.agent_status_since.as_deref().and_then(|since| remaining(since, UNBLOCK_GRACE, now)),
    ]
    .into_iter()
    .flatten()
    .max();
    match left {
        Some(left) => Gate::Waiting { expected: expected.to_string(), left },
        None => Gate::Expired { expected: expected.to_string() },
    }
}

/// 從 `from` 起算 `window` 還剩多久；`None` ＝已經到期。時間讀不懂就當到期：寧可走退路（會留說明），
/// 也不要永遠卡住。
fn remaining(from: &str, window: Duration, now: chrono::DateTime<chrono::Utc>) -> Option<Duration> {
    let started = chrono::DateTime::parse_from_rfc3339(from).ok()?.with_timezone(&chrono::Utc);
    let deadline = started + chrono::Duration::from_std(window).ok()?;
    (deadline - now).to_std().ok().filter(|d| !d.is_zero())
}

/// 呼叫端用這支：`Expired` 在這裡落地（CAS 寫 `resume_outcome='unverified'`＋系統訊息，同一個交易、只寫一次），
/// 然後當成 `Open` 回去。呼叫端持 bot 鎖。
pub async fn check(app: &(impl crate::capabilities::Db + crate::capabilities::Emit), bot: &db::Bot, run: &db::Run, conv: &str) -> Gate {
    let now = chrono::Utc::now();
    match decide(bot, run, now) {
        Gate::Expired { expected } => {
            if let Err(e) = give_up_waiting(app, bot, run, conv, &expected, waited_secs(&run.started_at, now)).await {
                tracing::warn!(bot = %bot.name, run = %run.id, error = %e, "could not record the unverified resume");
            }
            Gate::Open
        }
        other => other,
    }
}

/// 這個 run 從開始到現在等了幾秒（`started_at` 讀不懂就退回窗口長度）。等的時間可能比 [`VERIFY_WINDOW`]
/// 長（[`UNBLOCK_GRACE`]），說明裡要講真的等了多久，不要背一個固定數字。
fn waited_secs(started_at: &str, now: chrono::DateTime<chrono::Utc>) -> u64 {
    chrono::DateTime::parse_from_rfc3339(started_at)
        .ok()
        .and_then(|t| (now - t.with_timezone(&chrono::Utc)).to_std().ok())
        .map_or(VERIFY_WINDOW.as_secs(), |d| d.as_secs())
}

async fn give_up_waiting(
    app: &(impl crate::capabilities::Db + crate::capabilities::Emit),
    bot: &db::Bot,
    run: &db::Run,
    conv: &str,
    expected: &str,
    waited: u64,
) -> anyhow::Result<()> {
    let mut tx = app.db().begin().await?;
    let marked = sqlx::query(
        "UPDATE runs SET resume_outcome = 'unverified'
          WHERE id = ? AND resume_outcome IS NULL AND resume_session_id IS NOT NULL",
    )
    .bind(&run.id)
    .execute(&mut *tx)
    .await?;
    if marked.rows_affected() == 0 {
        return Ok(());
    }
    let note = format!(
        "⚠️ 用 `--resume` 接回原本的對話（session `{expected}`）之後，{waited} 秒內沒有收到 claude 回報的 session，\
         確認不了接回的是不是同一段。排隊的訊息照常送出；之後如果回報的 session 對不上，會再另外提醒。"
    );
    let msg = super::messages::insert_message_tx(&mut tx, conv, None, "system", &note, "system", false, None).await?;
    tx.commit().await?;
    super::messages::emit_message_added(app, &bot.id, msg).await;
    tracing::warn!(bot = %bot.name, run = %run.id, expected, "resume was never confirmed; released the prompt gate as a deliberate fallback");
    Ok(())
}
