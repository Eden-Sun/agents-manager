//! Codex 0.157.0's startup model migration screen requires a person to choose an outcome. The same goes for the
//! other dialogs that sit in front of the composer and want a person's choice: the startup "Update available" menu
//! (default `1. Update now`) and the rate-limit model-switch popup ([`Dialog`]).
//!
//! Keep the dialog open, mark the run blocked (with the reason, in the conversation) when herdr misses the prompt,
//! and hold queued deliveries until the user finishes the choice. This flow never sends keys to the pane.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

pub const WAITING_HINT: &str = "Codex 正停在模型升級提示，請到「終端」選 Try new model 或 Use existing model。daemon 不替你選，也不會把訊息打進選單；完成選擇後排隊的訊息會繼續送出。";
/// 啟動時的更新選單（預設選 `1. Update now`）。
pub const UPDATE_WAITING_HINT: &str = "Codex 更新提示等待選擇：啟動時跳出「Update available」選單（預設選 1. Update now）擋在輸入列前面。請到「終端」選 2. Skip（或 3. Skip until next version）；daemon 不替你選更新，也不會把訊息打進選單；完成選擇後排隊的訊息會繼續送出。";
/// 額度快用完時的換模型建議。
pub const RATE_LIMIT_WAITING_HINT: &str = "Codex 正在問要不要為了降低額度消耗切換模型；請到「終端」選擇，daemon 不會替你選，也不會把訊息打進選單；完成選擇後排隊的訊息會繼續送出。";
/// 啟動時的 hook 審核框（codex 0.169+）：選項 2 會信任所有待審 hook，所以一個鍵都不替使用者按。
pub const HOOK_REVIEW_WAITING_HINT: &str = "Codex 停在「Hooks need review」審核框（有新的或改過的 hook 還沒信任），擋在輸入列前面，訊息送不進去。重新啟動這顆 bot 即可（AG Man 啟動時會清掉自己留下、未信任的 hook；不會自動按鍵，也不會把訊息打進審核框）；排隊的訊息會在重啟後繼續送出。";
pub const CLOSED_NOTE: &str = "Codex 擋住輸入列的提示已關閉，排隊的訊息可繼續送出。";

/// 擋在輸入列前面、要使用者本人選的 codex 畫面。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialog {
    Migration,
    UpdateMenu,
    RateLimitSwitch,
    HookReview,
}

impl Dialog {
    /// 畫面上現在開著的是哪一種（都不是＝`None`）。
    pub fn of(screen: &str) -> Option<Self> {
        if crate::lifecycle::codex_hook_review::review_open(screen) {
            Some(Self::HookReview)
        } else if crate::tui_prompts::is_codex_model_migration_prompt(screen) {
            Some(Self::Migration)
        } else if crate::codex_update::update_menu_open(screen) {
            Some(Self::UpdateMenu)
        } else if crate::codex_live::rate_limit_switch_prompt_open(screen) {
            Some(Self::RateLimitSwitch)
        } else {
            None
        }
    }

    /// 結構化的原因（`blocked_reason`）：穩定的短代碼＋一句話。
    pub fn reason(self) -> crate::blocked_reason::Reason {
        match self {
            Self::Migration => crate::blocked_reason::Reason { code: "codex_migration", text: "codex 模型升級提示等待選擇".into() },
            Self::UpdateMenu => crate::blocked_reason::Reason { code: "codex_update_menu", text: "codex 更新提示等待選擇".into() },
            Self::RateLimitSwitch => crate::blocked_reason::Reason { code: "rate_limit_switch", text: "codex 額度換模型建議等待選擇".into() },
            Self::HookReview => crate::blocked_reason::Reason { code: "codex_hook_review", text: "codex hook 審核框擋住輸入（重新啟動即可）".into() },
        }
    }

    /// 告訴使用者「為什麼卡住、要去哪裡處理」的那句話。
    pub fn hint(self) -> &'static str {
        match self {
            Self::Migration => WAITING_HINT,
            Self::UpdateMenu => UPDATE_WAITING_HINT,
            Self::RateLimitSwitch => RATE_LIMIT_WAITING_HINT,
            Self::HookReview => HOOK_REVIEW_WAITING_HINT,
        }
    }
}

pub struct Episode {
    /// 由這裡補標成 `blocked` 之前的狀態；`None` = herdr 自己判的。
    pub forced_from: Option<String>,
    /// Only one observer may finish the episode while it awaits a database restore.
    pub closing: bool,
    /// 開著的是哪一種（`blocked_reason` 靠它）。
    pub dialog: Dialog,
}

pub fn open() -> &'static Mutex<HashMap<String, Episode>> {
    static V: OnceLock<Mutex<HashMap<String, Episode>>> = OnceLock::new();
    V.get_or_init(Default::default)
}

/// 不在 `active` 裡的 run（結束了）不留開著的記錄：框開著時 run 就結束的話，沒有人會再讀到「框關了」，記錄只增不減。
pub fn retain_runs(active: &[String]) {
    open().lock().unwrap().retain(|id, _| active.contains(id));
}

/// 這個 run 現在開著的是哪個擋路的 codex 畫面（`blocked_reason` 讀）；沒有＝`None`。
pub fn open_dialog(run_id: &str) -> Option<Dialog> {
    open().lock().unwrap().get(run_id).filter(|e| !e.closing).map(|e| e.dialog)
}

#[cfg(any(test, feature = "test-hooks", all(test, feature = "daemon-test-harness")))]
pub fn is_open(run_id: &str) -> bool {
    open().lock().unwrap().contains_key(run_id)
}

#[cfg(all(test, not(any(feature = "test-hooks", all(test, feature = "daemon-test-harness")))))]
mod retain_tests {
    use super::*;
    #[test]
    fn an_episode_of_a_run_that_ended_is_dropped() {
        for id in ["cmm-gone", "cmm-kept"] {
            open().lock().unwrap().insert(
                id.to_string(),
                Episode { forced_from: None, closing: false, dialog: Dialog::Migration },
            );
        }
        retain_runs(&["cmm-kept".to_string()]);
        assert!(!is_open("cmm-gone") && is_open("cmm-kept"));
        retain_runs(&[]);
    }
}
