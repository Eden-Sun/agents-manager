//! `runs.state` / `turns.status` 轉移現況表（issue #76）。
//!
//! 這不是理想模型，是從現有程式碼逐條 grep `UPDATE runs SET state` / `UPDATE turns SET status`
//! 讀出來、只收生產路徑（排掉 `#[cfg(test)]`）的結果，2026-09-18 讀的一份快照。程式碼才是準：
//! 這張表過時了就改表，不要改程式碼去配合表。每條邊後面的 `file:line` 指到實際下手的那一句，
//! 遇到疑點自己去核對。
//!
//! `supervisor_assignments.status` 的轉移表**不在這裡**：issue #71（2026-09-18 較晚落地）已經把它
//! 收進 `daemon/src/supervisor/assignment_state.rs`（`allowed()`／`sources_for()`／`TERMINAL`），
//! 而且是真的接進生產路徑的守衛（`mark_delivered`／`settle_and_notify`／`park_quota_blocked` 的
//! `WHERE status IN (…)` 都從那份表算出來），不是另一份旁觀的文件。這裡不重複抄一份——兩份表
//! 遲早會漂移，`assignment_state.rs` 才是唯一權威。
//!
//! 目前只有這個檔案自己的測試在讀這些表（`cargo clippy` 不編 `#[cfg(test)]`，所以看起來是
//! death code）；`#[allow(dead_code)]` 是因為它們的價值是「給人看、給以後可能出現的
//! TurnController（issue #69）當起點」，不是現在就要被生產程式碼呼叫。
//!
//! issue #76 checklist 第 4 項（舊 Run 的事件 vs 新 Run 的 Turn）**不在這張表裡**：#69 已經落地
//! （`lifecycle/fence.rs`），管的是 run 的世代（generation），不是這裡這種單一 run 內的
//! `runs.state`／`turns.status` CAS 邊，所以不重複列一份。覆蓋在 `fence.rs` 自己的 `decide()` 單元
//! 測試＋`classify_finds_the_prior_run_by_insertion_order_even_when_its_ulid_sorts_after_the_current_one`
//! （issue #98：世代序看的是 sqlite rowid 寫入順序，不是 ULID 字典序）＋
//! `hookrecv.rs::external_claim_tests::a_late_stop_from_the_previous_run_cannot_touch_the_new_runs_turn`
//! （接線到 `hookrecv::process` 的端到端驗證）。

#![allow(dead_code)]

/// `daemon/src/db.rs` SCHEMA 的 `runs.state` CHECK 約束：唯一合法值集合。
pub const RUN_STATES: [&str; 5] = ["starting", "running", "stopping", "stopped", "exited"];

/// `daemon/src/db.rs` SCHEMA 的 `turns.status` CHECK 約束。
pub const TURN_STATUSES: [&str; 5] = ["queued", "in_flight", "completed", "completed_fallback", "failed"];

/// `daemon/src/db.rs` SCHEMA 的 `turns.delivery` CHECK 約束。
pub const TURN_DELIVERIES: [&str; 4] = ["pending", "ok", "unknown", "failed"];

/// 觀察到的 `runs.state` 合法邊。`exited` 沒有出邊——生產路徑裡找不到任何從 `exited` 轉出去的
/// UPDATE；一顆 run 進了 `exited` 就是這輩子結束了，要再跑得開一顆新 run。
pub const RUN_STATE_EDGES: &[(&str, &str, &str)] = &[
    ("(insert)", "starting", "lifecycle/start.rs:161 新開一顆 run"),
    ("starting", "running", "reconcile.rs:344,390 CASE WHEN…THEN 'running'（在對應的 herdr agent 裡看到了）；lifecycle/start.rs start_inner（CAS `run_state::transition`，agent_status 探測回來；寫不進去回 503、排對帳重試，#145）"),
    ("stopping", "running", "reconcile.rs:344,390 同一條 CASE WHEN；lifecycle/start.rs:870 guarded UPDATE（停止被打斷，bot 又動了）"),
    ("starting", "stopping", "lifecycle/start.rs:841、lifecycle/stop.rs:36（使用者主動停止；SQL 本身沒有 guard 限定 FROM，呼叫端只在該轉的時候才呼叫）"),
    ("running", "stopping", "同上"),
    ("stopping", "stopped", "lifecycle/start.rs:877、lifecycle/stop.rs:72"),
    ("stopped", "exited", "lifecycle/start.rs:781（唯一在 SQL 本身就 guard `AND state='stopped'` 的一條）"),
    ("starting", "exited", "lifecycle/queue.rs mark_run_exited（CAS guard `AND state IN ('starting','running','stopping')`，#131 之前只有 Rust 層的 SELECT；寫不進去就不收尾、排對帳重試，#135）；lifecycle/start.rs start_bot_locked_with 啟動失敗（CAS from starting；寫不進去排對帳重試，#145）"),
    ("running", "exited", "同上"),
    ("stopping", "exited", "同上"),
];

