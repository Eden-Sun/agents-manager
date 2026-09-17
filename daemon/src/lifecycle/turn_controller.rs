//! Turn 狀態轉移的**單一權威**（issue #68，第一步）。
//!
//! 現況不是沒有防護：二十來處 `UPDATE turns SET status=…` 幾乎每一句都自己帶了
//! `AND status='<from>'` 的 CAS guard。問題是那是**每個呼叫端各自記得**的約定，沒有任何一個地方
//! 說得出「哪些轉移是合法的」；新的 lifecycle 路徑（scheduler／Mission／Assignment）只要有一處忘了
//! 帶 guard，就能把一筆已經收尾的回合改回進行中，而且沒有人會知道。
//!
//! 所以這個模組做兩件事，刻意**不**做 actor rewrite、不做 event sourcing（issue 的 non-goals）：
//!
//! 1. [`LEGAL_EDGES`] 是合法邊的唯一定義，並由它**生成一句 SQLite trigger**（[`guard_ddl`]）裝在
//!    `turns` 上。走哪條路徑都一樣：HTTP、hook、timer、reconcile、scheduler，繞不過去，未來新寫的
//!    路徑也繞不過去。這跟 `runs_agent_status_since` 用 trigger 而不是「在每一處補一行」是同一個理由
//!    （db.rs：漏掉一處就會在那條路徑上悄悄跟丟）。
//! 2. [`set_status`] 是給新程式碼用的那道門：帶 CAS、擋非法邊、轉移沒發生時**講出來**而不是
//!    默默 0 rows。既有那二十來處照舊——它們的 guard 已經在 SQL 裡，trigger 是它們的下限，
//!    一次全部改寫只會在 1000 多支測試釘住的 lifecycle 上製造回歸。
//!
//! 合法邊是逐條讀生產路徑的 `UPDATE turns SET status` 得到的。`lifecycle::transitions`（issue #76）
//! 是同一批資料的**描述性**快照，兩者對不上時以這裡為準——它有 trigger 背書，跑起來會痛。
//! （已知差異：`in_flight → queued`（`queue::defer_queued_turn` 把認領過的放回佇列）在 #76 那張表上
//! 漏了；它是真的邊，漏掉它去建 trigger 會讓每一次放回佇列都爆掉。）

use anyhow::Result;
use sqlx::SqlitePool;

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

pub async fn install_guard(tx: &mut sqlx::SqliteConnection) -> Result<()> {
    sqlx::query(&guard_ddl()).execute(&mut *tx).await?;
    Ok(())
}

/// 一次轉移的結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 真的改了。
    Applied,
    /// 那一筆現在不是 `from`：別的路徑先收掉了。**不是錯誤**（重複收尾要冪等），但講出來。
    Raced { now: String },
    /// 那一筆不存在。
    Missing,
}

/// 收掉一筆回合時，`delivery` 要不要跟著動。送達與回合成敗是兩件事（§4.4a），所以要講明。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryOnFail {
    /// 不動：字送出去了，失敗的是回合（stuck watchdog、畫面判定、turn_error…）。
    Keep,
    /// 一起標成 `failed`：一個字都沒送出去，或 CLI 自己說送不出去。
    Failed,
    /// 只有原本「證不明」（`unknown`）的才改成 failed；已經確定送達的不動。
    FailedIfUnknown,
}

impl DeliveryOnFail {
    fn sql(self) -> &'static str {
        match self {
            DeliveryOnFail::Keep => "delivery",
            DeliveryOnFail::Failed => "'failed'",
            DeliveryOnFail::FailedIfUnknown => "CASE WHEN delivery='unknown' THEN 'failed' ELSE delivery END",
        }
    }
}

/// **把一筆還在飛的回合收成 failed。** `in_flight -> failed` 這條邊在生產路徑上出現十來次
/// （interrupt、abort、run 結束、stuck watchdog、畫面判定、送達失敗、退避用盡…），
/// 以前每一處各自抄一句 SQL：guard 帶不帶、`delivery` 動不動、`completed_at` 蓋不蓋都要重想一次，
/// 而且**有四處沒有帶 guard**——它們會把一筆別人已經收好的回合覆蓋成 failed。
///
/// 現在只有這一句，CAS 固定帶上，`delivery` 的三種寫法由 [`DeliveryOnFail`] 講明。
pub async fn fail(pool: &SqlitePool, turn_id: &str, delivery: DeliveryOnFail, why: &str) -> Result<Outcome> {
    let mut conn = pool.acquire().await?;
    fail_on(&mut conn, turn_id, delivery, why).await
}

