//! issue #101（重開）：**舊 DB 的秒格式時間戳與新寫入的毫秒格式混存時，到期／先後判斷要照「時刻」判。**
//!
//! 寫入端早就統一成毫秒了（`db::now()`／`db::iso_in()`／`db::iso_at()`），但既有資料庫裡的到期時間
//! （`notify_next_at`／`next_attempt_at`／`resume_at`／`watchdog_next_at`／租約與核准的 `expires_at`）
//! 是舊版寫的**秒**：`2026-09-18T10:00:00Z`。它跟毫秒格式的 `2026-09-18T10:00:00.500Z` 在字串上
//! 差在第 20 個字元——`Z`(0x5A) 大於 `.`(0x2E)——所以拿字串比大小，同一秒內舊格式永遠「比較晚」。
//! 真實時間 10:00:00.000 早在 10:00:00.500 之前，字串卻說還沒到。
//!
//! 這裡每一條都用**兩種格式混存**的資料打真的判斷（SQL 與 Rust 兩邊都有），不是只驗 helper。

use crate::db;
use crate::testing as tt;
use serde_json::json;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::SqlitePool;

/// 同一秒之內、毫秒不在邊界上的一刻：舊格式（`…:SSZ`）與現在的毫秒字串（`…:SS.mmmZ`）
/// 落在同一個秒，字串比較最容易判錯的地方。
///
/// 回傳舊格式的「這一秒的整點」（`…:SSZ`）。等到毫秒落在 [60, 500] 才回，之後幾個 ms 內跑完的判斷
/// 都還在同一秒；真的跨秒了，字串比較剛好也對，只會少紅、不會誤紅。
async fn mid_second() -> String {
    loop {
        let n = chrono::Utc::now();
        if (60..=500).contains(&n.timestamp_subsec_millis()) {
            return n.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        }
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    }
}

async fn approved(p: &SqlitePool, requester: &str, purpose: &str, expires_at: &str) -> String {
    use crate::supervisor::store as st;
    st::get_or_init(p).await.unwrap();
    let a = st::create_approval(p, requester, purpose, "daemon", None, None, None).await.unwrap().approval;
    st::decide_approval(p, &a.id, "approved", "AGM", None, Some(expires_at)).await.unwrap();
    a.id
}

/// 協調者補送：`notify_next_at` 是舊版 `defer_notify` 寫的秒格式，`due_for` 綁的「現在」是毫秒。
///
/// 上一版的測試把「同一秒內舊格式晚一拍」寫成**接受的界線**——那正是這次重開要拿掉的東西。
#[tokio::test]
async fn a_second_precision_notify_deadline_is_due_by_the_instant_not_the_string() {
    use crate::supervisor::{roles, store};
    let e = tt::env().await;
    let p = &e.app.db;
    store::get_or_init(p).await.unwrap();
    let rows = [
        // 舊格式、同一秒內、已經過了（10:00:00.000 <= 10:00:00.500）→ 該出來。**字串比較在這裡判錯。**
        ("sec-same-second-past", "2026-09-18T10:00:00Z", true),
        // 毫秒格式、同一秒內、已經過了 → 該出來。
        ("ms-same-second-past", "2026-09-18T10:00:00.400Z", true),
        // 毫秒格式、同一秒內、還沒到 → 不該出來。
        ("ms-same-second-future", "2026-09-18T10:00:00.600Z", false),
        // 舊格式、下一秒 → 不該出來。
        ("sec-next-second", "2026-09-18T10:00:01Z", false),
        ("sec-far-past", "2026-09-18T09:59:59Z", true),
        ("sec-far-future", "2026-09-18T10:10:00Z", false),
        // 任何合法 RFC3339 都要判對（`+08:00` 的 18:00:00 就是 10:00:00Z）。
        ("offset-past", "2026-09-18T18:00:00+08:00", true),
        ("offset-future", "2026-09-18T18:00:01+08:00", false),
    ];
    for (key, _, _) in &rows {
        store::push_inbox(p, key, "approval_requested", None, None, None, &json!({})).await.unwrap();
    }
    roles::classify(p).await.unwrap();
    for (key, at, _) in &rows {
        sqlx::query("UPDATE supervisor_inbox SET notify_next_at=? WHERE event_key=?").bind(at).bind(key).execute(p).await.unwrap();
    }
    let now = "2026-09-18T10:00:00.500Z";
    let mut got: Vec<String> = roles::due_for(p, roles::Role::Patrol, false, now, 5).await.unwrap().into_iter().map(|e| e.event_key).collect();
    got.sort();
    let mut want: Vec<String> = rows.iter().filter(|r| r.2).map(|r| r.0.to_string()).collect();
    want.sort();
    assert_eq!(got, want, "新舊格式混存，要照時刻判");

    // 單角色時代那支（留給既有測試的守衛）也一樣。
    let mut got: Vec<String> = store::due_inbox(p, now, 5).await.unwrap().into_iter().map(|e| e.event_key).collect();
    got.sort();
    assert_eq!(got, want, "due_inbox 也要照時刻判");
}