/// 觀察到的 `turns.status` 合法邊。大多數邊在 SQL 本身就用 `AND status='from'` 當 CAS guard；
/// 少數（標了「Rust 層 guard」的）guard 在呼叫端的 SELECT，不在這句 UPDATE 的 WHERE 裡——
/// 這正是 #76 item 2/3/8 最值得釘的縫（見 `hookrecv.rs` 的
/// `a_late_stop_hook_does_not_resurrect_a_turn_that_interrupt_already_failed` 系列測試）。
pub const TURN_STATUS_EDGES: &[(&str, &str, &str)] = &[
    ("(insert)", "queued", "排隊的 web prompt"),
    ("(insert)", "in_flight", "直接送進 pane 的 prompt"),
    ("queued", "in_flight", "lifecycle/queue.rs:81 flush_queued_locked，CAS guard `AND status='queued'`"),
    ("queued", "failed", "lifecycle/queue.rs:71（空白 prompt 直接丟棄）、及 revoke_orphaned_queued_turns／revoke_queued_turn（run 已結束或交辦被撤）"),
    ("in_flight", "completed", "hookrecv.rs:452 fill_or_drop_late_hook（前面有 has_reply/body_text 檢查，Rust 層 guard，見下方 completed_fallback→completed）；hookrecv.rs:650 CAS guard `AND status='in_flight'`"),
    ("in_flight", "completed_fallback", "lifecycle/poller.rs:1140 CAS guard `AND status='in_flight'`；hookrecv.rs 的 fallback_wins 測試用 trigger 模擬備援贏過 hook 的窄窗"),
    ("in_flight", "failed", "turn_error.rs:216、lifecycle/screen.rs:85、lifecycle/stuck_turns.rs:184、lifecycle/poller.rs:926,1011,1151、lifecycle/prompt.rs 多處、lifecycle/queue.rs:232,311,384、lifecycle/queue.rs:574 fail_in_flight——全部 CAS guard `AND status='in_flight'`，除了 fail_in_flight 本身：guard 在它自己呼叫 db::in_flight_turn() 的 SELECT，UPDATE 只用 `WHERE id=?`（Rust 層 guard，見 transitions 模組測試）"),
    ("completed_fallback", "completed", "hookrecv.rs:452 fill_or_drop_late_hook：備援關掉但還沒有回覆的回合，遲到的 hook 把答案補上；guard 是「還沒有 assistant 訊息」（Rust 層），不是 SQL 的 status guard"),
];

/// 讀 `supervisor_assignments.status` 現況時曾經發現、還沒被 #71 一起收掉的一條縫（`store.rs::
/// settle_and_notify` 送通知那句 `INSERT OR IGNORE INTO supervisor_inbox` 不看前面那句 UPDATE 的
/// `moved`，guard 擋下時仍照樣排一則「請驗收」的通知）：**已修**，`f108cd56` 關掉 #99，`moved` 為假
/// 時不再組通知、`event_new` 直接是 false；釘住的測試是
/// `supervisor::store::a_cancelled_assignment_gets_no_stale_completion_notification`。

#[cfg(test)]
mod tests {
    use super::*;

    /// 表本身要自洽：邊上出現的每一個狀態都要是 CHECK 約束宣告過的合法值，不然這張表自己就是錯的。
    #[test]
    fn every_state_in_the_tables_is_a_declared_enum_value() {
        for (from, to, why) in RUN_STATE_EDGES {
            assert!(*from == "(insert)" || RUN_STATES.contains(from), "{from} 不是合法的 runs.state：{why}");
            assert!(RUN_STATES.contains(to), "{to} 不是合法的 runs.state：{why}");
        }
        for (from, to, why) in TURN_STATUS_EDGES {
            assert!(*from == "(insert)" || TURN_STATUSES.contains(from), "{from} 不是合法的 turns.status：{why}");
            assert!(TURN_STATUSES.contains(to), "{to} 不是合法的 turns.status：{why}");
        }
    }

    /// `exited` 是終態：表上不該有任何一條從它出發的邊。這條測試釘住「這張表本身」的這個宣告，
    /// 不是程式行為——程式行為的等價測試在 `hookrecv.rs`（一顆 exited run 底下的回合不會被
    /// 遲到的 hook 救回來）。
    #[test]
    fn exited_has_no_outgoing_edge_in_the_table() {
        assert!(!RUN_STATE_EDGES.iter().any(|(from, _, _)| *from == "exited"));
    }
}
