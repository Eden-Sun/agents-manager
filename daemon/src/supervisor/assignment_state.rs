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
//!
//! **issue #71 第二刀**：跟 `lifecycle::turn_controller`（issue #68，管 `turns.status`）補齊同一個
//! 等級的兩件事——
//!
//! 1. [`guard_ddl`] 由這張表生成一句 SQLite trigger 裝在 `supervisor_assignments` 上
//!    （[`install_guard`]，接在 `store::migrate` 裡）。非法轉移現在**繞不過去**：走 HTTP、走
//!    reconcile、走以後任何新寫的路徑都一樣，不再只靠「呼叫端記得帶 guard」這個約定。跟
//!    `turn_controller::guard_ddl` 同一個理由、同一個形狀（`WHEN OLD.status IS NOT NEW.status`——
//!    值沒變的重寫一律放行，不然重播、冪等重寫會被自己的保護擋下來）。
//! 2. [`AssignmentState`] 是明確的型別，[`set_status`]／[`set_status_on`] 是給新程式碼用的單一
//!    入口（回 [`Outcome`]，講得出轉移沒發生是因為別人先動了手還是這筆根本不存在），跟
//!    `turn_controller::set_status`／`set_status_on` 同一個形狀。既有那幾支「status 跟別的欄位一起
//!    寫在同一句」的函式（`mark_delivered`、`settle_and_notify`、`park_quota_blocked`）留著自己的
//!    整句 UPDATE——拆成兩步反而讓原子寫入變成非原子（跟 `turn_controller` 模組文件講的是同一個
//!    取捨），但它們的 `WHERE status IN (…)` 早就是從 [`sources_for`] 算出來的，不是自己抄的。

use anyhow::Result;
use sqlx::{SqlitePool, SqliteConnection};

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

/// 型別化的 `supervisor_assignments.status`。字串是給 DB／JSON 用的線上格式；新程式碼（包括
/// #74 MissionController 要呼叫的入口）比對、傳遞狀態走這個 enum——打錯字在編譯期就會發現，
/// 不用等到執行期查表才知道「這個狀態根本不存在」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AssignmentState {
    Queued,
    Delivered,
    Unknown,
    AwaitingReview,
    Blocked,
    QuotaBlocked,
    Completed,
    Failed,
    Cancelled,
    Superseded,
}

impl AssignmentState {
    /// DB／JSON 上的線上格式。跟 [`ALL`] 是同一份值，這裡只是型別化的另一面。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Delivered => "delivered",
            Self::Unknown => "unknown",
            Self::AwaitingReview => "awaiting_review",
            Self::Blocked => "blocked",
            Self::QuotaBlocked => "quota_blocked",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Superseded => "superseded",
        }
    }

    /// 從 DB 讀回來的字串解回型別。讀到不認得的字串（壞資料、將來被移除的狀態）回 `None`，
    /// 不是 panic——呼叫端自己決定要當成「找不到這筆」還是要噴錯。
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "queued" => Self::Queued,
            "delivered" => Self::Delivered,
            "unknown" => Self::Unknown,
            "awaiting_review" => Self::AwaitingReview,
            "blocked" => Self::Blocked,
            "quota_blocked" => Self::QuotaBlocked,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "superseded" => Self::Superseded,
            _ => return None,
        })
    }

    /// 終局沒有出邊——跟 [`is_terminal`] 同一份答案，這裡不重複判斷邏輯，只是型別化的一面。
    pub fn is_terminal(self) -> bool {
        is_terminal(self.as_str())
    }
}

impl std::fmt::Display for AssignmentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 由 [`allowed`] 生成的 trigger DDL：跟 `lifecycle::turn_controller::guard_ddl` 同一個形狀
/// （`WHEN OLD.status IS NOT NEW.status`——值沒變的重寫一律放行，重播、冪等重寫不該被自己的
/// 保護擋下來）。`ALL` 是封閉集合，`ALL × ALL` 只有 100 組，直接把 `allowed()` answer 是 true 的
/// 那些組成 OR 清單——表改了（`allowed` 改了）trigger 就跟著改，只有一份定義。
pub fn guard_ddl() -> String {
    let allowed_pairs = ALL
        .iter()
        .flat_map(|&from| ALL.iter().filter(move |&&to| allowed(from, to)).map(move |to| format!("(OLD.status='{from}' AND NEW.status='{to}')")))
        .collect::<Vec<_>>()
        .join("\n                 OR ");
    format!(
        "CREATE TRIGGER IF NOT EXISTS supervisor_assignments_status_transition
           BEFORE UPDATE OF status ON supervisor_assignments
           WHEN OLD.status IS NOT NEW.status
            AND NOT ({allowed_pairs})
         BEGIN
           SELECT RAISE(ABORT, 'illegal assignment status transition');
         END"
    )
}

