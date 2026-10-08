//! Supervisor domain rules shared by the daemon adapters.

use crate::lifecycle::LcError;
use serde_json::json;

/// 這筆任務的暫停是人設的嗎？daemon 自己設的暫停 reason 仍交由 AGM 處理。
pub(crate) fn user_pause_reason(paused_reason: Option<&str>) -> Option<&str> {
    const DAEMON_SET: [&str; 5] = ["max_rounds", "no_fable_for_verifier", "push_main_failed", "pr_failed", "clarify"];
    paused_reason.map(str::trim).filter(|r| !r.is_empty() && !DAEMON_SET.contains(r))
}

/// 這個任務現在收不收新交辦：已結案或被使用者暫停就不收。
pub fn mission_gate(m: &crate::mission::store::Mission) -> Result<(), LcError> {
    if m.completed_at.is_some() || m.cancelled_at.is_some() {
        return Err(LcError::conflict("mission is closed", json!({"reason": "mission_closed", "mission_id": m.id})));
    }
    if let Some(why) = user_pause_reason(m.paused_reason.as_deref()) {
        return Err(LcError::conflict(
            "mission is paused by the user; resume it before handing out more work",
            json!({"reason": "mission_paused", "mission_id": m.id, "paused_reason": why, "hint": "使用者決定之後用 `agm mission resume` 再派"}),
        ));
    }
    Ok(())
}

#[cfg(all(test, feature = "daemon-test-harness"))]
pub use crate::runners::supervisor::api::*;
