//! 忙的時候改 codex 的 fast：排到下一次 idle 再當場套用（#393）。
//!
//! `PATCH /api/bots/{id}` 的 live 套用要 pane 閒著（`slash_gate`）；bot 正在跑回合時以前一律退回「需重啟」，
//! 但 `/fast` 是一個鍵就能切的東西，不值得打斷回合。這裡記下「這顆 bot 有欄位等著套」，
//! 等 `pane_agent_status_changed` 的 idle 邊（events.rs）再走同一條 `apply_live_setting`；套不上才留下重啟徽章。
//!
//! 尚未套用的 TUI 工作只記在記憶體；readback 成功後會先把 runtime snapshot 寫進 durable
//! bookkeeping debt，因此 daemon 重啟只需補 DB，不會重送 slash 或 picker 操作。

use crate::state::App;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// A queued TUI change keeps the first pre-patch revision and the latest target revision together
/// with its fields. This lets the eventual live receipt clear only the drift covered by that TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingLive {
    fields: Vec<&'static str>,
    baseline_rev: String,
    target_rev: String,
}

fn pending() -> &'static Mutex<HashMap<String, PendingLive>> {
    static P: OnceLock<Mutex<HashMap<String, PendingLive>>> = OnceLock::new();
    P.get_or_init(Default::default)
}

/// 這個 live 套用失敗的理由，是不是「bot 現在忙」——那種才值得等下一次 idle。
pub(crate) fn is_busy_reason(reason: &str) -> bool {
    reason == "slash_gate: agent_busy" || reason == "slash_gate: turn_in_flight"
}

pub(crate) fn defer_live(bot_id: &str, fields: &[&str], baseline_rev: &str, target_rev: &str) {
    let mut m = pending().lock().unwrap_or_else(|e| e.into_inner());
    let e = m.entry(bot_id.to_string()).or_insert_with(|| PendingLive {
        fields: Vec::new(),
        baseline_rev: baseline_rev.to_string(),
        target_rev: target_rev.to_string(),
    });
    // A later PATCH supersedes the desired config but must preserve the original loaded baseline.
    e.target_rev = target_rev.to_string();
    for f in fields {
        let f: &'static str = match *f {
            "model" => "model",
            "effort" => "effort",
            "fast" => "fast",
            _ => continue,
        };
        if !e.fields.contains(&f) {
            e.fields.push(f);
        }
    }
}

pub(crate) fn is_deferred(bot_id: &str) -> bool {
    pending().lock().unwrap_or_else(|e| e.into_inner()).contains_key(bot_id)
}

fn take(bot_id: &str) -> Option<PendingLive> {
    pending().lock().unwrap_or_else(|e| e.into_inner()).remove(bot_id)
}

/// idle 邊叫一次：沒有排著的東西就什麼都不做。等一小段讓回合收尾（Stop hook、回讀狀態列）再套。
pub(crate) fn schedule_deferred_live(app: &Arc<App>, bot_id: &str) {
    if !is_deferred(bot_id) {
        return;
    }
    let (app, bot_id) = (app.clone(), bot_id.to_string());
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let _ = apply_deferred_once(&app, &bot_id).await;
    });
}

/// Apply the current pending item without the production idle-edge delay. Kept as a helper so the
/// retry path can be tested at the exact handoff point without sleeping through a scheduler timer.
pub(crate) async fn apply_deferred_once(
    app: &Arc<App>,
    bot_id: &str,
) -> Option<super::LiveApplyOutcome> {
    let Some(queued) = take(bot_id) else {
        return None;
    };
    let fields: Vec<&str> = queued.fields.iter().copied().collect();
    let outcome = super::apply_live_setting_with_revision(
        app,
        bot_id,
        &fields,
        &queued.baseline_rev,
        &queued.target_rev,
    )
    .await;
    match &outcome {
        super::LiveApplyOutcome::Applied { .. } => {}
        super::LiveApplyOutcome::Failed(why) if is_busy_reason(why) => {
            // 剛閒下來又被新回合搶走：再排一次。
            defer_live(
                bot_id,
                &fields,
                &queued.baseline_rev,
                &queued.target_rev,
            );
        }
        super::LiveApplyOutcome::BookkeepingPending { reason, .. } => {
            tracing::warn!(bot = %bot_id, ?fields, reason, "deferred live apply is waiting for DB-only bookkeeping");
        }
        super::LiveApplyOutcome::Failed(why) => {
            tracing::info!(bot = %bot_id, ?fields, reason = %why, "deferred live apply failed; the restart badge stays")
        }
    }
    app.emit("bot_changed", serde_json::json!({"bot_id": bot_id})).await;
    Some(outcome)
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
        defer_live(id, &["fast", "bogus", "fast"], "baseline-a", "target-a");
        defer_live(id, &["effort"], "baseline-b", "target-b");
        assert!(is_deferred(id));
        assert_eq!(
            take(id),
            Some(PendingLive {
                fields: vec!["fast", "effort"],
                baseline_rev: "baseline-a".into(),
                target_rev: "target-b".into(),
            }),
            "coalescing retains the first loaded baseline and newest requested revision"
        );
        assert!(!is_deferred(id), "take 之後就沒了：不會套兩次");
        assert!(take(id).is_none());
    }
}
