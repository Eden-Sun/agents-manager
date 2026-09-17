//! `runs.state` / `turns.status` 轉移現況表（issue #76）。
//!
//! 這不是理想模型，是從現有程式碼逐條 grep `UPDATE runs SET state` / `UPDATE turns SET status`
//! 讀出來、只收生產路徑（排掉 `#[cfg(test)]`）的結果，2026-09-18 讀的一份快照。程式碼才是準：
//! 這張表過時了就改表，不要改程式碼去配合表。每條邊後面的 `file:line` 指到實際下手的那一句，
//! 遇到疑點自己去核對。
//!
//! `supervisor_assignments.status` 沒有 DB 層的 CHECK 約束（純字串）。決定合法值與 AGM 裁示轉移的
//! 是 `daemon/src/supervisor/store.rs` 的 `decision_status()`（accept/fail/cancel/block/followup 對應
//! completed/failed/cancelled/blocked/superseded）、`review_with_followup()`（CAS 寫入）與
//! `EXECUTING_STATES`/`OPEN_STATES`/`STALLED_STATES` 三個常數——這裡的 `ASSIGNMENT_STATUS_EDGES`
//! 是把它們**逐條讀出來**的快照，不是另一份定義，過時了改這張表就好，別去動那三個常數。
//!
//! 目前只有這個檔案自己的測試在讀這些表（`cargo clippy` 不編 `#[cfg(test)]`，所以看起來是
//! death code）；`#[allow(dead_code)]` 是因為它們的價值是「給人看、給以後可能出現的
//! TurnController（issue #69）當起點」，不是現在就要被生產程式碼呼叫。

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
    ("starting", "running", "reconcile.rs:344,390 CASE WHEN…THEN 'running'（在對應的 herdr agent 裡看到了）；lifecycle/start.rs:731 set_run（agent_status 探測回來）"),
    ("stopping", "running", "reconcile.rs:344,390 同一條 CASE WHEN；lifecycle/start.rs:870 guarded UPDATE（停止被打斷，bot 又動了）"),
    ("starting", "stopping", "lifecycle/start.rs:841、lifecycle/stop.rs:36（使用者主動停止；SQL 本身沒有 guard 限定 FROM，呼叫端只在該轉的時候才呼叫）"),
    ("running", "stopping", "同上"),
    ("stopping", "stopped", "lifecycle/start.rs:877、lifecycle/stop.rs:72"),
    ("stopped", "exited", "lifecycle/start.rs:781（唯一在 SQL 本身就 guard `AND state='stopped'` 的一條）"),
    ("starting", "exited", "lifecycle/queue.rs:551 mark_run_exited（Rust 層 guard：state IN (starting,running,stopping) 才動手）；lifecycle/start.rs:182（啟動失敗）"),
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

/// `supervisor_assignments.status` 沒有 CHECK 約束，合法值集合抄自
/// `store.rs::OPEN_STATES`／`EXECUTING_STATES`（開放中的 6 種）＋ 4 種終態。
pub const ASSIGNMENT_STATUSES: [&str; 10] =
    ["queued", "delivered", "unknown", "awaiting_review", "blocked", "quota_blocked", "completed", "failed", "cancelled", "superseded"];

