//! `turns.status` 轉移守衛（issue #68）：合法邊的唯一定義，以及由它生成、裝進 DB 的 trigger。
//!
//! 這是資料庫的不變條件（終局的回合不可能被改回進行中），所以住在 db 這一側；`lifecycle::turn_controller` 只是用它的呼叫端
//! （並 re-export 這裡的名字，既有的 `turn_controller::{LEGAL_EDGES, is_legal, …}` 路徑不變）。

use anyhow::Result;

/// 還在進行中的狀態。其餘（`completed`／`completed_fallback`／`failed`）就是「不再進行中」——
/// 收尾時蓋 `completed_at`，而且**不可以回到這兩個之一**。
///
/// 只列 active 這一邊，不另外再列一份 terminal：兩張表遲早會有人只改一邊。
/// 注意 `completed_fallback` 雖然不再進行中，卻**不是死路**——§4.3 的終端備援先把回合關掉，
/// 遲到的 hook 還能把它升級成 `completed`（見 [`LEGAL_EDGES`]）。
pub const ACTIVE: [&str; 2] = ["queued", "in_flight"];

/// 生產路徑上真的存在的 `turns.status` 轉移邊。`(from, to, 誰在做)`。
///
/// 值本身相同的寫入（`x -> x`）一律放行：重播、冪等重寫不該被擋。
pub const LEGAL_EDGES: &[(&str, &str, &str)] = &[
    ("queued", "in_flight", "queue::flush_queued_locked 認領排隊的那一筆"),
    ("queued", "failed", "空白 prompt 丟棄／交辦撤銷／退避用盡"),
    ("in_flight", "queued", "queue::defer_queued_turn：認領過但還沒打字，放回佇列等下一次"),
    ("in_flight", "completed", "hook 收尾（hookrecv）"),
    ("in_flight", "completed_fallback", "§4.3 終端快照備援（poller）"),
    ("in_flight", "failed", "interrupt／run 結束／stuck watchdog／送達失敗"),
    ("completed_fallback", "completed", "遲到的 hook 把備援關掉的那一筆補上回覆"),
];

/// 不再進行中。`TERMINAL` 與 `ACTIVE` 是同一件事的兩面，這裡用 `ACTIVE` 定義，
/// 新增狀態時只要漏掉一邊就會在 `the_transition_table_is_self_consistent` 轉紅。
pub fn is_terminal(status: &str) -> bool {
    !ACTIVE.contains(&status)
}

/// 這一步合不合法。相同值放行（冪等重寫）。
pub fn is_legal(from: &str, to: &str) -> bool {
    from == to || LEGAL_EDGES.iter().any(|(f, t, _)| *f == from && *t == to)
}

/// 由 [`LEGAL_EDGES`] 生成的 trigger DDL。表改了 DDL 就跟著改——只有一份定義。
///
/// `BEFORE UPDATE OF status`：只有把 `status` 放進 SET 的語句會觸發；值沒變的不管。
/// `RAISE(ABORT)` 會讓整句 UPDATE 失敗——這正是要的：非法轉移要是**明確的錯誤**，
/// 不是「0 rows，沒人發現」。
pub fn guard_ddl() -> String {
    let allowed = LEGAL_EDGES
        .iter()
        .map(|(f, t, _)| format!("(OLD.status='{f}' AND NEW.status='{t}')"))
        .collect::<Vec<_>>()
        .join("\n                 OR ");
    format!(
        "CREATE TRIGGER IF NOT EXISTS turns_status_transition BEFORE UPDATE OF status ON turns
           WHEN OLD.status IS NOT NEW.status
            AND NOT ({allowed})
         BEGIN
           SELECT RAISE(ABORT, 'illegal turn status transition');
         END"
    )
}

/// 內容跟 DB 裡那一份不同就換掉（issue #186）：轉移表改了，舊 DB 的守衛要跟著換。
pub async fn install_guard(tx: &mut sqlx::SqliteConnection) -> Result<()> {
    super::sync_trigger(tx, "turns_status_transition", &guard_ddl()).await
}