/// 裝上 [`guard_ddl`] 那句 trigger。接在 `store::migrate` 裡，表一定已經存在之後。
///
/// DB 裡那一份跟現在的轉移表不同就換掉（issue #186）：以前 `IF NOT EXISTS` 只建一次，表改了舊 DB 永遠停在舊規則。
/// DROP 與重建在同一個交易裡，換到一半失敗不會留下沒有守衛的空窗。
pub async fn install_guard(pool: &SqlitePool) -> Result<()> {
    let mut tx = pool.begin().await?;
    crate::db::sync_trigger(&mut tx, "supervisor_assignments_status_transition", &guard_ddl()).await?;
    tx.commit().await?;
    Ok(())
}

/// 一次轉移的結果。跟 `turn_controller::Outcome` 同一個形狀：轉移沒發生時講得出為什麼，
/// 不是默默 0 rows 讓呼叫端自己猜。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 真的改了。
    Applied,
    /// 這一筆現在不是預期的 `from`：別的路徑先動了手（不是錯誤——重複收尾本來就該冪等），
    /// 附上它現在真正的值。
    Raced { now: String },
    /// 這一筆不存在。
    Missing,
}

/// **新程式碼改 assignment 的 `status` 走這裡。** 帶 CAS、擋非法邊、轉移沒發生時講得出為什麼。
///
/// `from == to` 一律放行（重播、冪等重寫，跟 `turn_controller::set_status` 同一個理由）；其餘
/// 轉移合不合法問 [`allowed`]——那張表才是「這一步准不准」的權威，這裡不重複判斷一次。
/// 既有那幾支「status 跟別的欄位一起寫」的函式（見模組文件）繼續用自己的整句 UPDATE，但一樣
/// 從 [`sources_for`] 算 guard，跟這裡問的是同一張表。
pub async fn set_status(pool: &SqlitePool, id: &str, from: AssignmentState, to: AssignmentState, why: &str) -> Result<Outcome> {
    let mut conn = pool.acquire().await?;
    set_status_on(&mut conn, id, from, to, why).await
}