/// 核准的 `expires_at` 是舊版 `iso_in` 寫的秒格式：過期判斷（`refusal`／`oldest_live_window_approval`）
/// 拿的「現在」是毫秒。
#[tokio::test]
async fn a_second_precision_approval_expiry_is_judged_by_the_instant() {
    use crate::supervisor::store as st;
    let e = tt::env().await;
    let p = &e.app.db;
    let now = "2026-09-18T10:00:00.500Z";

    // 已在 10:00:00.000 過期（舊格式），現在 10:00:00.500 → 過期。
    let id = approved(p, "bot-a", "restart", "2026-09-18T10:00:00Z").await;
    let a = st::approval(p, &id).await.unwrap().unwrap();
    assert_eq!(a.refusal(now, "restart", None), Some("approval_expired"), "同一秒內、舊格式：早就過期了");
    assert!(st::oldest_live_window_approval(p, now).await.unwrap().is_none(), "過期的不算還活著的核准");

    // 下一秒才過期（舊格式）→ 還有效。
    sqlx::query("UPDATE supervisor_approvals SET expires_at='2026-09-18T10:00:01Z' WHERE id=?").bind(&id).execute(p).await.unwrap();
    let a = st::approval(p, &id).await.unwrap().unwrap();
    assert_eq!(a.refusal(now, "restart", None), None, "還沒到期");
    assert_eq!(st::oldest_live_window_approval(p, now).await.unwrap().map(|x| x.id), Some(id.clone()));

    // 毫秒格式、同一秒內：0.400 已過、0.600 沒到。
    sqlx::query("UPDATE supervisor_approvals SET expires_at='2026-09-18T10:00:00.400Z' WHERE id=?").bind(&id).execute(p).await.unwrap();
    assert_eq!(st::approval(p, &id).await.unwrap().unwrap().refusal(now, "restart", None), Some("approval_expired"));
    sqlx::query("UPDATE supervisor_approvals SET expires_at='2026-09-18T10:00:00.600Z' WHERE id=?").bind(&id).execute(p).await.unwrap();
    assert_eq!(st::approval(p, &id).await.unwrap().unwrap().refusal(now, "restart", None), None);
}

/// 租約的 `held_at`：`expires_at` 是舊版寫的秒格式。
#[tokio::test]
async fn a_second_precision_lease_is_held_only_until_its_instant() {
    use crate::supervisor::store as st;
    let e = tt::env().await;
    let p = &e.app.db;
    let now = "2026-09-18T10:00:00.500Z";
    st::acquire_lease(p, "restart", "runner", None, None, "2026-09-18T10:00:00Z", false, None, &json!({})).await.unwrap();
    assert!(!st::lease(p, "restart").await.unwrap().unwrap().held_at(now), "10:00:00.000 已過，不算握著");
    // 沒 release 的租約過期了，另一個人可以接手：改成下一秒才過期的，要仍算握著。
    sqlx::query("UPDATE supervisor_leases SET expires_at='2026-09-18T10:00:01Z' WHERE resource='restart'").execute(p).await.unwrap();
    assert!(st::lease(p, "restart").await.unwrap().unwrap().held_at(now), "10:00:01 才過期，還握著");
}

/// 租約真的接手／續約走的是 SQL（`expires_at <= now`／`expires_at > now`），「現在」是 `db::now()`：
/// 用「同一秒之內」造出舊格式的過期時間。
#[tokio::test]
async fn a_lease_that_expired_earlier_in_this_second_is_taken_over_and_cannot_renew() {
    use crate::supervisor::store as st;
    let e = tt::env().await;
    let p = &e.app.db;
    let legacy = mid_second().await;
    let first = st::acquire_lease(p, "restart", "runner-1", None, None, &legacy, false, None, &json!({}))
        .await
        .unwrap()
        .expect("第一個人拿到");

    // 舊格式、這一秒的整點 → 已經過了 60ms 以上。續約要失敗（過期的租約不能續）。
    assert!(!st::renew_lease(p, "restart", "runner-1", first.fence, &db::iso_in(600)).await.unwrap(), "過期的租約不能續");
    // 過期沒 release → 下一個人可以接手。
    let later = db::iso_in(600);
    let second = st::acquire_lease(p, "restart", "runner-2", None, None, &later, false, None, &json!({})).await.unwrap();
    assert!(second.is_some(), "同一秒內過期的舊格式租約要能被接手");
}

