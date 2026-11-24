//! 重啟中的 bot（issue #106）：stop 與 start 之間那一段「沒有 active run」不是「bot 不在了」。
//!
//! `revoke_orphaned_queued_turns` 的判準是「這顆 bot 沒有 active run ＝ 排著的 prompt 沒有人會送」。
//! 重啟（`restart_bot_with`：換身份、`?resume=native`、一鍵重啟）先停舊 run 再起新 run，中間那一段
//! 剛好符合這個判準，於是 AGM 排著的派工被當成孤兒撤掉——換身份之後什麼都不會送，`resume_gate`
//! 再完美也沒有東西可以放行。會在那一段撤掉它的不只 stop 自己：`restart_start` 收掉擋路 run 的
//! `mark_run_exited`、不拿 bot 鎖的 pane-exit 事件（`events::end_runs_for_pane`）、定時的
//! `revoke_all_orphaned_queued_turns` 都會。所以不在各處補判斷，而是讓重啟**宣告**自己在進行中，
//! 撤孤兒的那一支只問這一件事。
//!
//! hold 本身是行程記憶體，但重啟的**意圖**是持久的（`restart` intent，#355）：daemon 在 stop 與 start 之間死掉，
//! 開機由 `restart_intents::recover_host` 往前補完那次重啟，所以排著的派工同樣不是孤兒（#378）。
//! 開機時（對帳之前，那會把舊 run 收尾並撤孤兒）[`adopt_open_intents`] 把每件還開著的 `restart` intent 灌成一個 hold，
//! recovery 把那件 intent 收尾（done／abandoned／failed／過期）才放掉；補不成的重試期間 hold 也留著。

use super::LcHost;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

fn holds() -> &'static Mutex<HashMap<String, u32>> {
    static M: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

/// 重啟進行中的憑證；drop 掉就結束。計數而不是布林：同一顆 bot 理論上不會重疊（都在 bot 鎖裡），
/// 真的重疊了也不會被先結束的那一個提早清掉。
pub struct Hold {
    bot_id: String,
}

pub fn begin(bot_id: &str) -> Hold {
    if let Ok(mut m) = holds().lock() {
        *m.entry(bot_id.to_string()).or_insert(0) += 1;
    }
    Hold { bot_id: bot_id.to_string() }
}

impl Drop for Hold {
    fn drop(&mut self) {
        if let Ok(mut m) = holds().lock() {
            if let Some(n) = m.get_mut(&self.bot_id) {
                *n -= 1;
                if *n == 0 {
                    m.remove(&self.bot_id);
                }
            }
        }
    }
}

/// 開機接回的 hold：(資料目錄, intent id) → 憑證。`recover_host` 收掉那件 intent 才放。
/// 帶資料目錄是因為「只留還開著的」（[`retain_open`]）只能對**同一個 DB** 的 intent 下判斷：測試共用同一個行程，
/// 另一個 App 的 `recover_host` 看到的開著的 intent 不含我們的，不能因此把我們的 hold 放掉（整樹負載下偶發紅）。
fn adopted() -> &'static Mutex<HashMap<(String, String), Hold>> {
    static M: OnceLock<Mutex<HashMap<(String, String), Hold>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

/// 開機（**對帳之前**）把還開著的 `restart` intent 各灌一個 hold。讀不到 DB 只能記 log（此時什麼都撤不了以外的事無從判斷），
/// 之後 `recover_host` 照樣會補完，只是這段視窗沒有保護。
pub async fn adopt_open_intents(app: &impl LcHost) {
    match crate::lifecycle::s6_ports::RestartHoldHostServices::open_restart_intents(app).await {
        Ok(open) => {
            for i in open.into_iter().filter(|i| i.kind == "restart") {
                let hold = begin(&i.subject_id);
                if let Ok(mut m) = adopted().lock() {
                    m.entry((owner(app), i.id)).or_insert(hold);
                }
            }
        }
        Err(e) => tracing::warn!(error = %e, "cannot list open restart intents; queued prompts are unprotected until recovery"),
    }
}

/// 那件 intent 已經收尾：放掉它的 hold（沒有就什麼都不做）。
pub fn release_intent(app: &impl crate::capabilities::DataDir, intent_id: &str) {
    let hold = adopted().lock().ok().and_then(|mut m| m.remove(&(owner(app), intent_id.to_string())));
    drop(hold);
}

fn owner(app: &impl crate::capabilities::DataDir) -> String {
    crate::capabilities::DataDir::data_dir(app).display().to_string()
}

/// 只留還開著的 intent 的 hold；其餘（例如過期被收掉的）放掉。
pub fn retain_open(app: &impl crate::capabilities::DataDir, open_ids: &std::collections::HashSet<String>) {
    let me = owner(app);
    let gone: Vec<Hold> = match adopted().lock() {
        Ok(mut m) => {
            let keys: Vec<(String, String)> = m.keys().filter(|(o, id)| *o == me && !open_ids.contains(id)).cloned().collect();
            keys.into_iter().filter_map(|k| m.remove(&k)).collect()
        }
        Err(_) => return,
    };
    drop(gone);
}

/// 這顆 bot 現在是不是在重啟（stop 到 start 之間）。鎖壞了當成「不是」：照舊行為撤孤兒，不會卡住佇列。
pub fn in_progress(bot_id: &str) -> bool {
    holds().lock().map(|m| m.contains_key(bot_id)).unwrap_or(false)
}

#[cfg(any(test, feature = "test-hooks", all(test, feature = "daemon-test-harness")))]
#[doc(hidden)]
pub mod test_seams {
    use super::*;
}