/// [`set_status`] 的交易內版本：要跟同一個交易裡的其他寫入綁在一起時用。
pub async fn set_status_on(conn: &mut SqliteConnection, id: &str, from: AssignmentState, to: AssignmentState, why: &str) -> Result<Outcome> {
    if from != to && !allowed(from.as_str(), to.as_str()) {
        anyhow::bail!("illegal assignment status transition {from} -> {to} ({why})");
    }
    let done = sqlx::query("UPDATE supervisor_assignments SET status=?, updated_at=? WHERE id=? AND status=?")
        .bind(to.as_str())
        .bind(crate::db::now())
        .bind(id)
        .bind(from.as_str())
        .execute(&mut *conn)
        .await?;
    if done.rows_affected() > 0 {
        return Ok(Outcome::Applied);
    }
    let now: Option<String> = sqlx::query_scalar("SELECT status FROM supervisor_assignments WHERE id=?").bind(id).fetch_optional(&mut *conn).await?;
    match now {
        Some(now) => {
            tracing::info!(assignment = id, %from, %to, %now, why, "assignment 轉移沒發生：它已經不是預期的起點了");
            Ok(Outcome::Raced { now })
        }
        None => {
            tracing::warn!(assignment = id, %from, %to, why, "assignment 轉移沒發生：這一筆不存在");
            Ok(Outcome::Missing)
        }
    }
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

    /// `AssignmentState` 跟字串是同一件事的兩面：來回轉一定要對得回去，認不得的字串要老實說不知道。
    #[test]
    fn assignment_state_round_trips_through_its_string() {
        for s in ALL {
            let parsed = AssignmentState::parse(s).unwrap_or_else(|| panic!("{s} 應該解得回來"));
            assert_eq!(parsed.as_str(), s);
            assert_eq!(parsed.to_string(), s);
        }
        assert!(AssignmentState::parse("nonsense").is_none());
        assert_eq!(AssignmentState::Completed.is_terminal(), is_terminal("completed"));
        assert_eq!(AssignmentState::Queued.is_terminal(), is_terminal("queued"));
    }

    use sqlx::sqlite::SqlitePoolOptions;

    async fn pool() -> SqlitePool {
        let p = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        crate::supervisor::store::migrate(&p).await.unwrap();
        p
    }

    async fn row(p: &SqlitePool, crid: &str) -> String {
        crate::supervisor::store::insert_assignment(p, None, "bot1", crid, "x", &[], None, true).await.unwrap().id
    }

    /// `set_status`：非法邊當場回錯（連 SQL 都不必送），轉移沒發生時講得出「它現在是什麼」，
    /// 值不變的重寫（重播）照樣放行。跟 `turn_controller::set_status_reports_why_a_transition_did_not_happen`
    /// 同一個形狀。
    #[tokio::test]
    async fn set_status_reports_why_a_transition_did_not_happen() {
        let p = pool().await;
        let id = row(&p, "c1").await;

        assert_eq!(set_status(&p, &id, AssignmentState::Queued, AssignmentState::Delivered, "測試").await.unwrap(), Outcome::Applied);
        let now: String = sqlx::query_scalar("SELECT status FROM supervisor_assignments WHERE id=?").bind(&id).fetch_one(&p).await.unwrap();
        assert_eq!(now, "delivered");

        // 別的路徑先動了手：不是錯誤，但講得出它現在是什麼。
        assert_eq!(
            set_status(&p, &id, AssignmentState::Queued, AssignmentState::Delivered, "測試").await.unwrap(),
            Outcome::Raced { now: "delivered".into() },
        );
        assert_eq!(set_status(&p, "nope", AssignmentState::Queued, AssignmentState::Delivered, "測試").await.unwrap(), Outcome::Missing);

        // 非法邊連 SQL 都不必送。
        let err = set_status(&p, &id, AssignmentState::Completed, AssignmentState::Delivered, "測試").await.unwrap_err();
        assert!(format!("{err}").contains("illegal assignment status transition"), "{err}");

        // 值不變的重寫（重播）一律放行，不查表。
        assert_eq!(set_status(&p, &id, AssignmentState::Delivered, AssignmentState::Delivered, "重播").await.unwrap(), Outcome::Applied);
    }

    /// trigger 是這張表的下限：不管走哪條路，非法轉移都繞不過去——包括完全跳過 Rust 這一層、
    /// 直接下 SQL 的呼叫端（跟 `turn_controller::the_guard_refuses_to_resurrect_a_finished_turn` 同一個形狀）。
    #[tokio::test]
    async fn the_guard_refuses_to_resurrect_a_settled_assignment() {
        let p = pool().await;
        let id = row(&p, "c1").await;

        // 合法：派出去、結案。
        sqlx::query("UPDATE supervisor_assignments SET status='delivered' WHERE id=?").bind(&id).execute(&p).await.expect("queued -> delivered 合法");
        sqlx::query("UPDATE supervisor_assignments SET status='completed' WHERE id=?").bind(&id).execute(&p).await.expect("delivered -> completed 合法");

        // 非法：終局回到在途。任何路徑都不行——擋在 DB，繞不過 Rust 這一層。
        for bad in ["delivered", "queued", "awaiting_review"] {
            let err = sqlx::query("UPDATE supervisor_assignments SET status=? WHERE id=?")
                .bind(bad)
                .bind(&id)
                .execute(&p)
                .await
                .expect_err(&format!("completed -> {bad} 必須是明確的錯誤，不是 0 rows"));
            assert!(format!("{err}").contains("illegal assignment status transition"), "{err}");
        }
        let still: String = sqlx::query_scalar("SELECT status FROM supervisor_assignments WHERE id=?").bind(&id).fetch_one(&p).await.unwrap();
        assert_eq!(still, "completed", "被擋下來的那一句一個欄位都沒改");

        // 值不變的重寫（冪等收尾）照樣放行——trigger 只管真的改變值的那一句。
        sqlx::query("UPDATE supervisor_assignments SET status='completed' WHERE id=?").bind(&id).execute(&p).await.expect("冪等重寫不該被擋");
    }

    /// issue #186：守衛 trigger 的內容由轉移表產生，以前用 `CREATE TRIGGER IF NOT EXISTS` 建——轉移表改了之後，舊 DB
    /// 裡那一份已經存在，永遠不會換：新增的合法邊被舊守衛擋下、拿掉的邊照樣放行。turns 那一個（#68，`turn_controller`）同一個做法。
    /// 只在測試的暫存 DB 驗：開一次、換成舊版的守衛、關掉重開（＝升級後的 daemon 開同一個 DB）。
    #[tokio::test]
    async fn guards_left_by_an_older_build_are_replaced_when_the_db_is_opened() {
        let dir = std::env::temp_dir().join(format!("agm-guard-refresh-{}", crate::db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db.sqlite3");
        let p = crate::db::open(&path).await.unwrap();
        // 舊版留下的守衛：轉移表還沒有 → quota_blocked 那幾條（現在合法的邊被擋），turns 那張還沒有 → in_flight。
        for (name, stale) in [
            (
                "supervisor_assignments_status_transition",
                "CREATE TRIGGER supervisor_assignments_status_transition BEFORE UPDATE OF status ON supervisor_assignments
                   WHEN OLD.status IS NOT NEW.status AND NEW.status = 'quota_blocked'
                 BEGIN SELECT RAISE(ABORT, 'stale assignment guard'); END",
            ),
            (
                "turns_status_transition",
                "CREATE TRIGGER turns_status_transition BEFORE UPDATE OF status ON turns
                   WHEN OLD.status IS NOT NEW.status AND NEW.status = 'in_flight'
                 BEGIN SELECT RAISE(ABORT, 'stale turn guard'); END",
            ),
        ] {
            // 同一條連線、同一個交易：DROP 與 CREATE 不能落在連線池裡兩條不同的連線上。
            let mut tx = p.begin().await.unwrap();
            sqlx::query(&format!("DROP TRIGGER {name}")).execute(&mut *tx).await.unwrap();
            sqlx::query(stale).execute(&mut *tx).await.unwrap();
            tx.commit().await.unwrap();
        }
        p.close().await;

        let p = crate::db::open(&path).await.unwrap();
        // SQLite 存的是去掉 `IF NOT EXISTS` 的原文。
        let current = |ddl: String| ddl.replacen("CREATE TRIGGER IF NOT EXISTS ", "CREATE TRIGGER ", 1);
        for (name, want) in [
            ("supervisor_assignments_status_transition", current(guard_ddl())),
            ("turns_status_transition", current(crate::lifecycle::turn_controller::guard_ddl())),
        ] {
            let have: Option<String> = sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE type='trigger' AND name=?").bind(name).fetch_optional(&p).await.unwrap();
            assert_eq!(have.as_deref(), Some(want.as_str()), "{name}：重開之後守衛要等於現在的轉移表");
        }
        // 行為上也是現在這一份：表上合法的邊走得通，終局照樣回不去。
        let id = row(&p, "g1").await;
        sqlx::query("UPDATE supervisor_assignments SET status='quota_blocked' WHERE id=?").bind(&id).execute(&p).await.expect("queued -> quota_blocked 現在是合法的");
        sqlx::query("UPDATE supervisor_assignments SET status='cancelled' WHERE id=?").bind(&id).execute(&p).await.unwrap();
        let err = sqlx::query("UPDATE supervisor_assignments SET status='queued' WHERE id=?").bind(&id).execute(&p).await.expect_err("終局不能回頭");
        assert!(format!("{err}").contains("illegal assignment status transition"), "{err}");
        p.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 表跟 trigger 讀的是同一份 `allowed()`：合法的邊（同一份清單 `sources_for` 已經釘過）走 SQL
    /// 一樣通，不會因為多了一層 trigger 就變嚴。
    #[tokio::test]
    async fn the_guard_permits_every_edge_the_table_calls_legal() {
        let p = pool().await;
        for (from, to) in [("queued", "blocked"), ("delivered", "blocked"), ("queued", "quota_blocked"), ("quota_blocked", "queued")] {
            let id = row(&p, &format!("c-{from}-{to}")).await;
            if from != "queued" {
                sqlx::query("UPDATE supervisor_assignments SET status=? WHERE id=?").bind(from).bind(&id).execute(&p).await.unwrap();
            }
            sqlx::query("UPDATE supervisor_assignments SET status=? WHERE id=?")
                .bind(to)
                .bind(&id)
                .execute(&p)
                .await
                .unwrap_or_else(|e| panic!("{from} -> {to} 表上是合法的，trigger 不該擋：{e}"));
        }
    }
}