/// 過期沒 release 的租約背後那張核准，接手時要被消耗（見 `taking_over_an_expired_lease_consumes…`），
/// 那條判斷同樣走 SQL 的 `expires_at <= now`。
#[tokio::test]
async fn taking_over_a_lease_that_expired_this_second_consumes_its_approval() {
    use crate::supervisor::store as st;
    let e = tt::env().await;
    let p = &e.app.db;
    let first = approved(p, "bot-a", "restart", &db::iso_in(600)).await;
    let legacy = mid_second().await;
    st::acquire_lease(p, "restart", "runner-1", Some(&first), None, &legacy, false, None, &json!({})).await.unwrap().unwrap();
    let second = approved(p, "bot-a", "restart", &db::iso_in(600)).await;
    st::acquire_lease(p, "restart", "runner-2", Some(&second), None, &db::iso_in(600), false, None, &json!({})).await.unwrap().unwrap();
    assert_eq!(st::approval(p, &first).await.unwrap().unwrap().status, "consumed");
}

/// 同一個申請者換 commit 重新申請時，只有**還沒過期**的舊申請才被標成 `superseded`、把等待起點接過來。
/// 舊申請的 `expires_at` 是舊版寫的秒格式、這一秒稍早已經過期：不該還被當成活的。
#[tokio::test]
async fn superseding_an_approval_that_expired_earlier_this_second_leaves_it_alone() {
    use crate::supervisor::store as st;
    let e = tt::env().await;
    let p = &e.app.db;
    let old = approved(p, "bot-a", "rebuild", &mid_second().await).await;
    let out = st::create_approval_superseding(p, "bot-a", "rebuild", "daemon", Some("abc"), None, None, Some(&old)).await.unwrap();
    assert!(out.superseded.is_none(), "已過期的舊申請不算活的，不該被接手");
    assert_eq!(st::approval(p, &old).await.unwrap().unwrap().status, "approved", "留著原狀");
    assert_eq!(out.approval.wait_since, None, "沒有接手，就沒有等待起點可以繼承");
}

/// 升級計時的起點：接續來的 `wait_since` 與自己被核准的時間，取早的那個——兩個時間格式可以不同。
#[tokio::test]
async fn the_escalation_clock_starts_at_the_earlier_instant_whatever_the_spelling() {
    use crate::supervisor::store as st;
    let e = tt::env().await;
    let p = &e.app.db;
    let id = approved(p, "bot-a", "rebuild", &db::iso_in(600)).await;
    // 接續來的等待起點（秒格式）10:00:00.000 早於自己被核准的 10:00:00.500。
    sqlx::query("UPDATE supervisor_approvals SET decided_at='2026-09-18T10:00:00.500Z', wait_since='2026-09-18T10:00:00Z' WHERE id=?")
        .bind(&id)
        .execute(p)
        .await
        .unwrap();
    let a = st::approval(p, &id).await.unwrap().unwrap();
    assert_eq!(a.waiting_since(), Some("2026-09-18T10:00:00Z"));
}

/// due_actions 是**讀取端**：把六種到期時間放在同一份摘要裡，這六種裡舊資料庫有秒也有毫秒。
mod due_actions {
    use super::*;
    use crate::due_actions as da;

    async fn assignment(p: &SqlitePool, id: &str, next: &str) {
        sqlx::query(
            "INSERT INTO supervisor_assignments
               (id, supervisor_id, target_bot_id, client_request_id, text, status, attempts, next_attempt_at, created_at, updated_at)
             VALUES (?, 'agm', 'bot1', ?, 'x', 'delivered', 1, ?, ?, ?)",
        )
        .bind(id)
        .bind(format!("crid-{id}"))
        .bind(next)
        .bind(db::now())
        .bind(db::now())
        .execute(p)
        .await
        .unwrap();
    }

    async fn hook(p: &SqlitePool, id: &str, next: &str) {
        sqlx::query(
            "INSERT INTO hook_events (id, bot_id, provider, source, body_json, received_at, attempts, next_attempt_at)
             VALUES (?, 'b1', 'claude', 'http', '{}', ?, 0, ?)",
        )
        .bind(id)
        .bind(db::now())
        .bind(next)
        .execute(p)
        .await
        .unwrap();
    }

    /// 重開留言原文的例子：due = 10:00:00Z、now = 10:00:00.500Z，實際已到期。
    #[tokio::test]
    async fn a_second_precision_due_time_is_overdue_once_the_instant_has_passed() {
        let e = tt::env().await;
        assignment(&e.app.db, "legacy", "2026-09-18T10:00:00Z").await;
        let soon = da::soonest(&e.app.db).await.unwrap().expect("有一筆");
        let now = "2026-09-18T10:00:00.500Z";
        assert_eq!(soon.json(now)["overdue"], true, "10:00:00.000 早就過了 10:00:00.500");
        assert_eq!(soon.json("2026-09-18T09:59:59.999Z")["overdue"], false, "還差 1ms");
    }

