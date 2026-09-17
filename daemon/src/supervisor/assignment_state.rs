//! 交辦的狀態機（issue #71）：**一份明確的合法轉移表**，還有「哪些狀態是終局」這一個問題的唯一答案。
//!
//! 這張表不是設計出來的，是把 `store.rs` 現在真的在做的事讀出來寫成的（schema 上 `status` 那段註解
//! 就是它的散文版：在途是 `queued | delivered | unknown`，回合結束推到 `awaiting_review`，只有 AGM
//! 明確裁示才走得到 `completed | failed | cancelled | superseded`，而 `blocked` 是一種「還開著」的裁示）。
//!
//! 大部分轉移**本來就有**期望狀態的 CAS 守著（`... WHERE id=? AND status='queued'` 之類），那些照舊；
//! 這裡補的是沒有守衛的那條，以及把「終局不能回頭」變成一個查得到、測得到的規則，而不是散在十個
//! `WHERE` 子句裡的預設。
//!
//! 跟 #68 的 TurnController 的分界：這裡只管**交辦**的 `status`。回合自己的狀態（`turn_status`、
//! `delivery`）是傳輸事實，不是裁示，仍舊各走各的——schema 註解講得很清楚：「回合結束了」跟
//! 「這份工作被接受了」不是同一個主張。

/// 終局：AGM 裁示過了，這筆交辦不會再動。**終局沒有任何出邊**——issue #71 要擋的就是
/// `completed → delivered`、`cancelled → awaiting_review` 這種把結案的工作弄活過來的轉移。
pub const TERMINAL: [&str; 4] = ["completed", "failed", "cancelled", "superseded"];

/// 還在途中：已經派出去、還沒有結果。`delivered` 與 `unknown` 的差別是「送到了」與「不知道送到沒」，
/// 兩個都還在跑，所以都能再被記一次送達（同一個 crid 重派是冪等的）。
pub const IN_FLIGHT: [&str; 3] = ["queued", "delivered", "unknown"];

pub fn is_terminal(status: &str) -> bool {
    TERMINAL.contains(&status)
}

/// AGM 的裁示會把交辦推到哪個狀態（`store::decision_status` 的值域）。裁示可以從任何**還沒結案**的
/// 狀態下達——使用者隨時可以取消，AGM 隨時可以擋下來。
fn is_decision(status: &str) -> bool {
    is_terminal(status) || status == "blocked"
}

/// `from → to` 合不合法。表以外的一律不合法（預設關閉）。
///
/// 每一條邊都對得上 `store.rs` 裡一支真的函式，不是憑空補的：
///
/// | 邊 | 誰做的 |
/// |---|---|
/// | `queued → delivered \| unknown` | `mark_delivered`（派出去了） |
/// | `delivered \| unknown → delivered \| unknown` | 同上，重派同一個 crid 再記一次（冪等） |
/// | `queued → blocked` | `mark_undeliverable`（一直進不去那顆 bot） |
/// | `delivered → blocked` | `block_stale_queue`（排太久沒送出的保險絲） |
/// | 在途 → `awaiting_review \| completed` | `settle`（回合結束；`expects_review=0` 的通知當場結案） |
/// | 在途 → `quota_blocked` | `park_quota_blocked` |
/// | `quota_blocked → queued` | `resume_quota_blocked`（額度回來，重送次數 +1） |
/// | 任何未結案 → 裁示狀態 | `review_with_followup`（accept／fail／cancel／block／followup） |
pub fn allowed(from: &str, to: &str) -> bool {
    // 終局沒有出邊。這一行就是這張表存在的理由。
    if is_terminal(from) {
        return false;
    }
    // 裁示可以從任何還沒結案的狀態下達。
    if is_decision(to) {
        return true;
    }
    match (from, to) {
        // 派出去了。`queued` 是第一次；`delivered`／`unknown` 之間互相走，是重派同一個 crid
        // 再記一次（冪等），所以自己到自己也是一條**真的**邊，不是靠「原地不動一律放行」放進來的。
        (f, "delivered" | "unknown") if IN_FLIGHT.contains(&f) => true,
        // 回合結束。
        (f, "awaiting_review") if IN_FLIGHT.contains(&f) => true,
        // 等額度。
        (f, "quota_blocked") if IN_FLIGHT.contains(&f) => true,
        // 額度回來了，重新排隊。
        ("quota_blocked", "queued") => true,
        _ => false,
    }
}

/// 全部狀態，`sources_for` 掃這一份。
pub const ALL: [&str; 10] = [
    "queued",
    "delivered",
    "unknown",
    "awaiting_review",
    "blocked",
    "quota_blocked",
    "completed",
    "failed",
    "cancelled",
    "superseded",
];