/// [`fail`] 的交易內版本：要跟系統訊息綁在同一個交易裡時用。
pub async fn fail_on(conn: &mut sqlx::SqliteConnection, turn_id: &str, delivery: DeliveryOnFail, why: &str) -> Result<Outcome> {
    let sql = format!(
        "UPDATE turns SET status='failed', delivery={}, completed_at=? WHERE id=? AND status='in_flight'",
        delivery.sql()
    );
    let done = sqlx::query(&sql).bind(crate::db::now()).bind(turn_id).execute(&mut *conn).await?;
    if done.rows_affected() > 0 {
        return Ok(Outcome::Applied);
    }
    settled(&mut *conn, turn_id, "in_flight", "failed", why).await
}

/// **新的 lifecycle 路徑改 turn 狀態走這裡。** 帶 CAS、擋非法邊、轉移沒發生時講得出為什麼。
///
/// 形狀固定的那幾條（收成 failed）走 [`fail`]；這支是通用的那道門。
pub async fn set_status(pool: &SqlitePool, turn_id: &str, from: &str, to: &str, why: &str) -> Result<Outcome> {
    if !is_legal(from, to) {
        anyhow::bail!("illegal turn status transition {from} -> {to} ({why})");
    }
    let done = sqlx::query("UPDATE turns SET status=?, completed_at=CASE WHEN ? THEN ? ELSE completed_at END WHERE id=? AND status=?")
        .bind(to)
        .bind(is_terminal(to))
        .bind(crate::db::now())
        .bind(turn_id)
        .bind(from)
        .execute(pool)
        .await?;
    if done.rows_affected() > 0 {
        return Ok(Outcome::Applied);
    }
    let mut conn = pool.acquire().await?;
    settled(&mut conn, turn_id, from, to, why).await
}