    /// 最早到期的是誰：assignment 的舊格式 10:00:05Z 比 hook 的毫秒格式 10:00:05.500Z 早。
    /// 字串比較把 `.500Z` 排在 `Z` 前面（`.` < `Z`），於是選到晚的那一筆。
    #[tokio::test]
    async fn the_soonest_and_the_listing_order_compare_instants_across_kinds() {
        let e = tt::env().await;
        let p = &e.app.db;
        assignment(p, "legacy-sec", "2026-09-18T10:00:05Z").await;
        hook(p, "new-ms", "2026-09-18T10:00:05.500Z").await;
        let soon = da::soonest(p).await.unwrap().unwrap();
        assert_eq!(soon.entity_id, "legacy-sec", "10:00:05.000 早於 10:00:05.500");
        let order: Vec<String> = da::pending(p).await.unwrap().into_iter().map(|r| r.entity_id).collect();
        assert_eq!(order, vec!["legacy-sec", "new-ms"], "列表也要照時刻排");
    }

    /// 同一種來源裡新舊格式混存：SQL 的 `ORDER BY` 與 `LIMIT` 決定誰進得了清單、誰是「最早」。
    #[tokio::test]
    async fn within_one_source_the_sql_ordering_is_by_instant_too() {
        let e = tt::env().await;
        let p = &e.app.db;
        assignment(p, "b-ms", "2026-09-18T10:00:05.500Z").await;
        assignment(p, "a-sec", "2026-09-18T10:00:05Z").await;
        let soon = da::soonest(p).await.unwrap().unwrap();
        assert_eq!(soon.entity_id, "a-sec", "同一張表裡舊格式的 05.000 比 05.500 早");
    }

    /// 清單每一類最多列 [`da::PER_KIND`] 筆：截斷靠 SQL 的 `ORDER BY ... LIMIT`，所以排序也要照時刻，
    /// 不然「最早到期」的舊格式那一筆會因為字串排在後面被擠出清單。
    #[tokio::test]
    async fn the_listing_cap_keeps_the_earliest_by_instant_not_by_string() {
        let e = tt::env().await;
        let p = &e.app.db;
        for i in 0..da::PER_KIND {
            assignment(p, &format!("ms-{i:03}"), "2026-09-18T10:00:05.500Z").await;
        }
        // 舊格式、同一秒內更早（05.000 < 05.500）；字串排序把它排在最後一筆，剛好被 LIMIT 擠掉。
        assignment(p, "legacy-earliest", "2026-09-18T10:00:05Z").await;
        let rows = da::pending(p).await.unwrap();
        assert_eq!(rows.len() as i64, da::PER_KIND, "每一類最多這麼多筆");
        assert_eq!(rows[0].entity_id, "legacy-earliest", "最早的要在清單裡，而且排第一");
    }

    /// `overdue` 的計數是 SQL 聚合，「現在」是 `db::now()`：舊格式的整點秒在同一秒內已經過了。
    #[tokio::test]
    async fn the_overdue_count_treats_a_second_precision_time_earlier_this_second_as_overdue() {
        let e = tt::env().await;
        let p = &e.app.db;
        assignment(p, "legacy", &mid_second().await).await;
        let c = da::counts(p).await.unwrap();
        assert_eq!(c["assignment_retry"].overdue, 1, "這一秒的整點已經過了 60ms 以上");
        assert_eq!(c["assignment_retry"].pending, 1);
    }
}

/// 只有 SQL 的小庫：驗 `db::ts_sql` 本身，不需要整個 App。
async fn bare_pool() -> SqlitePool {
    SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap()
}

/// `db::ts_sql` 讓 SQLite 把每一種寫法都變成同一種字串：秒、毫秒、微秒、`Z`、`+08:00`、`-05:00`。
#[tokio::test]
async fn ts_sql_turns_every_spelling_into_the_canonical_string() {
    let p = bare_pool().await;
    let sql = format!("SELECT {} FROM (SELECT ? AS v)", db::ts_sql("v"));
    for (raw, want) in [
        ("2026-09-18T10:00:00Z", "2026-09-18T10:00:00.000Z"),
        ("2026-09-18T10:00:00.5Z", "2026-09-18T10:00:00.500Z"),
        ("2026-09-18T10:00:00.500Z", "2026-09-18T10:00:00.500Z"),
        ("2026-09-18T10:00:00.594+00:00", "2026-09-18T10:00:00.594Z"),
        ("2026-09-18T18:00:00+08:00", "2026-09-18T10:00:00.000Z"),
        ("2026-09-18T05:00:00-05:00", "2026-09-18T10:00:00.000Z"),
        // 跨日、跨年也要換對。
        ("2027-01-01T05:00:00+08:00", "2026-12-31T21:00:00.000Z"),
    ] {
        let got: String = sqlx::query_scalar(&sql).bind(raw).fetch_one(&p).await.unwrap();
        assert_eq!(got, want, "{raw}");
        assert_eq!(db::parse_ts(raw).map(db::iso_at).as_deref(), Some(want), "Rust 端 {raw}");
    }
    // 解不開的原樣退回（不憑空編時間），NULL 還是 NULL。
    let got: String = sqlx::query_scalar(&sql).bind("not-a-time").fetch_one(&p).await.unwrap();
    assert_eq!(got, "not-a-time");
    let got: Option<String> = sqlx::query_scalar(&sql).bind(None::<String>).fetch_one(&p).await.unwrap();
    assert_eq!(got, None);
}

