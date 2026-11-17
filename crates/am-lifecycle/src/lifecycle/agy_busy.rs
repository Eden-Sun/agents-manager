//! agy 還沒完全閒置的證據（#2146）。
//!
//! agy 的 Stop hook 在背景工作還在跑時帶 `fullyIdle:false`；herdr 這時照樣報 idle，5 秒終端備援與 5 分鐘閒置收尾
//! 都會把還在工作的回合誤收。這裡記「這個 run 的這一個 turn 收過 `fullyIdle:false`」，綁 exact run_id＋turn_id：
//! 別的 turn 不受影響；之後同 turn 的 `fullyIdle:true` Stop、或 agy 又動過（working）再回 idle，才解除。
//! 放記憶體：daemon 重啟後只剩畫面判斷（[`am_base::agy_screen::is_busy`]），寧可晚收。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// `hookrecv::classify` 對 `fullyIdle:false` 的 Ignore 理由；process 靠它辨認並記證據。
pub const NOT_IDLE_REASON: &str = "agy stop with background work still running";

struct Evidence {
    turn_id: String,
    /// 記下之後 agy 是否又動過（working）。動過再回 idle ＝新的 idle，證據解除。
    saw_working: bool,
}

fn store() -> &'static Mutex<HashMap<String, Evidence>> {
    static S: OnceLock<Mutex<HashMap<String, Evidence>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

fn lock() -> std::sync::MutexGuard<'static, HashMap<String, Evidence>> {
    store().lock().unwrap_or_else(|e| e.into_inner())
}

/// 收到 `fullyIdle:false`：這個 run 的這個 turn 還沒完全閒置。
pub fn mark_not_idle(run_id: &str, turn_id: &str) {
    lock().insert(run_id.to_string(), Evidence { turn_id: turn_id.to_string(), saw_working: false });
}

/// 同 run 的 `fullyIdle:true` Stop（或回合已收）：解除。
pub fn clear(run_id: &str) {
    lock().remove(run_id);
}

/// herdr 的狀態事件／每輪掃描：記下 agy 又 working 過。
pub fn note_status(run_id: &str, agent_status: &str) {
    if agent_status == "working" {
        if let Some(e) = lock().get_mut(run_id) {
            e.saw_working = true;
        }
    }
}

/// 這個 turn 還有未解除的 `fullyIdle:false` 證據嗎。看到「working 之後又 idle」就順手解除並回 `false`。
pub fn holds(run_id: &str, turn_id: &str) -> bool {
    let mut m = lock();
    match m.get(run_id) {
        Some(e) if e.turn_id == turn_id => {
            if e.saw_working {
                m.remove(run_id);
                false
            } else {
                true
            }
        }
        _ => false,
    }
}

/// 終端收尾（備援、閒置收尾）該不該放手：證據未解除，或畫面結構上還在忙。證據在、畫面不忙時只在「新的 idle」才放行。
pub fn should_hold(run_id: &str, turn_id: &str, screen: &str) -> bool {
    holds(run_id, turn_id) || am_base::agy_screen::is_busy(screen)
}

pub fn retain_runs(active: &[String]) {
    lock().retain(|id, _| active.contains(id));
}
