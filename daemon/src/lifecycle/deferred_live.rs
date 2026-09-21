//! 忙的時候改 codex 的 fast：排到下一次 idle 再當場套用（#393）。
//!
//! `PATCH /api/bots/{id}` 的 live 套用要 pane 閒著（`slash_gate`）；bot 正在跑回合時以前一律退回「需重啟」，
//! 但 `/fast` 是一個鍵就能切的東西，不值得打斷回合。這裡記下「這顆 bot 有欄位等著套」，
//! 等 `pane_agent_status_changed` 的 idle 邊（events.rs）再走同一條 `apply_live_setting`；套不上才留下重啟徽章。
//!
//! 只記在記憶體：daemon 重啟後這份就沒了，這時 UI 的落差徽章仍在，使用者再按一次「當場套用」即可。

use crate::db;
use crate::state::App;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// bot id → 等著套的欄位（`model`／`effort`／`fast` 的子集）。
fn pending() -> &'static Mutex<HashMap<String, Vec<&'static str>>> {
    static P: OnceLock<Mutex<HashMap<String, Vec<&'static str>>>> = OnceLock::new();
    P.get_or_init(Default::default)
}

/// 這個 live 套用失敗的理由，是不是「bot 現在忙」——那種才值得等下一次 idle。
pub(crate) fn is_busy_reason(reason: &str) -> bool {
    reason == "slash_gate: agent_busy" || reason == "slash_gate: turn_in_flight"
}

pub(crate) fn defer_live(bot_id: &str, fields: &[&str]) {
    let mut m = pending().lock().unwrap_or_else(|e| e.into_inner());
    let e = m.entry(bot_id.to_string()).or_default();
    for f in fields {
        let f: &'static str = match *f {
            "model" => "model",
            "effort" => "effort",
            "fast" => "fast",
            _ => continue,
        };
        if !e.contains(&f) {
            e.push(f);
        }
    }
}

pub(crate) fn is_deferred(bot_id: &str) -> bool {
    pending().lock().unwrap_or_else(|e| e.into_inner()).contains_key(bot_id)
}

fn take(bot_id: &str) -> Vec<&'static str> {
    pending().lock().unwrap_or_else(|e| e.into_inner()).remove(bot_id).unwrap_or_default()
}

/// idle 邊叫一次：沒有排著的東西就什麼都不做。等一小段讓回合收尾（Stop hook、回讀狀態列）再套。
pub(crate) fn schedule_deferred_live(app: &Arc<App>, bot_id: &str) {
    if !is_deferred(bot_id) {
        return;
    }
    let (app, bot_id) = (app.clone(), bot_id.to_string());
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let fields = take(&bot_id);
        if fields.is_empty() {
            return;
        }
        match super::apply_live_setting(&app, &bot_id, &fields).await {
            None => {
                // 同 PATCH：套成功就把這個 run 的啟動版本蓋成現在的設定，否則會被誤判成過期（#353）。
                if let (Ok(Some(run)), Ok(Some(bot))) = (db::active_run(&app.db, &bot_id).await, db::bot(&app.db, &bot_id).await) {
                    if let Err(e) = crate::launch_rev::stamp(&app.db, &run.id, &crate::launch_rev::of(&bot)).await {
                        tracing::warn!(bot = %bot_id, error = %e, "could not record the launch revision after a deferred live apply");
                    }
                }
            }
            Some(why) if is_busy_reason(&why) => {
                // 剛閒下來又被新回合搶走：再排一次。
                defer_live(&bot_id, &fields);
            }
            Some(why) => tracing::info!(bot = %bot_id, ?fields, reason = %why, "deferred live apply failed; the restart badge stays"),
        }
        app.emit("bot_changed", serde_json::json!({"bot_id": bot_id})).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_busy_bot_is_worth_waiting_for() {
        assert!(is_busy_reason("slash_gate: agent_busy"));
        assert!(is_busy_reason("slash_gate: turn_in_flight"));
        assert!(!is_busy_reason("slash_gate: not_running"));
        assert!(!is_busy_reason("codex: readback_fast_mismatch"));
    }

    #[test]
    fn deferring_collects_known_fields_once_and_take_empties_it() {
        let id = "test-deferred-live-bot";
        assert!(!is_deferred(id));
        defer_live(id, &["fast", "bogus", "fast"]);
        defer_live(id, &["effort"]);
        assert!(is_deferred(id));
        assert_eq!(take(id), vec!["fast", "effort"]);
        assert!(!is_deferred(id), "take 之後就沒了：不會套兩次");
        assert!(take(id).is_empty());
    }
}