/// 混著寫的一堆時刻：SQL 的 `ORDER BY ts_sql(...)` 與 Rust 的 `cmp_ts` 排出來的順序都是時刻序，
/// 而**直接排字串**（舊行為）不是——這條同時證明這個測試打得到病灶。
#[tokio::test]
async fn ordering_a_mixed_bag_by_instant_matches_real_time_and_the_raw_strings_do_not() {
    let base = chrono::DateTime::parse_from_rfc3339("2026-09-18T10:00:00Z").unwrap().with_timezone(&chrono::Utc);
    // 每 250ms 一個，前後各半秒；整秒的寫成秒格式，一半改用 `+08:00`，其餘毫秒。
    let mut items: Vec<(chrono::DateTime<chrono::Utc>, String)> = Vec::new();
    for k in -8..=8i64 {
        let t = base + chrono::Duration::milliseconds(250 * k);
        let spelled = match (k % 2 == 0, t.timestamp_subsec_millis() == 0) {
            (true, true) => t.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            (true, false) => db::iso_at(t),
            (false, _) => {
                let east = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
                t.with_timezone(&east).to_rfc3339_opts(chrono::SecondsFormat::Millis, false)
            }
        };
        items.push((t, spelled));
    }
    // 決定性的洗牌。
    let mut shuffled = items.clone();
    shuffled.sort_by_key(|(t, _)| (t.timestamp_millis() * 7919) % 1009);

    let want: Vec<String> = items.iter().map(|(_, s)| s.clone()).collect();

    let mut by_rust: Vec<String> = shuffled.iter().map(|(_, s)| s.clone()).collect();
    by_rust.sort_by(|a, b| db::cmp_ts(a, b));
    assert_eq!(by_rust, want, "cmp_ts 照時刻排");

    let p = bare_pool().await;
    sqlx::query("CREATE TABLE t (v TEXT)").execute(&p).await.unwrap();
    for (_, s) in &shuffled {
        sqlx::query("INSERT INTO t (v) VALUES (?)").bind(s).execute(&p).await.unwrap();
    }
    let by_sql: Vec<String> = sqlx::query_scalar(&format!("SELECT v FROM t ORDER BY {}", db::ts_sql("v"))).fetch_all(&p).await.unwrap();
    assert_eq!(by_sql, want, "ts_sql 照時刻排");

    let raw: Vec<String> = sqlx::query_scalar("SELECT v FROM t ORDER BY v").fetch_all(&p).await.unwrap();
    assert_ne!(raw, want, "直接排字串會排錯——不然這個測試就沒打到病灶");
}

#[test]
fn same_instant_ignores_how_the_time_is_spelled() {
    assert!(db::same_instant(Some("2026-09-18T10:00:00Z"), Some("2026-09-18T10:00:00.000Z")));
    assert!(db::same_instant(Some("2026-09-18T18:00:00+08:00"), Some("2026-09-18T10:00:00.000Z")));
    assert!(!db::same_instant(Some("2026-09-18T10:00:00Z"), Some("2026-09-18T10:00:00.001Z")));
    assert!(db::same_instant(None, None));
    assert!(!db::same_instant(Some("2026-09-18T10:00:00Z"), None));
    // 解不開的退回字串相等。
    assert!(db::same_instant(Some("x"), Some("x")));
    assert!(!db::same_instant(Some("x"), Some("y")));
}

// ---------------------------------------------------------------- 守衛：不准有人再繞過去

