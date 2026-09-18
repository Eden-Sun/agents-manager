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
//! **status 跟別的欄位在同一句寫的那五種形狀**（issue #125）：#68 刻意沒搬的五處——拆成「先改 status、
//! 再寫其他欄位」兩句會把原本一句的原子寫入變成兩步，比抄一句 SQL 更糟。所以不包進通用 mutator，
//! 而是每一種形狀各一支、**自己擁有整句 UPDATE**（CAS、`completed_at`、`delivery` 的寫法、世代前提都在句子裡）：
//! - [`complete_with_native_evidence`]／[`fail_with_native_evidence`]：hook 收尾與 `StopFailure`，同一句寫
//!   `native_session_id`／`native_turn_id`（去重鑰匙）。要圍籬放行的證明（[`super::fence::Admitted`]），
//!   而且只動還掛在那一代 run 上的回合。
//! - [`claim_queued`]：認領排隊的那一筆，同一句掛上 run——只認領到**此刻還在跑**的 run 上。
//! - [`return_to_queue`]：認領過但沒打字，同一句拔掉 run、`flush_retries+1`、排下一次。
//! - [`retract_queued`]：撤銷還在排隊的，同一句寫 `delivery='failed'`、清掉 `next_flush_at`。
//!
//! 生產路徑上 `UPDATE turns SET status` 現在只出現在這個模組裡；trigger 仍是最後一道防線，
//! 但每一支都先對 [`LEGAL_EDGES`] 檢查自己那條邊，非法的根本不會送出 SQL。
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
    /// 那一筆還在起點，但這個形狀的前提不成立（不是圍籬放行的那一代、要認領的 run 已經不在跑）。
    Fenced(&'static str),
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
    // `completed_at` 用 COALESCE：收尾時間只記第一次。這一條路的起點永遠是 `in_flight`（還沒收尾、
    // 欄位是 NULL），所以行為跟直接蓋一樣；但規則跟 `set_status` 一致，之後不會有人各寫各的。
    let sql = format!(
        "UPDATE turns SET status='failed', delivery={}, completed_at=COALESCE(completed_at, ?) WHERE id=? AND status='in_flight'",
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
    let mut conn = pool.acquire().await?;
    set_status_on(&mut conn, turn_id, from, to, why).await
}

/// [`set_status`] 的交易內版本：要跟同一個交易裡的其他寫入綁在一起時用。
///
/// `completed_at` 一律 `COALESCE(completed_at, ?)`——收尾時間**只記第一次**。遲到的 hook 把備援
/// 關掉的那一筆升級成 `completed` 時，回合真正結束的時間是備援那一刻，不是 hook 到達的這一刻。
pub async fn set_status_on(
    conn: &mut sqlx::SqliteConnection,
    turn_id: &str,
    from: &str,
    to: &str,
    why: &str,
) -> Result<Outcome> {
    if !is_legal(from, to) {
        anyhow::bail!("illegal turn status transition {from} -> {to} ({why})");
    }
    let done = sqlx::query(
        "UPDATE turns SET status=?, completed_at=CASE WHEN ? THEN COALESCE(completed_at, ?) ELSE completed_at END
          WHERE id=? AND status=?",
    )
    .bind(to)
    .bind(is_terminal(to))
    .bind(crate::db::now())
    .bind(turn_id)
    .bind(from)
    .execute(&mut *conn)
    .await?;
    if done.rows_affected() > 0 {
        return Ok(Outcome::Applied);
    }
    settled(&mut *conn, turn_id, from, to, why).await
}

/// hook 帶來的 native 證據。`(session, turn)` 是去重的鑰匙，所以要跟收尾寫在同一句（#115）。
#[derive(Debug, Clone, Copy, Default)]
pub struct NativeEvidence<'a> {
    pub session_id: Option<&'a str>,
    pub turn_id: Option<&'a str>,
}

/// 每一支專用形狀先對表檢查自己那條邊：表上拿掉一條邊時，是這裡明確失敗，而不是等 trigger。
fn edge(from: &str, to: &str, shape: &str) -> Result<()> {
    if !is_legal(from, to) {
        anyhow::bail!("illegal turn status transition {from} -> {to} ({shape})");
    }
    Ok(())
}