/// 觀察到的 `supervisor_assignments.status` 合法邊。四個終態（completed/failed/cancelled/superseded）
/// 沒有出邊：`api.rs:258 post_review` 一開始就擋「`!a.is_open()` → 409 already_closed」，唯一的出路是
/// `followup` 開一件新的續作，不是原地轉移。
///
/// **這裡也記兩個看起來會發生、但明顯不對的縫（已回報 AGM，故意沒有寫測試去釘成「正確行為」）：**
/// 1. `store.rs::mark_delivered`（約 1197 行）完全沒有 SQL 狀態 guard（`WHERE id=?`，不檢查現在的
///    status）。`controller.rs::dispatch`（約 90 行）只在**進入時**讀一次 `a.status != "queued"`，
///    中間經過好幾個 await 點（含實際送 prompt 進 pane），若 AGM 這段時間內對同一筆下了 `cancel`
///    （`post_review` 容許 cancel 在 executing 狀態下生效），`mark_delivered` 最後仍會把
///    `status` 蓋回 `delivered`／`unknown`，且不會動到已經寫下的 `review_decision='cancel'` 等欄位
///    ——結果是一列 `status` 說「還在跑」但審查欄位說「已經取消」的自相矛盾列。
/// 2. `store.rs::settle_and_notify`（約 1316 行）本身的 `status` 欄位有正確 guard（見下面
///    `queued|delivered|unknown → awaiting_review` 那條的 CAS），但它「送通知」那句
///    `INSERT OR IGNORE INTO supervisor_inbox` 不看 `moved`，guard 沒擋下（`moved=false`，例如
///    這一列已經被 cancel）時照樣排一則 `assignment_completed`／`needs_review:true` 的通知——
///    已經關掉的交辦還會讓 AGM 收到「請驗收」的訊息。
pub const ASSIGNMENT_STATUS_EDGES: &[(&str, &str, &str)] = &[
    ("(insert)", "queued", "supervisor/mod.rs::assign() 建交辦的初始值"),
    ("queued", "delivered", "store.rs:1200 mark_delivered（delivery != 'unknown' 時）；UPDATE 沒有 SQL 狀態 guard，見上面「看起來會發生但不對」第 1 點"),
    ("queued", "unknown", "同上，delivery == 'unknown' 時"),
    ("delivered", "blocked", "store.rs:1409 block_stale_queue_tx，CAS guard `AND status='delivered'`（排隊送出去太久沒消息的保險絲）"),
    ("queued", "quota_blocked", "store.rs:1433 park_quota_blocked，CAS guard `AND status IN ('queued','delivered','unknown')`"),
    ("delivered", "quota_blocked", "同上"),
    ("unknown", "quota_blocked", "同上"),
    ("quota_blocked", "queued", "store.rs:1481 resume_quota_blocked，CAS guard `AND status='quota_blocked'`（額度回來了，重新排隊）"),
    ("queued", "awaiting_review", "store.rs:1338 settle_and_notify，CAS guard `AND status IN ('queued','delivered','unknown')`（回合結束、還要驗收）"),
    ("delivered", "awaiting_review", "同上"),
    ("unknown", "awaiting_review", "同上；見 `a_cancelled_assignment_is_not_resurrected_by_a_late_settle` 測試"),
    ("queued", "completed", "同一句 settle_and_notify：`expects_review=0`（notice）且回合正常結束時 `CASE WHEN` 直接落地 completed，不經過 awaiting_review"),
    ("delivered", "completed", "同上"),
    ("unknown", "completed", "同上"),
    ("awaiting_review", "completed", "api.rs:351 post_review → store.rs:1693 review_with_followup（guarded UPDATE at 1695），decision=accept，CAS guard `AND status=<呼叫端讀到的舊值>`"),
    ("awaiting_review", "failed", "同上，decision=fail"),
    ("awaiting_review", "blocked", "同上，decision=block"),
    ("awaiting_review", "superseded", "同上，decision=followup（同時在同一交易建續作，見 `a_follow_up_is_a_new_assignment_linked_to_the_old_one`）"),
    ("blocked", "completed", "同上；`blocked` 是 OPEN 但不是 EXECUTING，accept/fail/block/followup 都容許"),
    ("blocked", "failed", "同上"),
    ("blocked", "superseded", "同上"),
    ("quota_blocked", "completed", "同上；`quota_blocked` 一樣是 OPEN 非 EXECUTING"),
    ("quota_blocked", "failed", "同上"),
    ("quota_blocked", "superseded", "同上"),
    ("queued", "cancelled", "api.rs:272 post_review 對 executing 狀態只放行 cancel → review_with_followup，CAS guard"),
    ("delivered", "cancelled", "同上"),
    ("unknown", "cancelled", "同上"),
    ("awaiting_review", "cancelled", "同上（非 executing 狀態的 cancel 走一般路徑）"),
    ("blocked", "cancelled", "同上"),
    ("quota_blocked", "cancelled", "同上"),
];

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
        for (from, to, why) in ASSIGNMENT_STATUS_EDGES {
            assert!(*from == "(insert)" || ASSIGNMENT_STATUSES.contains(from), "{from} 不是合法的 assignment status：{why}");
            assert!(ASSIGNMENT_STATUSES.contains(to), "{to} 不是合法的 assignment status：{why}");
        }
    }

    /// 四個終態沒有出邊：表上一旦出現以它們當起點的邊就是這張表自己錯了——程式行為的等價測試
    /// 是 `store.rs::post_review` 對 `!is_open()` 一律回 409（`already_closed`），已經有既有覆蓋
    /// （`a_follow_up_is_a_new_assignment_linked_to_the_old_one` 等）。
    #[test]
    fn closed_assignment_statuses_have_no_outgoing_edge_in_the_table() {
        const CLOSED: [&str; 4] = ["completed", "failed", "cancelled", "superseded"];
        for (from, _, why) in ASSIGNMENT_STATUS_EDGES {
            assert!(!CLOSED.contains(from), "{from} 是終態，不該有出邊：{why}");
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