fn production_sources() -> Vec<(String, String)> {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    walk(&root, &mut files);
    files.sort();
    files
        .into_iter()
        .filter_map(|f| {
            let at = f.strip_prefix(&root).unwrap().display().to_string();
            if at == "timestamp_compat_tests.rs" || at.ends_with("_tests.rs") {
                return None; // 整份都是測試
            }
            let src = std::fs::read_to_string(&f).unwrap();
            // 生產區：第一個「`#[cfg(test)]` 接著 `mod`」之前。單獨一個 `#[cfg(test)] fn`（例如只給測試用的
            // 輔助函式）不算測試區的開頭，後面的正式程式碼仍要掃。
            let lines: Vec<&str> = src.lines().collect();
            let mut end = lines.len();
            for (i, l) in lines.iter().enumerate() {
                if l.trim() == "#[cfg(test)]" {
                    let next = lines[i + 1..].iter().find(|n| !n.trim_start().starts_with("#["));
                    if next.is_some_and(|n| n.trim_start().starts_with("mod ") || n.trim_start().starts_with("pub mod ")) {
                        end = i;
                        break;
                    }
                }
            }
            Some((at, lines[..end].join("\n")))
        })
        .collect()
}

/// 已知還沒修、而且**不歸這張票的人改**的地方：`(命中的那行要包含的字串, 歸誰／追在哪張票)`。
/// 修掉之後那一行不再命中，下面 `stale allowlist` 那道檢查會逼你把它從這裡拿掉。
/// （目前沒有：`supervisor/maintenance.rs` 那兩處已在 #140 修掉。）
const KNOWN_UNFIXED: &[(&str, &str)] = &[];

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `rest` 開頭（略過空白）是不是一個排序比較運算子（`<`、`<=`、`>`、`>=`；不含 `<>`、`->`、`=>`）。
fn starts_with_ordering_op(rest: &str) -> bool {
    let r = rest.trim_start();
    r.starts_with("<=") || r.starts_with(">=") || (r.starts_with('<') && !r.starts_with("<>")) || r.starts_with('>')
}

/// `before` 結尾（略過空白）是不是一個排序比較運算子。
fn ends_with_ordering_op(before: &str) -> bool {
    let b = before.trim_end();
    b.ends_with("<=") || b.ends_with(">=") || (b.ends_with('<') && !b.ends_with("<<")) || (b.ends_with('>') && !b.ends_with("->") && !b.ends_with("=>"))
}

/// `col` 以完整識別字出現，而且後面接著排序比較（`expires_at <= ?`）。
fn column_compared(line: &str, col: &str) -> bool {
    line.match_indices(col).any(|(i, _)| {
        let before_ok = line[..i].chars().next_back().is_none_or(|c| !is_ident(c));
        let after = &line[i + col.len()..];
        before_ok && after.chars().next().is_none_or(|c| !is_ident(c)) && starts_with_ordering_op(after)
    })
}

/// `x.as_str() < y`／`x < y.as_str()`：兩邊都是字串的排序比較。
fn str_ordering(line: &str) -> bool {
    line.match_indices(".as_str()").any(|(i, _)| {
        if starts_with_ordering_op(&line[i + ".as_str()".len()..]) {
            return true;
        }
        // 往回退到這個運算元的開頭，看它前面是不是比較運算子。
        let head = &line[..i];
        let start = head.rfind(|c: char| !(is_ident(c) || c == '.' || c == '&' || c == '*' || c == '(' || c == ')')).map_or(0, |k| k + 1);
        ends_with_ordering_op(&head[..start])
    })
}

/// `|t| t <= now`：閉包參數直接拿來比大小。
fn closure_param_ordering(line: &str) -> bool {
    line.match_indices('|').any(|(i, _)| {
        let rest = &line[i + 1..];
        let name: String = rest.chars().take_while(|c| is_ident(*c)).collect();
        if name.is_empty() {
            return false;
        }
        let Some(after_bar) = rest[name.len()..].strip_prefix('|') else { return false };
        let body = after_bar.trim_start().trim_start_matches('*');
        body.strip_prefix(name.as_str()).is_some_and(|tail| tail.chars().next().is_none_or(|c| !is_ident(c)) && starts_with_ordering_op(tail))
    })
}

/// 兩個都叫「某個時間」的裸識別字直接比大小（`exp < requested`）。只認這幾個名字：識別字太泛的話，
/// 每個 `n > 0` 都會誤報。
fn time_named_operands_ordered(line: &str) -> bool {
    const TIME_WORDS: &[&str] = &["exp", "expires", "expiry", "requested", "retry", "until", "deadline", "reset", "due", "cutoff", "decided", "now", "now_iso", "iso"];
    let word = |s: &str| -> String { s.chars().take_while(|c| is_ident(*c)).collect() };
    ["<=", ">=", "<", ">"].iter().any(|op| {
        line.match_indices(op).any(|(i, _)| {
            // `<=` 也含 `<`：只在運算子開頭那一格判。
            if (*op == "<" || *op == ">") && line[i + 1..].starts_with('=') {
                return false;
            }
            if line[..i].ends_with('-') || line[..i].ends_with('=') || line[i + op.len()..].starts_with('>') {
                return false; // `->`、`=>`、`<>`
            }
            let left: String = line[..i].trim_end().chars().rev().take_while(|c| is_ident(*c)).collect::<Vec<_>>().into_iter().rev().collect();
            let right = word(line[i + op.len()..].trim_start());
            TIME_WORDS.contains(&left.as_str()) && TIME_WORDS.contains(&right.as_str())
        })
    })
}

