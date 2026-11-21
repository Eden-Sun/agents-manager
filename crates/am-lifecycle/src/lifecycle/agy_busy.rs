//! agy 還沒完全閒置的證據（#2146 ＋ statusLine 即時狀態）。
//!
//! 來源有二：
//! 1. **Stop hook 的 `fullyIdle:false`**：背景工作還在跑時 agy 先送一則 `fullyIdle:false` 的 Stop，herdr 這時照樣報 idle，
//!    5 秒終端備援與 5 分鐘閒置收尾都會把還在工作的回合誤收。這裡記「這個 run 的這一個 turn 收過 `fullyIdle:false`」，
//!    綁 exact run_id＋turn_id：別的 turn 不受影響；之後同 turn 的 `fullyIdle:true` Stop、或 agy 又動過（working）再回 idle、
//!    或 statusLine 明確回報 `agent_state == "idle"` 且 `task_count == 0`，才解除。
//! 2. **statusLine 的 `agent_state` ＋ `task_count`（＋ `tool_confirmation_pending`）**：agy 每當狀態改變就把整包 JSON 給
//!    `agy-hook.sh state`。只要最新一筆 statusLine 顯示 `task_count > 0`（背景工作還在跑）、`tool_confirmation_pending == true`
//!    （工具待確認）、或 `agent_state` 是工作中狀態（`working`／`tool_use`／`reviewing`／`initializing`／`authenticating`），
//!    終端備援與閒置收尾就不收回合；當背景工作結束、statusLine 回到 `agent_state == "idle"` 且 `task_count == 0` 時放行，
//!    既不會一直卡在「輸出中」也不會提早誤收。statusLine 沒來（舊版、沒有 hook 的子 agent）時維持 Stop 證據＋畫面判讀備援。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// `hookrecv::classify` 對 `fullyIdle:false` 的 Ignore 理由；process 靠它辨認並記證據。
pub const NOT_IDLE_REASON: &str = "agy stop with background work still running";

struct Evidence {
    turn_id: String,
    /// 記下之後 agy 是否又動過（working）。動過再回 idle ＝新的 idle，證據解除。
    saw_working: bool,
}

/// statusLine 最新回報的即時狀態（per run）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveState {
    pub agent_state: Option<String>,
    pub task_count: u32,
    pub pending_input_count: u32,
    pub tool_confirmation_pending: bool,
}

impl LiveState {
    /// statusLine 自己說現在是不是還在忙（前景工作、背景 task 或待確認）。
    pub fn is_busy(&self) -> bool {
        if self.task_count > 0 || self.tool_confirmation_pending {
            return true;
        }
        matches!(
            self.agent_state.as_deref(),
            Some("working" | "tool_use" | "reviewing" | "initializing" | "authenticating")
        )
    }

    /// statusLine 明確證明目前已完全閒置（`agent_state == "idle"` 且無背景工作、無待確認）。
    pub fn is_fully_idle(&self) -> bool {
        self.agent_state.as_deref() == Some("idle") && self.task_count == 0 && !self.tool_confirmation_pending
    }
}

fn store() -> &'static Mutex<HashMap<String, Evidence>> {
    static S: OnceLock<Mutex<HashMap<String, Evidence>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

fn live_store() -> &'static Mutex<HashMap<String, LiveState>> {
    static L: OnceLock<Mutex<HashMap<String, LiveState>>> = OnceLock::new();
    L.get_or_init(Default::default)
}

fn lock() -> std::sync::MutexGuard<'static, HashMap<String, Evidence>> {
    store().lock().unwrap_or_else(|e| e.into_inner())
}

fn live_lock() -> std::sync::MutexGuard<'static, HashMap<String, LiveState>> {
    live_store().lock().unwrap_or_else(|e| e.into_inner())
}

/// 收到 `fullyIdle:false`：這個 run 的這個 turn 還沒完全閒置。
pub fn mark_not_idle(run_id: &str, turn_id: &str) {
    lock().insert(run_id.to_string(), Evidence { turn_id: turn_id.to_string(), saw_working: false });
}

/// 同 run 的 `fullyIdle:true` Stop（或回合已收）：解除 Stop 的 `fullyIdle:false` 證據。
pub fn clear(run_id: &str) {
    lock().remove(run_id);
}

/// 收到 agy `statusLine`（`state`）：更新這個 run 的即時 `agent_state`、`task_count`、`pending_input_count`、`tool_confirmation_pending`。
/// 若 statusLine 顯示已回到完全閒置（`agent_state == "idle"` 且 `task_count == 0` 且無待確認），順手解除先前 `fullyIdle:false` 的暫存證據，
/// 避免背景工作結束後沒有第二則 Stop 時一直卡住。
pub fn note_statusline(
    run_id: &str,
    agent_state: Option<&str>,
    task_count: u32,
    pending_input_count: u32,
    tool_confirmation_pending: bool,
) {
    let state = LiveState {
        agent_state: agent_state.map(str::trim).filter(|s| !s.is_empty()).map(str::to_ascii_lowercase),
        task_count,
        pending_input_count,
        tool_confirmation_pending,
    };
    let fully_idle = state.is_fully_idle();
    live_lock().insert(run_id.to_string(), state);
    if fully_idle {
        clear(run_id);
    }
}

/// 讀取這個 run 最近一次 statusLine 記下的狀態（沒有收過 statusLine 回 `None`）。
pub fn live_state(run_id: &str) -> Option<LiveState> {
    live_lock().get(run_id).cloned()
}

/// 這個 run 的 statusLine 是否回報正在等待工具權限確認（`tool_confirmation_pending == true`）。
pub fn tool_confirmation_pending(run_id: &str) -> bool {
    live_lock().get(run_id).is_some_and(|s| s.tool_confirmation_pending)
}

/// herdr 的狀態事件／每輪掃描：記下 agy 又 working 過。
pub fn note_status(run_id: &str, agent_status: &str) {
    if agent_status == "working" {
        if let Some(e) = lock().get_mut(run_id) {
            e.saw_working = true;
        }
    }
}

/// 這個 turn 還有未解除的 `fullyIdle:false` 證據或 statusLine 忙碌證據嗎。看到「working 之後又 idle」（且 statusLine 也沒說還在忙）就順手解除並回 `false`。
pub fn holds(run_id: &str, turn_id: &str) -> bool {
    if live_lock().get(run_id).is_some_and(LiveState::is_busy) {
        return true;
    }
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

/// 終端收尾（備援、閒置收尾）該不該放手：statusLine 顯示還在忙（`task_count > 0`／`agent_state != idle`／`tool_confirmation_pending`）、
/// `fullyIdle:false` 證據未解除，或畫面結構上還在忙。
pub fn should_hold(run_id: &str, turn_id: &str, screen: &str) -> bool {
    holds(run_id, turn_id) || am_base::agy_screen::is_busy(screen)
}

pub fn retain_runs(active: &[String]) {
    lock().retain(|id, _| active.contains(id));
    live_lock().retain(|id, _| active.contains(id));
}
