//! 忙的時候改 codex 的 fast：排到下一次 idle 再當場套用（#393）。
//!
//! `PATCH /api/bots/{id}` 的 live 套用要 pane 閒著（`slash_gate`）；bot 正在跑回合時以前一律退回「需重啟」，
//! 但 `/fast` 是一個鍵就能切的東西，不值得打斷回合。這裡記下「這顆 bot 有欄位等著套」，
//! 等 `pane_agent_status_changed` 的 idle 邊（events.rs）再走同一條 `apply_live_setting`；套不上才留下重啟徽章。
//!
//! 尚未套用的 TUI 工作只記在記憶體；readback 成功後會先把 runtime snapshot 寫進 durable
//! bookkeeping debt，因此 daemon 重啟只需補 DB，不會重送 slash 或 picker 操作。

use crate::lifecycle::s6_ports::DeferredLiveContext;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
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
/// `codex: busy_not_ready`：回合中切 fast 時輸入框有字或選單開著（#712），一樣等回合結束。
pub(crate) fn is_busy_reason(reason: &str) -> bool {
    reason == "slash_gate: agent_busy"
        || reason == "slash_gate: turn_in_flight"
        || reason.strip_prefix("codex: ") == Some(crate::codex_live::BUSY_NOT_READY)
}

/// 排到下一次 idle。回 `false`＝沒排（呼叫端照舊回需重啟）：`single_field` 的 kind（claude／grok 的 slash 指令一次一個值）
/// 已經排著**別的**欄位時不合併——合併後 idle 那次會以 `not_a_single_field` 失敗，而且被合進去的前一個也跟著丟掉。
pub(crate) fn defer_live(bot_id: &str, fields: &[&str], baseline_rev: &str, target_rev: &str, single_field: bool) -> bool {
    let mut m = pending().lock().unwrap_or_else(|e| e.into_inner());
    if single_field && m.get(bot_id).is_some_and(|e| e.fields.iter().any(|f| !fields.contains(f))) {
        return false;
    }
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
    true
}

/// 不在 `live` 裡的 bot 不留排著的即時套用：要等 idle 邊才套，bot 被刪掉就永遠等不到。
pub(crate) fn retain_bots(live: &[String]) {
    pending().lock().unwrap_or_else(|e| e.into_inner()).retain(|id, _| live.contains(id));
}

pub(crate) fn is_deferred(bot_id: &str) -> bool {
    pending().lock().unwrap_or_else(|e| e.into_inner()).contains_key(bot_id)
}

fn take(bot_id: &str) -> Option<PendingLive> {
    pending().lock().unwrap_or_else(|e| e.into_inner()).remove(bot_id)
}

/// idle 邊叫一次：沒有排著的東西就什麼都不做。等一小段讓回合收尾（Stop hook、回讀狀態列）再套。
pub(crate) fn schedule_deferred_live<A: DeferredLiveContext>(app: &A, bot_id: &str) {
    if !is_deferred(bot_id) {
        return;
    }
    let (app, bot_id) = ((*app).clone(), bot_id.to_string());
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let _ = apply_deferred_once(&app, &bot_id).await;
    });
}

/// Apply the current pending item without the production idle-edge delay. Kept as a helper so the
/// retry path can be tested at the exact handoff point without sleeping through a scheduler timer.
pub(crate) async fn apply_deferred_once(
    app: &impl DeferredLiveContext,
    bot_id: &str,
) -> Option<super::LiveApplyOutcome> {
    let queued = take(bot_id)?;
    let fields = queued.fields.to_vec();
    let outcome = app.apply_live_setting_with_revision(
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
            // 同一組欄位放回去（`take` 已清空），不會被 single_field 擋。
            defer_live(
                bot_id,
                &fields,
                &queued.baseline_rev,
                &queued.target_rev,
                false,
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
mod retain_tests {
    use super::*;

    #[test]
    fn a_deleted_bots_deferred_live_apply_is_dropped() {
        for bot in ["defer-gone", "defer-kept"] {
            assert!(defer_live(bot, &["model"], "r0", "r1", false));
        }
        retain_bots(&["defer-kept".to_string()]);
        assert!(!is_deferred("defer-gone") && is_deferred("defer-kept"));
        retain_bots(&[]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_busy_bot_is_worth_waiting_for() {
        assert!(is_busy_reason("slash_gate: agent_busy"));
        assert!(is_busy_reason("slash_gate: turn_in_flight"));
        assert!(is_busy_reason("codex: busy_not_ready"), "回合中切 fast 但輸入框有字：等回合結束");
        assert!(!is_busy_reason("busy_not_ready"), "只認 codex 路徑包出來的那一個");
        assert!(!is_busy_reason("slash_gate: not_running"));
        assert!(!is_busy_reason("codex: readback_fast_mismatch"));
    }

    #[test]
    fn deferring_collects_known_fields_once_and_take_empties_it() {
        let id = "test-deferred-live-bot";
        assert!(!is_deferred(id));
        assert!(defer_live(id, &["fast", "bogus", "fast"], "baseline-a", "target-a", false));
        assert!(defer_live(id, &["effort"], "baseline-b", "target-b", false));
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

    /// claude／grok 的 slash 指令一次一個值：已經排著 model，再排 effort 不合併（合併了 idle 那次整批失敗）。
    #[test]
    fn a_single_field_kind_does_not_merge_a_second_field() {
        let id = "test-deferred-live-single";
        assert!(defer_live(id, &["model"], "base", "t1", true));
        assert!(defer_live(id, &["model"], "base", "t2", true), "同一個欄位再改：更新目標版本");
        assert!(!defer_live(id, &["effort"], "base", "t3", true), "別的欄位：不排，回需重啟");
        assert_eq!(
            take(id),
            Some(PendingLive { fields: vec!["model"], baseline_rev: "base".into(), target_rev: "t2".into() })
        );
        assert!(defer_live(id, &["effort"], "base", "t4", true), "清空後新的一筆照排");
        take(id);
    }
}