/// 生產程式碼裡的**時間字串**比大小，只有兩條合法的路：
/// - SQL：把欄位包進 `db::ts_sql(..)` 再比（讀取端正規化）；
/// - Rust：`db::cmp_ts`／`db::same_instant`／`db::parse_ts` 之後比時刻。
///
/// 這條掃原始碼，擋住：
/// 1. 到期欄位（會被舊版寫成秒格式的那幾欄）直接 `<=`／`>` 綁參數，沒有 `ts_sql`；
/// 2. Rust 裡拿 `&str` 直接 `<`／`>`（`x.as_str() > now.as_str()`、`|t| t <= now`），
///    或對 `resets_at` 的字串直接 `.min()`／`.max()`；
/// 3. 各自用 `datetime('now')`／`CURRENT_TIMESTAMP`／`strftime(` 造出另一種格式的時間。
///
/// 只由 `db::now()`／`Utc` 的毫秒格式寫、從來沒有舊格式的欄位（`build_slots`、`hook_events`、
/// `herdr_maintenance`、`bot_reads`）字串比較本來就對，列在 `CANONICAL_ONLY`，各有理由。
#[test]
fn deadline_comparisons_never_compare_raw_timestamp_strings() {
    // (檔案, 為什麼字串比較在這裡是對的)
    const CANONICAL_ONLY: &[(&str, &str)] = &[
        ("build_scheduler.rs", "build_slots 的 since／last_seen／expires_at 只由本檔用 Utc::now() 的毫秒格式寫，從來沒有秒格式"),
        ("hook_inbox.rs", "hook_events.next_attempt_at 只由 mark_failed 用 Utc 的毫秒格式寫"),
        ("herdr_maintenance.rs", "herdr_maintenance.until 只由本檔用 Utc 的毫秒格式寫"),
        ("read_marks.rs", "normalize_at 先把輸入 parse 成 Utc 再寫成毫秒，才跟 db::now() 比"),
    ];
    const DEADLINE_COLS: &[&str] = &[
        "notify_next_at",
        "next_attempt_at",
        "resume_at",
        "watchdog_next_at",
        "expires_at",
        "cooldown_until",
        "quota_reset_at",
        "next_flush_at",
    ];
    let mut hits: Vec<String> = Vec::new();
    for (file, prod) in production_sources() {
        if file == "db.rs" {
            continue; // 格式與正規化就定義在這裡
        }
        let canonical_only = CANONICAL_ONLY.iter().any(|(f, _)| *f == file);
        for (i, line) in prod.lines().enumerate() {
            let t = line.trim();
            if t.starts_with("//") || t.starts_with('*') || t.starts_with("--") {
                continue;
            }
            let bad_sql = (DEADLINE_COLS.iter().any(|c| column_compared(line, c)) && !line.contains("ts_sql"))
                || line.match_indices("{due}").any(|(k, m)| starts_with_ordering_op(&line[k + m.len()..]));
            let unit = line.contains("Instant") || line.contains("Duration") || line.contains(".len()");
            let time_column = DEADLINE_COLS.iter().chain(["until", "resets_at", "due_at"].iter()).any(|c| line.contains(c));
            let bad_rust = (!unit && (str_ordering(line) || (time_column && closure_param_ordering(line)) || time_named_operands_ordered(line)))
                || (line.contains("resets_at") && (line.contains(".min()") || line.contains(".max()")));
            let bad_clock = line.contains("datetime('now'") || line.contains("CURRENT_TIMESTAMP") || line.contains("strftime(");
            let bad = if canonical_only { bad_clock } else { bad_sql || bad_rust || bad_clock };
            if bad {
                hits.push(format!("{file}:{}: {t}", i + 1));
            }
        }
    }
    let unexpected: Vec<&String> = hits.iter().filter(|h| !KNOWN_UNFIXED.iter().any(|(needle, _)| h.contains(needle))).collect();
    assert!(
        unexpected.is_empty(),
        "時間字串直接比大小：舊資料庫的秒格式（`…:00Z`）跟毫秒格式（`…:00.500Z`）混存時，同一秒內會判錯。\n\
         SQL 端把欄位包成 `db::ts_sql(\"col\")`；Rust 端用 `db::cmp_ts`／`db::same_instant`（issue #101）：\n{}",
        unexpected.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n")
    );
    // 已知未修的清單不准留著過期的項目：修掉了就要從這裡拿掉。
    for (needle, why) in KNOWN_UNFIXED {
        assert!(hits.iter().any(|h| h.contains(needle)), "stale allowlist：{needle}（{why}）已經不再命中，請從 KNOWN_UNFIXED 移除");
    }
}