/// CAS 沒打中：那一筆現在是什麼？講出來，不要讓呼叫端把「0 rows」當成「成功」。
async fn settled(conn: &mut sqlx::SqliteConnection, turn_id: &str, from: &str, to: &str, why: &str) -> Result<Outcome> {
    let now: Option<String> = sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(turn_id).fetch_optional(&mut *conn).await?;
    match now {
        Some(now) => {
            tracing::info!(turn = turn_id, %from, %to, %now, why, "turn 轉移沒發生：它已經不是預期的起點了");
            Ok(Outcome::Raced { now })
        }
        None => {
            tracing::warn!(turn = turn_id, %from, %to, why, "turn 轉移沒發生：這一筆不存在");
            Ok(Outcome::Missing)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 轉移表本身：終局不回頭，值沒變的放行，表上的狀態都是 schema 認得的。
    #[test]
    fn the_transition_table_is_self_consistent() {
        for (from, to, _) in LEGAL_EDGES {
            assert!(crate::lifecycle::transitions::TURN_STATUSES.contains(from), "{from} 不是合法狀態");
            assert!(crate::lifecycle::transitions::TURN_STATUSES.contains(to), "{to} 不是合法狀態");
            assert!(!ACTIVE.contains(to) || !is_terminal(from), "終局 {from} 不可以回到進行中的 {to}");
        }
        let outgoing = |from: &str| LEGAL_EDGES.iter().filter(|(f, _, _)| *f == from).map(|(_, t, _)| *t).collect::<Vec<_>>();
        for s in crate::lifecycle::transitions::TURN_STATUSES.iter().filter(|s| is_terminal(s)) {
            assert!(is_legal(s, s), "{s} -> {s}（冪等重寫）要放行");
            for active in ACTIVE {
                assert!(!is_legal(s, active), "{s} 不可以回到 {active}");
            }
        }
        // 真正的死路（`completed`／`failed`）一條出邊都沒有；`completed_fallback` 只有往 `completed` 那一條。
        assert!(outgoing("completed").is_empty() && outgoing("failed").is_empty(), "終局不該有出邊");
        assert_eq!(outgoing("completed_fallback"), vec!["completed"]);
        // `completed_fallback` 是唯一一個還能往前走的終局狀態——遲到的 hook 把回覆補上。
        assert!(is_legal("completed_fallback", "completed"));
        assert!(!is_legal("completed", "completed_fallback"));
        // 這條是 #76 現況表漏掉的真實邊：漏了它去建 trigger，每一次放回佇列都會爆。
        assert!(is_legal("in_flight", "queued"), "defer_queued_turn 要放得回去");
        assert!(!is_legal("queued", "completed"), "沒送出去的不可能直接完成");
    }

    #[tokio::test]
    async fn the_guard_refuses_to_resurrect_a_finished_turn() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let now = crate::db::now();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('tb',?,'tb','claude','tok',?)")
            .bind(&e.project_id)
            .bind(&now)
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO conversations (id,bot_id,created_at) VALUES ('ct','tb',?)").bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO turns (id,conversation_id,origin,status,delivery,created_at) VALUES ('t1','ct','web','in_flight','ok',?)")
            .bind(&now)
            .execute(&app.db)
            .await
            .unwrap();

        // 合法：認領過的放回佇列，以及收尾。
        sqlx::query("UPDATE turns SET status='queued' WHERE id='t1'").execute(&app.db).await.expect("in_flight -> queued 是合法的");
        sqlx::query("UPDATE turns SET status='in_flight' WHERE id='t1'").execute(&app.db).await.unwrap();
        sqlx::query("UPDATE turns SET status='completed' WHERE id='t1'").execute(&app.db).await.unwrap();

        // 非法：終局回到進行中。任何路徑都不行——擋在 DB，繞不過去。
        for bad in ["in_flight", "queued"] {
            let err = sqlx::query("UPDATE turns SET status=? WHERE id='t1'")
                .bind(bad)
                .execute(&app.db)
                .await
                .expect_err(&format!("completed -> {bad} 必須是明確的錯誤，不是 0 rows"));
            assert!(format!("{err}").contains("illegal turn status transition"), "{err}");
        }
        let still: String = sqlx::query_scalar("SELECT status FROM turns WHERE id='t1'").fetch_one(&app.db).await.unwrap();
        assert_eq!(still, "completed", "被擋下來的那一句一個欄位都沒改");
        // 冪等重寫照樣放行。
        sqlx::query("UPDATE turns SET status='completed' WHERE id='t1'").execute(&app.db).await.unwrap();
    }

    /// `set_status`：非法邊當場回錯，轉移沒發生時講得出「它現在是什麼」。
    #[tokio::test]
    async fn set_status_reports_why_a_transition_did_not_happen() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let now = crate::db::now();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('sb',?,'sb','claude','tok2',?)")
            .bind(&e.project_id)
            .bind(&now)
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO conversations (id,bot_id,created_at) VALUES ('cs','sb',?)").bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO turns (id,conversation_id,origin,status,delivery,created_at) VALUES ('s1','cs','web','in_flight','ok',?)")
            .bind(&now)
            .execute(&app.db)
            .await
            .unwrap();

        assert_eq!(set_status(&app.db, "s1", "in_flight", "failed", "測試").await.unwrap(), Outcome::Applied);
        let t: (String, Option<String>) =
            sqlx::query_as("SELECT status, completed_at FROM turns WHERE id='s1'").fetch_one(&app.db).await.unwrap();
        assert_eq!(t.0, "failed");
        assert!(t.1.is_some(), "收尾要記時間");

        // 同一步再來一次：不是錯誤，但說得出它現在是什麼（重複收尾冪等）。
        assert_eq!(
            set_status(&app.db, "s1", "in_flight", "failed", "測試").await.unwrap(),
            Outcome::Raced { now: "failed".into() },
        );
        assert_eq!(set_status(&app.db, "nope", "in_flight", "failed", "測試").await.unwrap(), Outcome::Missing);
        // 非法邊連 SQL 都不必送。
        let err = set_status(&app.db, "s1", "failed", "in_flight", "測試").await.unwrap_err();
        assert!(format!("{err}").contains("illegal turn status transition"), "{err}");
    }
}