/// **hook 收尾**：`in_flight -> completed`。同一句寫 native 證據、把「證不明」（`unknown`）的送達升成 `ok`
/// （回覆到了就是送到了），收尾時間只記第一次。只動還掛在圍籬放行那一代 run 上的回合。
///
/// 呼叫端要跟回覆訊息綁在同一個交易裡（#115），所以只有交易內版本。
pub async fn complete_with_native_evidence(
    conn: &mut sqlx::SqliteConnection,
    turn_id: &str,
    admitted: &super::fence::Admitted,
    ev: NativeEvidence<'_>,
) -> Result<Outcome> {
    edge("in_flight", "completed", "hook 收尾")?;
    let done = sqlx::query(
        "UPDATE turns SET status='completed', delivery=CASE WHEN delivery='unknown' THEN 'ok' ELSE delivery END,
                          completed_at=COALESCE(completed_at, ?), native_session_id=?, native_turn_id=?
          WHERE id=? AND status='in_flight' AND run_id=?",
    )
    .bind(crate::db::now())
    .bind(ev.session_id)
    .bind(ev.turn_id)
    .bind(turn_id)
    .bind(admitted.run_id())
    .execute(&mut *conn)
    .await?;
    if done.rows_affected() > 0 {
        return Ok(Outcome::Applied);
    }
    settled_or_fenced(&mut *conn, turn_id, "in_flight", "completed", "hook 收尾", "回合不在圍籬放行的那一代 run 上").await
}

/// **`StopFailure`**：`in_flight -> failed`。同一句寫 native 證據；`delivery` 不動——字送出去了，
/// 失敗的是回合（issue #79）。世代前提同 [`complete_with_native_evidence`]。
pub async fn fail_with_native_evidence(
    conn: &mut sqlx::SqliteConnection,
    turn_id: &str,
    admitted: &super::fence::Admitted,
    ev: NativeEvidence<'_>,
) -> Result<Outcome> {
    edge("in_flight", "failed", "StopFailure")?;
    let done = sqlx::query(
        "UPDATE turns SET status='failed', completed_at=COALESCE(completed_at, ?), native_session_id=?, native_turn_id=?
          WHERE id=? AND status='in_flight' AND run_id=?",
    )
    .bind(crate::db::now())
    .bind(ev.session_id)
    .bind(ev.turn_id)
    .bind(turn_id)
    .bind(admitted.run_id())
    .execute(&mut *conn)
    .await?;
    if done.rows_affected() > 0 {
        return Ok(Outcome::Applied);
    }
    settled_or_fenced(&mut *conn, turn_id, "in_flight", "failed", "StopFailure", "回合不在圍籬放行的那一代 run 上").await
}

/// **認領排隊的那一筆**：`queued -> in_flight`，同一句掛上 run。
///
/// 世代前提：那個 run **此刻還在跑**。flush 讀完 run 到認領之間，run 可能被不拿 bot 鎖的路徑收掉
/// （`mark_run_exited`：pane 死掉、reconcile）；重啟中那一段孤兒撤銷又刻意不撤，這時認領下去，
/// 回合就掛在一個死掉的 run 上、字打進不存在的 pane。沒認領到的留在佇列給下一個 run。
///
/// 同一句把 `auto_resend` 關掉：認領＝接著要打字了，送出之後結果寫不回來時（#149）這一筆也不會變成可以自動重送；
/// 送達結果寫回時照證據打開。
pub async fn claim_queued(conn: &mut sqlx::SqliteConnection, turn_id: &str, run_id: &str) -> Result<Outcome> {
    edge("queued", "in_flight", "認領排隊")?;
    let done = sqlx::query(
        "UPDATE turns SET status='in_flight', run_id=?, auto_resend=0
          WHERE id=? AND status='queued' AND EXISTS (SELECT 1 FROM runs WHERE id=? AND state='running')",
    )
    .bind(run_id)
    .bind(turn_id)
    .bind(run_id)
    .execute(&mut *conn)
    .await?;
    if done.rows_affected() > 0 {
        return Ok(Outcome::Applied);
    }
    settled_or_fenced(&mut *conn, turn_id, "queued", "in_flight", "認領排隊", "要認領到的 run 已經不在跑").await
}

/// **認領過但沒打字，放回佇列**：`in_flight -> queued`，同一句拔掉 run、重試次數加一、排下一次嘗試。
/// `delivery` 不動（還是 `pending`：一個字都沒打）。
pub async fn return_to_queue(conn: &mut sqlx::SqliteConnection, turn_id: &str, next_flush_at: &str) -> Result<Outcome> {
    edge("in_flight", "queued", "放回佇列")?;
    let done = sqlx::query(
        "UPDATE turns SET status='queued', run_id=NULL, flush_retries=flush_retries+1, next_flush_at=?
          WHERE id=? AND status='in_flight'",
    )
    .bind(next_flush_at)
    .bind(turn_id)
    .execute(&mut *conn)
    .await?;
    if done.rows_affected() > 0 {
        return Ok(Outcome::Applied);
    }
    settled(&mut *conn, turn_id, "in_flight", "queued", "放回佇列").await
}