/// 能合法走到 `to` 的所有來源狀態。
///
/// 這支存在的意義是：SQL 的守衛（`... WHERE status IN (…)`）**從表算出來**，而不是各自把清單抄一次。
/// 以前 `settle` 與 `park_quota_blocked` 各自硬寫著 `('queued','delivered','unknown')`，抄對了是運氣好；
/// 現在改表就等於改守衛，抄不走鐘。
pub fn sources_for(to: &str) -> Vec<&'static str> {
    ALL.iter().copied().filter(|from| allowed(from, to)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// issue #71 點名的兩條：結案的工作不可以被弄活過來。
    #[test]
    fn a_settled_assignment_never_goes_back_to_work() {
        assert!(!allowed("completed", "delivered"), "completed → delivered");
        assert!(!allowed("cancelled", "awaiting_review"), "cancelled → awaiting_review");
        for from in TERMINAL {
            for to in ["queued", "delivered", "unknown", "awaiting_review", "quota_blocked", "blocked"] {
                assert!(!allowed(from, to), "{from} → {to} 不該放行");
            }
            // 連換一個終局都不行：裁示只下一次。
            for to in TERMINAL {
                if to != from {
                    assert!(!allowed(from, to), "{from} → {to} 不該放行");
                }
            }
        }
    }

    /// 現況真的在走的那些邊都要還在——這張表是讀出來的，不是新規矩。
    #[test]
    fn the_paths_the_code_actually_takes_are_all_legal() {
        for to in ["delivered", "unknown"] {
            assert!(allowed("queued", to), "mark_delivered");
            assert!(allowed("delivered", to), "重派同一個 crid 再記一次");
            assert!(allowed("unknown", to));
        }
        assert!(allowed("queued", "blocked"), "mark_undeliverable");
        assert!(allowed("delivered", "blocked"), "block_stale_queue");
        for from in IN_FLIGHT {
            assert!(allowed(from, "awaiting_review"), "settle: {from}");
            assert!(allowed(from, "completed"), "settle 通知當場結案: {from}");
            assert!(allowed(from, "quota_blocked"), "park: {from}");
        }
        assert!(allowed("quota_blocked", "queued"), "resume");
        // 裁示從任何還沒結案的狀態都下得了。
        for from in ["queued", "delivered", "unknown", "awaiting_review", "blocked", "quota_blocked"] {
            for to in ["completed", "failed", "cancelled", "superseded", "blocked"] {
                assert!(allowed(from, to), "裁示 {from} → {to}");
            }
        }
    }

    /// 表以外的一律擋掉：預設是關閉的，不是開放的。
    #[test]
    fn everything_outside_the_table_is_refused() {
        assert!(!allowed("awaiting_review", "delivered"), "驗收中不會自己變回在途");
        assert!(!allowed("awaiting_review", "queued"));
        assert!(!allowed("blocked", "delivered"), "擋下來的要重新裁示，不是自己跑回去");
        assert!(!allowed("quota_blocked", "delivered"), "額度回來只會回到 queued");
        assert!(!allowed("delivered", "queued"), "沒有回頭路：重排是 resume 才有的事");
        assert!(!allowed("queued", "nonsense"));
        assert!(!allowed("nonsense", "queued"));
    }

    /// 自己到自己只在真的會重寫的地方成立，不是一條通用的後門。
    #[test]
    fn only_the_in_flight_states_may_be_recorded_again() {
        for s in ["delivered", "unknown"] {
            assert!(allowed(s, s), "{s} → {s}：重派同一個 crid 會再記一次");
        }
        for s in ["queued", "awaiting_review", "quota_blocked"] {
            assert!(!allowed(s, s), "{s} → {s} 不是任何一支函式在做的事");
        }
        // `blocked → blocked` 走的是「裁示」那條，不是重寫。
        for s in TERMINAL {
            assert!(!allowed(s, s), "{s} 已經結案，連再寫一次都不該經過轉移");
        }
    }

    /// SQL 的守衛是從表算出來的，所以這裡順便釘住那三組實際用到的來源清單。
    #[test]
    fn the_sql_guards_come_out_of_the_table() {
        assert_eq!(sources_for("delivered"), vec!["queued", "delivered", "unknown"]);
        assert_eq!(sources_for("unknown"), vec!["queued", "delivered", "unknown"]);
        assert_eq!(sources_for("awaiting_review"), vec!["queued", "delivered", "unknown"]);
        assert_eq!(sources_for("quota_blocked"), vec!["queued", "delivered", "unknown"]);
        // 終局永遠不在來源裡——守衛因此自動擋掉「結案的又活過來」。
        for to in ["delivered", "unknown", "awaiting_review", "quota_blocked"] {
            for t in TERMINAL {
                assert!(!sources_for(to).contains(&t), "{t} 不該能走到 {to}");
            }
        }
    }

    #[test]
    fn terminal_is_exactly_the_four_decision_outcomes() {
        assert!(is_terminal("completed") && is_terminal("failed") && is_terminal("cancelled") && is_terminal("superseded"));
        // `blocked` 是「還開著」的裁示，不是終局——它仍在 `OPEN_STATES` 裡。
        assert!(!is_terminal("blocked"));
        for s in crate::supervisor::store::OPEN_STATES {
            assert!(!is_terminal(s), "{s} 在 OPEN_STATES 裡，不可能是終局");
        }
    }
}