/// 上面那道守衛認得出病灶：對著**舊寫法**的樣本行，每一種都要命中。沒有這條，守衛壞了（regex 打歪、
/// 運算子判斷寫反）也會靜悄悄地綠。
#[test]
fn the_source_guard_recognises_the_shapes_it_is_meant_to_catch() {
    assert!(column_compared("WHERE resource=? AND expires_at <= ?", "expires_at"));
    assert!(column_compared("AND (notify_next_at IS NULL OR notify_next_at <= ?3)", "notify_next_at"));
    assert!(column_compared("AND expires_at > ?", "expires_at"));
    assert!(!column_compared("SET expires_at=? WHERE", "expires_at"), "賦值不是比較");
    assert!(!column_compared("AND expires_at <> ?", "expires_at"), "不等於不是排序");
    assert!(!column_compared("AND prev_expires_at <= ?", "expires_at"), "要是完整識別字");
    assert!(str_ordering("if exp.as_str() > now.as_str() {"));
    assert!(str_ordering("Some(t) if t < retry.as_str() && ok => 1,"));
    assert!(str_ordering("if w.until.as_str() > crate::db::now().as_str() {"));
    assert!(!str_ordering("fn f() -> String { x.as_str().to_string() }"));
    assert!(!str_ordering("Some(x) => x.as_str(),"));
    assert!(time_named_operands_ordered("Some(exp) if exp < requested => exp.to_string(),"));
    assert!(!time_named_operands_ordered("if n > 0 { 1 } else { 2 }"));
    assert!(!time_named_operands_ordered("fn f(now: u64) -> Option<u64> { None }"));
    assert!(closure_param_ordering(".is_some_and(|t| t <= now)"));
    assert!(closure_param_ordering(".filter(|w| *w < decided)"));
    assert!(!closure_param_ordering(".map(|t| t + 1)"));
    assert!(!closure_param_ordering(".map(|a, b| a.cmp(b))"));
}

/// 維護窗口的升級計時：`escalation_for` 過濾「還沒過期的核准」，用的是 `db::now()`。
/// 核准的 `expires_at` 是舊版寫的秒格式、這一秒稍早已過期：不算還能用來開窗口。
#[tokio::test]
async fn an_approval_that_expired_earlier_this_second_does_not_start_the_escalation_clock() {
    let e = tt::env().await;
    let p = &e.app.db;
    let id = approved(p, "bot-a", "restart", &mid_second().await).await;
    let got = crate::supervisor::maintenance::escalation_for(&e.app, Some(&id)).await.unwrap();
    assert!(got.is_none(), "已過期的核准不算還能用來開窗口");
    // 下一秒才過期的照舊有效。
    let live = approved(p, "bot-b", "restart", &db::iso_in(600)).await;
    assert!(crate::supervisor::maintenance::escalation_for(&e.app, Some(&live)).await.unwrap().is_some());
}

/// 租約的到期時間取「要求的」與「核准自己的到期」較早的那個。核准的 `expires_at` 可能是舊版寫的秒格式，
/// 或 AGM 從 API 帶進來的任意 RFC3339（`+08:00`）——照時刻取，不照字串。
#[test]
fn the_lease_deadline_is_the_earlier_instant_whatever_the_spelling() {
    use crate::supervisor::maintenance::lease_deadline;
    // 舊格式、同一秒內更早（10:00:00.000 < 10:00:00.500）→ 用核准的。
    assert_eq!(lease_deadline("2026-09-18T10:00:00.500Z", Some("2026-09-18T10:00:00Z")), "2026-09-18T10:00:00Z");
    // 帶位移：18:00:00+08:00 就是 10:00:00Z，早於 10:00:01 → 用核准的（字串比較差 8 小時）。
    assert_eq!(lease_deadline("2026-09-18T10:00:01.000Z", Some("2026-09-18T18:00:00+08:00")), "2026-09-18T18:00:00+08:00");
    // 核准比較晚 → 用要求的。
    assert_eq!(lease_deadline("2026-09-18T10:00:00.500Z", Some("2026-09-18T10:00:01Z")), "2026-09-18T10:00:00.500Z");
    assert_eq!(lease_deadline("2026-09-18T10:00:00.500Z", Some("2026-09-18T18:00:01+08:00")), "2026-09-18T10:00:00.500Z");
    assert_eq!(lease_deadline("2026-09-18T10:00:00.500Z", None), "2026-09-18T10:00:00.500Z");
}