/// **撤銷還在排隊的**：`queued -> failed`，同一句寫 `delivery='failed'`（一個字都沒送）、清掉 `next_flush_at`。
/// 已經被認領（`in_flight`）或送出的撤不回來，不假裝撤回。
pub async fn retract_queued(conn: &mut sqlx::SqliteConnection, turn_id: &str) -> Result<Outcome> {
    edge("queued", "failed", "撤銷排隊")?;
    let done = sqlx::query(
        "UPDATE turns SET status='failed', delivery='failed', completed_at=COALESCE(completed_at, ?), next_flush_at=NULL
          WHERE id=? AND status='queued'",
    )
    .bind(crate::db::now())
    .bind(turn_id)
    .execute(&mut *conn)
    .await?;
    if done.rows_affected() > 0 {
        return Ok(Outcome::Applied);
    }
    settled(&mut *conn, turn_id, "queued", "failed", "撤銷排隊").await
}

/// 帶世代前提的 CAS 沒打中：還在起點就是前提不成立（[`Outcome::Fenced`]），否則同 [`settled`]。
async fn settled_or_fenced(
    conn: &mut sqlx::SqliteConnection,
    turn_id: &str,
    from: &str,
    to: &str,
    why: &str,
    fenced: &'static str,
) -> Result<Outcome> {
    let now: Option<String> = sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(turn_id).fetch_optional(&mut *conn).await?;
    if now.as_deref() == Some(from) {
        tracing::info!(turn = turn_id, %from, %to, why, fenced, "turn 轉移沒發生：前提不成立");
        return Ok(Outcome::Fenced(fenced));
    }
    settled(&mut *conn, turn_id, from, to, why).await
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

    /// 一顆 bot、一個跑著的 run，回傳 (conversation, run)。
    async fn shape_fixture(app: &std::sync::Arc<crate::state::App>, project_id: &str) -> (String, String) {
        let bot = crate::testing::claude_bot(app, project_id, &format!("shape-{}", &crate::db::ulid()[18..])).await;
        let run = crate::testing::fake_run(app, &bot.id).await;
        (crate::db::conversation_id(&app.db, &bot.id).await.unwrap(), run)
    }

    async fn turn_in(app: &std::sync::Arc<crate::state::App>, conv: &str, run: Option<&str>, status: &str, delivery: &str) -> String {
        let id = crate::db::ulid();
        sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,created_at) VALUES (?,?,?,'web',?,?,?)")
            .bind(&id)
            .bind(conv)
            .bind(run)
            .bind(status)
            .bind(delivery)
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .unwrap();
        id
    }

    async fn row(app: &std::sync::Arc<crate::state::App>, id: &str) -> crate::db::Turn {
        sqlx::query_as::<_, crate::db::Turn>("SELECT * FROM turns WHERE id=?").bind(id).fetch_one(&app.db).await.unwrap()
    }

    /// issue #125：hook 收尾與 `StopFailure` 各一句寫完 status＋native 證據。重播的同一則（或第二則 hook）
    /// 不再收一次、也不蓋掉第一次的 native id；不屬於圍籬放行那一代 run 的回合一個欄位都不動。
    #[tokio::test]
    async fn hook_shapes_close_once_and_only_on_the_admitted_generation() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let (conv, run) = shape_fixture(app, &e.project_id).await;
        let admitted = crate::lifecycle::fence::Admitted::for_test(&run);
        let ev = |s, t| NativeEvidence { session_id: Some(s), turn_id: Some(t) };

        // 收尾：unknown 的送達升成 ok，native id 跟 status 同一句寫進去。
        let t = turn_in(app, &conv, Some(&run), "in_flight", "unknown").await;
        let mut c = app.db.acquire().await.unwrap();
        assert_eq!(complete_with_native_evidence(&mut c, &t, &admitted, ev("s1", "p1")).await.unwrap(), Outcome::Applied);
        let r = row(app, &t).await;
        assert_eq!((r.status.as_str(), r.delivery.as_str()), ("completed", "ok"));
        assert_eq!((r.native_session_id.as_deref(), r.native_turn_id.as_deref()), (Some("s1"), Some("p1")));
        let first_done = r.completed_at.clone();
        assert!(first_done.is_some());
        // 重複的 hook：不再收一次，native id 與收尾時間都不被蓋掉；StopFailure 後到也一樣。
        assert_eq!(
            complete_with_native_evidence(&mut c, &t, &admitted, ev("s1", "p2")).await.unwrap(),
            Outcome::Raced { now: "completed".into() }
        );
        assert_eq!(fail_with_native_evidence(&mut c, &t, &admitted, ev("s1", "p3")).await.unwrap(), Outcome::Raced { now: "completed".into() });
        let r = row(app, &t).await;
        assert_eq!((r.native_turn_id.as_deref(), r.completed_at), (Some("p1"), first_done));

        // 舊世代：回合掛在別的 run 上。放行的是這一代，收不到它。
        let other_run = crate::testing::fake_run(app, &crate::testing::claude_bot(app, &e.project_id, "shape-other").await.id).await;
        let stale = turn_in(app, &conv, Some(&other_run), "in_flight", "ok").await;
        assert!(matches!(complete_with_native_evidence(&mut c, &stale, &admitted, ev("s9", "p9")).await.unwrap(), Outcome::Fenced(_)));
        assert!(matches!(fail_with_native_evidence(&mut c, &stale, &admitted, ev("s9", "p9")).await.unwrap(), Outcome::Fenced(_)));
        let r = row(app, &stale).await;
        assert_eq!((r.status.as_str(), r.native_turn_id), ("in_flight", None), "一個欄位都沒改");

        // StopFailure：收成 failed，`delivery` 不動（字送出去了，失敗的是回合）。
        sqlx::query("UPDATE turns SET status='failed', completed_at=? WHERE id=?").bind(crate::db::now()).bind(&stale).execute(&app.db).await.unwrap();
        let f = turn_in(app, &conv, Some(&run), "in_flight", "ok").await;
        assert_eq!(fail_with_native_evidence(&mut c, &f, &admitted, ev("s1", "p4")).await.unwrap(), Outcome::Applied);
        let r = row(app, &f).await;
        assert_eq!((r.status.as_str(), r.delivery.as_str(), r.native_turn_id.as_deref()), ("failed", "ok", Some("p4")));
    }

    /// issue #125：撤銷與認領搶同一筆排隊的，只會有一個贏；輸的那邊講得出為什麼、什麼都沒寫。
    /// 放回佇列只放得回還在飛的那一筆。
    #[tokio::test]
    async fn queue_shapes_have_exactly_one_winner_per_race() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let (conv, run) = shape_fixture(app, &e.project_id).await;
        sqlx::query("UPDATE runs SET state='running' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
        let mut c = app.db.acquire().await.unwrap();

        // 認領先到：撤銷撤不回來。
        let q = turn_in(app, &conv, None, "queued", "pending").await;
        assert_eq!(claim_queued(&mut c, &q, &run).await.unwrap(), Outcome::Applied);
        assert_eq!(retract_queued(&mut c, &q).await.unwrap(), Outcome::Raced { now: "in_flight".into() });
        let r = row(app, &q).await;
        assert_eq!((r.status.as_str(), r.delivery.as_str(), r.run_id.as_deref()), ("in_flight", "pending", Some(run.as_str())));

        // 放回佇列：拔掉 run、記一次重試、排下一次；已經不在飛的放不回去。
        assert_eq!(return_to_queue(&mut c, &q, "2099-01-01T00:00:00.000Z").await.unwrap(), Outcome::Applied);
        let r = row(app, &q).await;
        assert_eq!((r.status.as_str(), r.run_id.as_deref(), r.flush_retries), ("queued", None, 1));
        assert_eq!(return_to_queue(&mut c, &q, "2099-01-01T00:00:00.000Z").await.unwrap(), Outcome::Raced { now: "queued".into() });
        assert_eq!(row(app, &q).await.flush_retries, 1, "沒放回去就不多記一次");

        // 撤銷先到：認領不到，也不會把它拉回進行中。
        assert_eq!(retract_queued(&mut c, &q).await.unwrap(), Outcome::Applied);
        let r = row(app, &q).await;
        assert_eq!((r.status.as_str(), r.delivery.as_str(), r.next_flush_at), ("failed", "failed", None));
        assert!(r.completed_at.is_some());
        assert_eq!(claim_queued(&mut c, &q, &run).await.unwrap(), Outcome::Raced { now: "failed".into() });
        assert_eq!(retract_queued(&mut c, &q).await.unwrap(), Outcome::Raced { now: "failed".into() }, "重複撤銷冪等");

        // 認領到已經不在跑的 run：留在佇列。
        let q2 = turn_in(app, &conv, None, "queued", "pending").await;
        sqlx::query("UPDATE runs SET state='exited' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
        assert!(matches!(claim_queued(&mut c, &q2, &run).await.unwrap(), Outcome::Fenced(_)));
        assert_eq!((row(app, &q2).await.status.as_str(), row(app, &q2).await.run_id), ("queued", None));
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
