use super::*;
use sqlx::sqlite::SqlitePoolOptions;

async fn pool() -> SqlitePool {
    let p = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
    migrate(&p).await.unwrap();
    p
}

async fn pre_additive_schema() -> SqlitePool {
    let p = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
    for stmt in DDL.split(";\n") {
        let s = stmt.trim();
        if !s.is_empty() {
            sqlx::query(s).execute(&p).await.unwrap();
        }
    }
    p
}

/// #1653：收件匣只有一個 `supervisor_id`，`supervisor_inbox_key` 的前綴等於整張表；`state` 又排在 `payload_json` 後面，
/// 照它篩 `delivered`／`pending` 得把每列的 payload 讀過一遍（線上 2.6 千列、14 MB）。partial 的 `supervisor_inbox_open`
/// 幫不上（SQLite 不認 `state='delivered'` 蘊含 `state != 'handled'`），要走 `(supervisor_id, state)`。
/// 兩句是 `delivered_inbox`／`exhausted_inbox` 的形狀；查詢計畫是決定性的，不靠計時。
#[tokio::test]
async fn inbox_lookups_by_state_walk_the_supervisor_state_index() {
    let p = pool().await;
    // 歸屬的定義只有 `roles::OWNER` 一份（#504 的守門測試不准再抄一份），這裡照 `exhausted_inbox` 的寫法組出來。
    let owner = super::super::roles::OWNER;
    for (name, sql) in [
        (
            "delivered_inbox",
            "SELECT * FROM supervisor_inbox WHERE supervisor_id=? AND state='delivered' ORDER BY COALESCE(delivered_at, updated_at) ASC".to_string(),
        ),
        (
            "exhausted_inbox",
            format!(
                "SELECT COUNT(*) FROM supervisor_inbox WHERE supervisor_id=? AND state='pending' AND notify_attempts >= ? \
                 AND COALESCE({owner}, 'patrol')='patrol'"
            ),
        ),
    ] {
        let plan: Vec<(i64, i64, i64, String)> = sqlx::query_as(&format!("EXPLAIN QUERY PLAN {sql}")).fetch_all(&p).await.unwrap();
        let details: Vec<&str> = plan.iter().map(|r| r.3.as_str()).collect();
        assert!(
            details.iter().any(|d| d.starts_with("SEARCH supervisor_inbox USING INDEX supervisor_inbox_sup_state (supervisor_id=? AND state=?)")),
            "{name}: {details:#?}"
        );
    }
}

// ------------------------------------------------ issue #831：先讀後寫的交易從 BEGIN 就拿寫鎖





// ------------------------------------------------ issue #421：重申請不再累積 pending

async fn request(p: &SqlitePool, commit: &str) -> ApprovalOutcome {
    create_approval_notifying(p, "agm-kick", "rebuild", "release rebuild", Some(commit), None, None, None, None, true).await.unwrap()
}

/// 照**寫進去的順序**列（issue #443）：本來是 `ORDER BY created_at, id`，但同一輪四筆是同一毫秒建的，
/// `created_at` 全部一樣，於是比到 `id`——ULID 在同一毫秒內不保證遞增（隨機那段說了算），
/// 「最後一筆是最新的」就變成擲硬幣，main 因此偶發紅。`rowid` 是 SQLite 的插入序，不會平手。
async fn statuses(p: &SqlitePool) -> Vec<(String, String)> {
    sqlx::query_as("SELECT target_commit, status FROM supervisor_approvals ORDER BY rowid").fetch_all(p).await.unwrap()
}

/// issue #421：kick 每個整點重試，記住舊 id 的本機狀態檔掉了就不會帶 `--supersedes`，於是每一輪
/// 開一筆新的 pending——那個晚上累積了四筆，AGM 被叫醒四次，講的都是同一件事。
/// daemon 自己認：同一個申請者、同一種用途、還 pending 的那一筆直接取代掉。
#[tokio::test]
async fn a_repeat_request_supersedes_the_pending_one_instead_of_piling_up() {
    let p = pool().await;
    let first = request(&p, "c1").await;
    let second = request(&p, "c2").await;
    assert!(second.created, "第二次是新的一筆");
    assert_eq!(second.superseded.as_deref(), Some(first.approval.id.as_str()), "舊的那筆被自動取代");
    assert_eq!(statuses(&p).await, vec![("c1".into(), "superseded".into()), ("c2".into(), "pending".into())]);
    // 只剩一筆 pending，所以 AGM 只有一則要裁示的。
    let pending: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_approvals WHERE status='pending'").fetch_one(&p).await.unwrap();
    assert_eq!(pending, 1);
    // 作廢那筆還沒送出的 `approval_requested` 一起收掉，不要叫人裁示一筆已經不算的申請。
    let open: Vec<String> = sqlx::query_scalar("SELECT event_key FROM supervisor_inbox WHERE state='pending' ORDER BY created_at")
        .fetch_all(&p)
        .await
        .unwrap();
    assert_eq!(open, vec![format!("approval:{}:requested", second.approval.id)]);
}

/// issue #1115：已經送出去（`delivered`）或補送放棄（`gave_up`）的那則 `approval_requested`，被取代時也要收掉。
/// 不收的話 `recover_unacked` 會每次叫醒 AGM 去裁示一筆已經作廢的申請，最後再推一則 `inbox_gave_up`。
#[tokio::test]
async fn superseding_closes_a_request_event_that_was_already_delivered() {
    for state in ["delivered", "gave_up"] {
        let p = pool().await;
        let first = request(&p, "c1").await;
        let first_key = format!("approval:{}:requested", first.approval.id);
        sqlx::query("UPDATE supervisor_inbox SET state=? WHERE event_key=?").bind(state).bind(&first_key).execute(&p).await.unwrap();
        let second = request(&p, "c2").await;
        assert_eq!(second.superseded.as_deref(), Some(first.approval.id.as_str()), "{state}");
        let (st, by): (String, Option<String>) = sqlx::query_as("SELECT state, acked_by FROM supervisor_inbox WHERE event_key=?")
            .bind(&first_key)
            .fetch_one(&p)
            .await
            .unwrap();
        assert_eq!((st.as_str(), by.as_deref()), ("handled", Some("daemon")), "{state}：舊的那則要收掉");
        let open: Vec<String> = sqlx::query_scalar("SELECT event_key FROM supervisor_inbox WHERE state!='handled'").fetch_all(&p).await.unwrap();
        assert_eq!(open, vec![format!("approval:{}:requested", second.approval.id)], "{state}：只剩新的那則");
    }
}

/// 申請還 pending 就被裁示（申請者自己 revoke、有人 deny、另一個角色 approve）：叫 AGM 裁示的那則
/// `approval_requested` 要跟著收掉。以前只有「被取代」那條路收它，其他裁示都留著 pending／delivered，
/// 於是 AGM 被叫醒去裁示一筆已經有結果的申請、補送也繼續，協調者不可用時 failover 還會把它改派給巡檢。
#[tokio::test]
async fn deciding_a_pending_approval_closes_the_inbox_event_that_asked_for_the_decision() {
    let p = pool().await;
    for (status, delivered) in [("revoked", false), ("denied", true), ("approved", false)] {
        let a = request(&p, &format!("c-{status}")).await.approval;
        let key = format!("approval:{}:requested", a.id);
        if delivered {
            sqlx::query("UPDATE supervisor_inbox SET state='delivered' WHERE event_key=?").bind(&key).execute(&p).await.unwrap();
        }
        decide_approval_from(&p, &a.id, "pending", status, "someone", None, None).await.unwrap().unwrap();
        let state: String = sqlx::query_scalar("SELECT state FROM supervisor_inbox WHERE event_key=?").bind(&key).fetch_one(&p).await.unwrap();
        assert_eq!(state, "handled", "{status}：裁示完就不用再叫人裁示");
    }
    // 消耗（approved→consumed）不是裁示：不動別筆的事件。
    let b = request(&p, "c-other").await.approval;
    decide_approval_from(&p, &b.id, "pending", "approved", "someone", None, None).await.unwrap().unwrap();
    let open: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE state!='handled'").fetch_one(&p).await.unwrap();
    assert_eq!(open, 0);
}

/// issue #443：同一毫秒建的幾筆，列出來要照**寫進去的順序**。
///
/// 這條是拿 [`statuses`] 的排序當守衛：ULID 在同一毫秒內只有隨機那段在變、不保證遞增，
/// 所以 `ORDER BY created_at, id` 在這種情形是擲硬幣——`four_rounds_leave_exactly_one_pending_approval`
/// 的「留下的是最新那筆」因此偶發紅（main 2026-09-24）。id 這裡故意跟寫入順序相反，
/// 換回舊排序這條會當場紅，不必靠重跑碰運氣。同型的前例：837cf7d0（決定歷程）、d9bfbeee（交辦順序）。
#[tokio::test]
async fn approvals_written_in_the_same_millisecond_are_listed_in_write_order() {
    let p = pool().await;
    let at = "2026-09-24T03:00:00.000Z";
    for (id, commit) in [("01ZZZZZZZZZZZZZZZZZZZZZZZZ", "first"), ("01AAAAAAAAAAAAAAAAAAAAAAAA", "second")] {
        sqlx::query(
            "INSERT INTO supervisor_approvals
               (id, supervisor_id, requester, requester_unverified, purpose, scope, target_commit, status, created_at, updated_at)
             VALUES (?,?, 'agm-kick', 1, 'rebuild', 'release rebuild', ?, 'pending', ?, ?)",
        )
        .bind(id)
        .bind(SUPERVISOR_ID)
        .bind(commit)
        .bind(at)
        .bind(at)
        .execute(&p)
        .await
        .unwrap();
    }
    let rows = statuses(&p).await;
    assert_eq!(rows.iter().map(|(c, _)| c.as_str()).collect::<Vec<_>>(), vec!["first", "second"], "照寫入順序，不看 ULID 的亂數段");
}

/// **issue #446**：自動取代挑「最新那筆 pending」也要照插入序。
///
/// 同毫秒兩筆 pending（兩個 `post_approval` 併發：`BEGIN DEFERRED` 下兩邊都先讀到「沒有 pending」，
/// 而 `supervisor_approvals` 沒有 `(requester, purpose, status)` 的唯一索引擋）時，舊的
/// `ORDER BY created_at DESC, id DESC` 會比到 ULID 的亂數段——取代到**比較舊**的那筆、最新的留著，
/// AGM 手上仍有兩筆 pending，正是 #421 要防的「被同一件事叫醒兩次」，而且是靜靜地錯
/// （`auto` 分支對不上時只會「不取代」，不報錯）。
///
/// 這裡兩筆的 `created_at` 完全相同、**ULID 故意跟插入順序相反**（先寫 `01ZZZ…`、後寫 `01AAA…`），
/// 所以換回舊排序會當場紅，不必靠重跑碰運氣。
#[tokio::test]
async fn the_auto_supersede_picks_the_last_written_pending_not_the_larger_ulid() {
    let p = pool().await;
    let at = "2026-09-24T05:00:00.000Z";
    // 先寫的 ULID 比較大：舊排序（id DESC）會挑到它，插入序（rowid DESC）會挑到後寫的那筆。
    for (id, commit) in [("01ZZZZZZZZZZZZZZZZZZZZZZZZ", "older"), ("01AAAAAAAAAAAAAAAAAAAAAAAA", "newer")] {
        sqlx::query(
            "INSERT INTO supervisor_approvals
               (id, supervisor_id, requester, requester_unverified, purpose, scope, target_commit, status, created_at, updated_at)
             VALUES (?,?, 'agm-kick', 1, 'rebuild', 'release rebuild', ?, 'pending', ?, ?)",
        )
        .bind(id)
        .bind(SUPERVISOR_ID)
        .bind(commit)
        .bind(at)
        .bind(at)
        .execute(&p)
        .await
        .unwrap();
    }

    let out = request(&p, "this-round").await;

    assert_eq!(out.superseded.as_deref(), Some("01AAAAAAAAAAAAAAAAAAAAAAAA"), "取代的是後寫的那筆（插入序最新），不是 ULID 比較大的");
    let by_id: std::collections::HashMap<String, String> =
        sqlx::query_as::<_, (String, String)>("SELECT id, status FROM supervisor_approvals").fetch_all(&p).await.unwrap().into_iter().collect();
    assert_eq!(by_id["01AAAAAAAAAAAAAAAAAAAAAAAA"], "superseded");
    assert_eq!(by_id["01ZZZZZZZZZZZZZZZZZZZZZZZZ"], "pending", "先寫的那筆本來就不是最新的，這一輪不動它");
    assert_eq!(by_id[&out.approval.id], "pending", "新開的這筆是 pending");
}

/// 連跑四輪只會留下一筆 pending（票上那個晚上的形狀）。
#[tokio::test]
async fn four_rounds_leave_exactly_one_pending_approval() {
    let p = pool().await;
    for c in ["c1", "c2", "c3", "c4"] {
        request(&p, c).await;
    }
    let rows = statuses(&p).await;
    assert_eq!(rows.len(), 4, "四筆都留著歷史");
    assert_eq!(rows.iter().filter(|(_, st)| st == "pending").count(), 1, "但只有一筆還要人裁示：{rows:?}");
    assert_eq!(rows.last().unwrap().1, "pending", "留下的是最新那筆");
}

/// **已經核准的不自動取代**：那是人做過的裁示，不能因為排程又跑了一輪就無聲消失
/// （部署腳本可能正要拿它開窗口）。明講 `--supersedes` 才動得了它。
#[tokio::test]
async fn an_approved_decision_is_never_superseded_automatically() {
    let p = pool().await;
    let first = request(&p, "c1").await;
    decide_approval(&p, &first.approval.id, "approved", "AGM", None, None).await.unwrap();
    let second = request(&p, "c2").await;
    assert_eq!(second.superseded, None, "沒帶 --supersedes 就不該動那筆核准");
    assert_eq!(statuses(&p).await, vec![("c1".into(), "approved".into()), ("c2".into(), "pending".into())]);
    // 明講就還是取代得掉（原本的機制沒有變）。
    let third = create_approval_superseding(&p, "agm-kick", "rebuild", "release rebuild", Some("c3"), None, None, Some(&first.approval.id), None)
        .await
        .unwrap();
    assert_eq!(third.superseded.as_deref(), Some(first.approval.id.as_str()));
}

/// 別人的申請不會被我的重申請掃掉：自動取代只認**同一個申請者、同一種用途**。
#[tokio::test]
async fn the_automatic_supersede_only_touches_my_own_requests() {
    let p = pool().await;
    let other = create_approval_notifying(&p, "someone-else", "rebuild", "s", Some("c1"), None, None, None, None, true).await.unwrap();
    let other_purpose = create_approval_notifying(&p, "agm-kick", "restart", "s", Some("c1"), None, None, None, None, true).await.unwrap();
    let mine = request(&p, "c1").await;
    assert_eq!(mine.superseded, None, "沒有自己的 pending 可取代");
    for a in [&other, &other_purpose] {
        let st: String = sqlx::query_scalar("SELECT status FROM supervisor_approvals WHERE id=?").bind(&a.approval.id).fetch_one(&p).await.unwrap();
        assert_eq!(st, "pending", "別人的／別種用途的申請不能被動到");
    }
}

/// 自動挑到的那筆如果剛好在同一瞬間被裁示掉，整筆申請**不能**因此失敗——
/// 明講 `--supersedes` 指到裁示過的才回 refuse，自動那條只是不取代。
#[tokio::test]
async fn an_automatic_target_that_slipped_away_does_not_fail_the_request() {
    let p = pool().await;
    let first = request(&p, "c1").await;
    // 模擬那一瞬間：狀態已經不是 pending 了，但自動查詢是在同一個交易裡讀的，
    // 這裡直接把它改成 cancelled，相當於自動挑中之後才發現不能取代。
    sqlx::query("UPDATE supervisor_approvals SET status='cancelled' WHERE id=?").bind(&first.approval.id).execute(&p).await.unwrap();
    let second = request(&p, "c2").await;
    assert!(second.created, "還是要開得出新的一筆");
    assert_eq!(second.superseded, None);
}

#[tokio::test]
async fn the_row_is_created_once_and_read_back_the_same() {
    let p = pool().await;
    let a = get_or_init(&p).await.unwrap();
    let b = get_or_init(&p).await.unwrap();
    assert_eq!(a.id, SUPERVISOR_ID);
    assert_eq!(a.created_at, b.created_at, "a second call must not replace the row");
    assert_eq!((a.identity.as_str(), a.effort.as_str(), a.active_model.as_str()), ("cc0", "low", "fable"));
}

/// The throttle's clock lives in the row, so a daemon restart does not buy the manager an
/// extra wake-up.
#[tokio::test]
async fn the_last_wake_up_is_remembered_across_reads() {
    let p = pool().await;
    assert!(get_or_init(&p).await.unwrap().last_notify_at.is_none(), "never woken yet");
    set_last_notify(&p, "2026-09-12T10:00:00Z").await.unwrap();
    assert_eq!(get_or_init(&p).await.unwrap().last_notify_at.as_deref(), Some("2026-09-12T10:00:00Z"));
}

/// issue #71：派送途中被裁示掉的交辦，不可以被那次派送的結果弄活過來。
///
/// `dispatch` 讀到一筆 `queued`，中間 `prompt_relayed_queueable` 有好幾個 await；那段時間裡
/// AGM 或使用者取消它是做得到的。以前 `mark_delivered` 是 `WHERE id=?`，會無條件把
/// `cancelled` 蓋回 `delivered`——結案的工作看起來又在跑了。
#[tokio::test]
async fn a_decided_assignment_is_not_revived_by_a_late_delivery() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    for (decided, label) in [("cancelled", "取消"), ("completed", "驗收通過"), ("failed", "判定失敗"), ("superseded", "被接續取代")] {
        let a = insert_assignment(&p, None, "bot1", &format!("req-{decided}"), "x", &[], None, true).await.unwrap();
        sqlx::query("UPDATE supervisor_assignments SET status=? WHERE id=?")
            .bind(decided)
            .bind(&a.id)
            .execute(&p)
            .await
            .unwrap();

        let moved = mark_delivered(&p, &a.id, "t-late", "ok").await.unwrap();
        assert!(!moved, "{label}：遲到的送達不該寫進去");
        let after = assignment(&p, &a.id).await.unwrap().unwrap();
        assert_eq!(after.status, decided, "{label}：狀態要留在裁示的結果");
        assert!(after.turn_id.is_none(), "{label}：也不該把 turn 綁上去");
    }
}

/// 在途的三個狀態照樣記得下去——重派同一個 crid 本來就是冪等的，會再記一次。
#[tokio::test]
async fn an_assignment_still_in_flight_records_its_delivery() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    for from in super::assignment_state::IN_FLIGHT {
        let a = insert_assignment(&p, None, "bot1", &format!("req-{from}"), "x", &[], None, true).await.unwrap();
        sqlx::query("UPDATE supervisor_assignments SET status=? WHERE id=?").bind(from).bind(&a.id).execute(&p).await.unwrap();
        assert!(mark_delivered(&p, &a.id, "t-1", "ok").await.unwrap(), "{from} 要記得下去");
        assert_eq!(assignment(&p, &a.id).await.unwrap().unwrap().status, "delivered");
    }
}

/// 結案之後再裁示一次不會覆蓋原本的裁示（`review` 自己查轉移表）。
#[tokio::test]
async fn a_second_decision_on_a_settled_assignment_is_refused() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let a = insert_assignment(&p, None, "bot1", "req-twice", "x", &[], None, true).await.unwrap();
    let first = review(&p, &a.id, "accept", "AGM", "cli", Some("好了"), None, None).await.unwrap();
    assert!(first.is_some(), "第一次裁示成立");
    let again = review(&p, &a.id, "fail", "AGM", "cli", Some("反悔"), None, None).await.unwrap();
    assert!(again.is_none(), "已經結案，第二次裁示要被擋下來");
    let after = assignment(&p, &a.id).await.unwrap().unwrap();
    assert_eq!(after.status, "completed", "留在第一次的結果");
    assert_eq!(after.review_reason.as_deref(), Some("好了"), "理由也不該被蓋掉");
}

/// The whole point of the client_request_id: a retry after a crash is the same assignment.
#[tokio::test]
async fn one_client_request_id_is_one_assignment() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let a = insert_assignment(&p, None, "bot1", "req-1", "do the thing", &[], None, true).await.unwrap();
    assert_eq!(a.status, "queued");
    assert!(insert_assignment(&p, None, "bot1", "req-1", "do the thing", &[], None, true).await.is_err());
    assert_eq!(assignment_by_crid(&p, "req-1").await.unwrap().unwrap().id, a.id);
}

/// 通知（`expects_review=false`）：送到、回合結束就自己結案。
///
/// 2026-09-12 兩件 `assignment_stalled` incident 的根因——AGM 對 bot 說一句「收到」也被
/// 當成一份要驗收的工作，沒人 review 就被 controller 報成卡住。
#[tokio::test]
async fn a_notice_closes_itself_when_the_turn_ends() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let a = insert_assignment(&p, None, "bot1", "notice-1", "收到，進 idle", &[], None, false).await.unwrap();
    assert!(a.is_notice());
    mark_delivered(&p, &a.id, "t1", "ok").await.unwrap();
    let s = settle_and_notify(&p, &a.id, "completed", true, Some("好"), None, "k1", "assignment_completed", &json!({}))
        .await
        .unwrap();
    assert!(s.moved);
    let after = assignment(&p, &a.id).await.unwrap().unwrap();
    assert_eq!(after.status, "completed", "通知不需要驗收");
    assert!(after.completed_at.is_some());
    assert_eq!(awaiting_review_count(&p).await.unwrap(), 0);
    // inbox 事件還是有（AGM 想看得到），但它不是待辦。
    let ev = pending_inbox(&p).await.unwrap();
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].kind, "assignment_noticed");
    let payload: Value = serde_json::from_str(&ev[0].payload_json).unwrap();
    assert_eq!(payload["needs_review"], json!(false));
}

/// …但**失敗**的通知還是要有人看：送不出去這件事本身是壞消息。
#[tokio::test]
async fn a_notice_that_failed_still_waits_for_agm() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let a = insert_assignment(&p, None, "bot1", "notice-2", "看完即可", &[], None, false).await.unwrap();
    settle_and_notify(&p, &a.id, "dispatch_failed", true, None, Some("bot has no active run"), "k2", "assignment_failed", &json!({}))
        .await
        .unwrap();
    let after = assignment(&p, &a.id).await.unwrap().unwrap();
    assert_eq!(after.status, "awaiting_review");
    assert_eq!(awaiting_review_count(&p).await.unwrap(), 1);
}

/// 一般交辦不受影響：回合結束照樣停在 awaiting_review。
#[tokio::test]
async fn a_task_still_waits_for_acceptance() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let a = insert_assignment(&p, None, "bot1", "task-1", "做這個", &[], None, true).await.unwrap();
    settle_and_notify(&p, &a.id, "completed", true, Some("做完了"), None, "k3", "assignment_completed", &json!({}))
        .await
        .unwrap();
    assert_eq!(assignment(&p, &a.id).await.unwrap().unwrap().status, "awaiting_review");
}

/// 撞到用量上限不是失敗也不是完成：停在 `quota_blocked`，等額度回來自己重送。
#[tokio::test]
async fn a_quota_block_parks_and_then_resumes_with_a_fresh_crid() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let a = insert_assignment(&p, None, "bot1", "job-1", "跑那個批次", &[], None, true).await.unwrap();
    mark_delivered(&p, &a.id, "t1", "ok").await.unwrap();
    let parked = park_quota_blocked(&p, &a.id, "2026-09-13T01:00:00Z", "撞到上限", "qb:1", &json!({}))
        .await
        .unwrap();
    assert!(parked.moved && parked.event_new);
    let blocked = assignment(&p, &a.id).await.unwrap().unwrap();
    assert_eq!(blocked.status, "quota_blocked");
    assert_eq!(blocked.resume_at.as_deref(), Some("2026-09-13T01:00:00Z"));
    assert!(blocked.is_open(), "工作還沒做完，只是在等額度");
    // 這一段時間內它不算「卡住」——incident 探針看不到它。
    assert!(assignments_idle_since(&p, "2999-01-01T00:00:00Z").await.unwrap().iter().all(|x| x.id != a.id));

    let resumed = resume_quota_blocked(&p, &a.id, "qr:1", &json!({})).await.unwrap();
    assert!(resumed.moved);
    let back = assignment(&p, &a.id).await.unwrap().unwrap();
    assert_eq!(back.status, "queued");
    assert_eq!(back.quota_retries, 1);
    assert!(back.turn_id.is_none(), "下一次是新的 turn");
    // crid 換過了，否則 `lifecycle::prompt` 會直接把上一個撞牆的 turn 還回來。
    assert_eq!(back.dispatch_crid(), "job-1#r1");
    assert_eq!(a.dispatch_crid(), "job-1");
    // 兩則通知都在（擋下 / 重送），而且都不是待辦。
    let kinds: Vec<String> = pending_inbox(&p).await.unwrap().into_iter().map(|e| e.kind).collect();
    assert!(kinds.contains(&"assignment_quota_blocked".to_string()));
    assert!(kinds.contains(&"assignment_quota_resumed".to_string()));
}

/// issue #71 第二刀時發現：額度回來重送前，AGM 已經先把這筆交辦取消掉了——guard 擋下這次
/// 轉移是對的，但以前不看 `moved` 就照樣推一則「額度回來、重新排隊」的通知，跟
/// `a_cancelled_assignment_gets_no_stale_completion_notification` 是同一種形狀的縫。
#[tokio::test]
async fn a_decided_assignment_gets_no_stale_quota_resume_notification() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let a = insert_assignment(&p, None, "bot1", "req-race3", "做 A", &[], None, true).await.unwrap();
    mark_delivered(&p, &a.id, "turn-race3", "ok").await.unwrap();
    park_quota_blocked(&p, &a.id, "2026-09-13T01:00:00Z", "撞到上限", "qb:race3", &json!({})).await.unwrap();
    review_with_followup(&p, &a.id, "quota_blocked", "cancel", "AGM", "cli", Some("改主意"), None, None)
        .await
        .unwrap()
        .expect("cancel 先落地");

    let resumed = resume_quota_blocked(&p, &a.id, "qr:race3", &json!({})).await.unwrap();
    assert!(!resumed.moved, "guard 要擋下：這一列已經不是 quota_blocked");
    assert!(!resumed.event_new, "沒有真的轉移，不該生出一則新事件");
    assert_eq!(assignment(&p, &a.id).await.unwrap().unwrap().status, "cancelled", "guard 擋下的不會被蓋回 queued");

    let kinds: Vec<String> = pending_inbox(&p).await.unwrap().into_iter().map(|e| e.kind).collect();
    assert!(!kinds.contains(&"assignment_quota_resumed".to_string()), "已經取消的交辦不該讓 AGM 收到「重新排隊」的通知");
}

/// 撞額度收成的那一刻，AGM 已經先把這筆交辦取消掉了：guard 擋下 `quota_blocked` 是對的，
/// 但通知也不能照推——「這件在等額度、時間到自己重送」講的是一件已經結案、不會再重送的工作
/// （跟 `a_decided_assignment_gets_no_stale_quota_resume_notification` 同一種縫，換成 park 這一頭）。
#[tokio::test]
async fn a_decided_assignment_gets_no_stale_quota_blocked_notification() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let a = insert_assignment(&p, None, "bot1", "req-park-race", "做 A", &[], None, true).await.unwrap();
    mark_delivered(&p, &a.id, "turn-park-race", "ok").await.unwrap();
    review_with_followup(&p, &a.id, "delivered", "cancel", "AGM", "cli", Some("改主意"), None, None)
        .await
        .unwrap()
        .expect("cancel 先落地");

    let parked = park_quota_blocked(&p, &a.id, "2026-09-13T01:00:00Z", "撞到上限", "qb:park-race", &json!({})).await.unwrap();
    assert!(!parked.moved, "guard 要擋下：這一列已經不是在途");
    assert!(!parked.event_new, "沒有真的轉移，不該生出一則新事件");
    assert_eq!(assignment(&p, &a.id).await.unwrap().unwrap().status, "cancelled");
    let kinds: Vec<String> = pending_inbox(&p).await.unwrap().into_iter().map(|e| e.kind).collect();
    assert!(!kinds.contains(&"assignment_quota_blocked".to_string()), "已經取消的交辦不該讓 AGM 收到「在等額度」的通知：{kinds:?}");
}

/// 同一次擋下重放幾遍只會有一則通知（event_key 帶重送次數）。
#[tokio::test]
async fn parking_twice_does_not_wake_agm_twice() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let a = insert_assignment(&p, None, "bot1", "job-2", "x", &[], None, true).await.unwrap();
    let first = park_quota_blocked(&p, &a.id, "2026-09-13T01:00:00Z", "撞到上限", "qb:2", &json!({})).await.unwrap();
    let again = park_quota_blocked(&p, &a.id, "2026-09-13T01:00:00Z", "撞到上限", "qb:2", &json!({})).await.unwrap();
    assert!(first.event_new);
    assert!(!again.event_new, "重放不再通知一次");
    assert!(!again.moved, "已經在 quota_blocked 了");
}

/// A replayed turn event — or the restart rescan seeing the same completion — must not
/// wake the manager twice about one result.
#[tokio::test]
async fn the_same_event_key_only_lands_once() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let first = push_inbox(&p, "turn:t1:completed", "assignment_completed", None, None, None, &json!({})).await.unwrap();
    let again = push_inbox(&p, "turn:t1:completed", "assignment_completed", None, None, None, &json!({})).await.unwrap();
    assert!(first.is_some());
    assert!(again.is_none(), "a duplicate event is dropped, not queued again");
    assert_eq!(pending_inbox(&p).await.unwrap().len(), 1);
}

/// Delivered ≠ handled: only an ack closes an event, so a notify the manager never
/// managed to answer stays owed to it.
#[tokio::test]
async fn delivery_does_not_close_an_event_but_an_ack_does() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let id = push_inbox(&p, "k1", "assignment_completed", None, None, None, &json!({})).await.unwrap().unwrap();
    mark_delivered_inbox(&p, &[id.clone()], "turn-9", "ok").await.unwrap();
    assert!(pending_inbox(&p).await.unwrap().is_empty(), "delivered events are not offered again");
    assert_eq!(pending_count(&p).await.unwrap(), 1, "but they are still outstanding");
    assert!(ack_inbox(&p, &id).await.unwrap());
    assert_eq!(pending_count(&p).await.unwrap(), 0);
    assert!(!ack_inbox(&p, "nope").await.unwrap());
}

#[tokio::test]
async fn pending_counts_open_work_as_well_as_unhandled_news() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let a = insert_assignment(&p, None, "bot1", "req-1", "x", &[], None, true).await.unwrap();
    assert_eq!(pending_count(&p).await.unwrap(), 1);
    mark_delivered(&p, &a.id, "t1", "ok").await.unwrap();
    assert_eq!(assignment(&p, &a.id).await.unwrap().unwrap().status, "delivered");
    assert_eq!(pending_count(&p).await.unwrap(), 1, "delivered work is still open work");
    settle_and_notify(&p, &a.id, "completed", true, Some("done"), None, "k-done", "assignment_completed", &json!({}))
        .await
        .unwrap();
    assert_eq!(
        pending_count(&p).await.unwrap(),
        2,
        "a finished turn is still open work, plus the notification nobody has read"
    );
    review(&p, &a.id, "accept", "AGM", "cli", Some("編譯通過"), Some("turn t1"), None).await.unwrap();
    let done = assignment(&p, &a.id).await.unwrap().unwrap();
    assert_eq!(done.status, "completed");
    assert_eq!(done.result.as_deref(), Some("done"));
    assert_eq!(open_assignment_count(&p).await.unwrap(), 0, "accepted work leaves the open list");
}

/// 意圖沒寫到任何列一樣是失敗（issue #84）：呼叫端會照這個回傳去做啟停的副作用，而看門狗讀到的
/// 還是舊的意圖。`Ok` 的意思必須是「重啟之後看門狗看得到這個決定」。
#[tokio::test]
async fn an_intent_write_that_matches_no_row_is_an_error_not_a_silent_success() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    set_desired_running(&p, true).await.unwrap();

    sqlx::query("DELETE FROM supervisors WHERE id=?").bind(SUPERVISOR_ID).execute(&p).await.unwrap();
    let err = set_desired_running(&p, false).await.unwrap_err();
    assert!(format!("{err}").contains("matched no supervisor row"), "{err}");
}

/// Giving up is reported once, not once per tick (review #31), and a recovery re-arms it so
/// the *next* outage is not silenced by a marker left over from the last one.
#[tokio::test]
async fn the_watchdog_reports_giving_up_once_and_a_recovery_re_arms_it() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    set_desired_running(&p, true).await.unwrap();
    set_watchdog(&p, 5, None).await.unwrap();

    assert!(mark_watchdog_gave_up(&p, "CLI 起來就死").await.unwrap(), "the first tick reports");
    for _ in 0..50 {
        assert!(!mark_watchdog_gave_up(&p, "CLI 起來就死").await.unwrap(), "later ticks stay silent");
    }
    let s = get_or_init(&p).await.unwrap();
    assert!(s.watchdog_gave_up_at.is_some());
    assert_eq!(s.watchdog_last_error.as_deref(), Some("CLI 起來就死"));

    // Seen answering again: the streak resets and so does the give-up record.
    set_watchdog(&p, 0, None).await.unwrap();
    let s = get_or_init(&p).await.unwrap();
    assert!(s.watchdog_gave_up_at.is_none(), "a recovery clears it");
    assert!(s.watchdog_last_error.is_none());
    assert!(mark_watchdog_gave_up(&p, "第二次").await.unwrap(), "so a later outage is reported again");

    // A human deciding (start / stop) also clears it.
    set_desired_running(&p, true).await.unwrap();
    let s = get_or_init(&p).await.unwrap();
    assert!(s.watchdog_gave_up_at.is_none());
    assert_eq!((s.watchdog_attempts, s.watchdog_next_at), (0, None));

    // A non-zero update must *not* clear it: that is the retry bookkeeping, not a recovery.
    mark_watchdog_gave_up(&p, "第三次").await.unwrap();
    set_watchdog(&p, 5, Some("2099-01-01T00:00:00Z")).await.unwrap();
    assert!(get_or_init(&p).await.unwrap().watchdog_gave_up_at.is_some());
}

/// The stopped-bot case (review #30). Every retry calls `defer`, which moves `updated_at`,
/// so work bouncing off a dead bot every five minutes looks *busy* forever and the idle
/// probe never sees it. The undelivered probe clocks it from `created_at` instead.
#[tokio::test]
async fn work_that_keeps_retrying_is_visible_even_though_it_never_looks_idle() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let a = insert_assignment(&p, None, "stopped-bot", "req-r", "做事", &[], None, true).await.unwrap();
    // Created long ago; retried 20 times, the last one just now. `defer` stamps
    // `updated_at` off the real clock, so the cutoff has to sit between the two: older than
    // "now" (or the idle probe would match anything) and newer than `created_at`.
    sqlx::query("UPDATE supervisor_assignments SET created_at='2020-01-01T00:00:00.000Z' WHERE id=?")
        .bind(&a.id)
        .execute(&p)
        .await
        .unwrap();
    for _ in 0..20 {
        defer(&p, &a.id, "2099-01-01T00:00:00Z", "bot has no active run").await.unwrap();
    }
    let cutoff = (chrono::Utc::now() - chrono::Duration::seconds(60))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let cutoff = cutoff.as_str();
    assert!(
        assignments_idle_since(&p, cutoff).await.unwrap().is_empty(),
        "the idle probe is blind to it: every retry refreshes updated_at"
    );
    let seen = assignments_undelivered_since(&p, cutoff).await.unwrap();
    assert_eq!(seen.len(), 1, "but never-delivered work is clocked from created_at");
    assert_eq!(seen[0].attempts, 20);
    assert_eq!(seen[0].error.as_deref(), Some("bot has no active run"), "with the real reason, not `conflict`");
    assert_eq!(seen[0].to_json()["next_attempt_at"], "2099-01-01T00:00:00Z", "and when it will try again");

    // Once it actually goes out it is no longer undelivered, whatever happens next.
    mark_delivered(&p, &a.id, "t1", "ok").await.unwrap();
    assert!(assignments_undelivered_since(&p, cutoff).await.unwrap().is_empty());
}

/// Cancelling work that is or may be running is allowed — and must not pretend it un-sent
/// anything. The transport facts survive the decision precisely so nobody has to guess.
#[tokio::test]
async fn cancelling_unknown_delivery_keeps_the_evidence_that_it_may_be_running() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let a = insert_assignment(&p, None, "bot1", "req-u", "x", &[], None, true).await.unwrap();
    mark_delivered(&p, &a.id, "t7", "unknown").await.unwrap();
    assert_eq!(assignment(&p, &a.id).await.unwrap().unwrap().status, "unknown");

    review(&p, &a.id, "cancel", "AGM", "cli", Some("使用者改主意"), Some("送達未知，沒有中止回合"), None)
        .await
        .unwrap();
    let a = assignment(&p, &a.id).await.unwrap().unwrap();
    assert_eq!(a.status, "cancelled");
    assert!(!a.is_open());
    // The two facts that stop `cancelled` from being read as "it never went out".
    assert_eq!(a.delivery.as_deref(), Some("unknown"), "delivery is not rewritten by the decision");
    assert_eq!(a.turn_id.as_deref(), Some("t7"), "and the turn is still named, so it can be checked");
    let log = reviews(&p, &a.id).await.unwrap();
    assert!(log[0]["evidence"].as_str().unwrap().contains("沒有中止回合"));
}

/// The heart of the 2026-09-12 P1: a turn ending is not a job being done.
///
/// `01M246903Z54XW872GWD7XXJAE` answered "still waiting on the build, will report back" and
/// the row said `completed`. Now that reply parks the assignment on `awaiting_review`, where
/// it stays on the open list until somebody decides.
#[tokio::test]
async fn a_finished_turn_waits_for_acceptance_instead_of_closing_itself() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let a = insert_assignment(&p, None, "bot1", "req-a", "改好那個 bug", &[], None, true).await.unwrap();
    mark_delivered(&p, &a.id, "t1", "ok").await.unwrap();
    let s = settle_and_notify(
        &p,
        &a.id,
        "completed",
        true,
        Some("還在等編譯，等一下回報"),
        None,
        "assignment_completed:a:t1",
        "assignment_completed",
        &json!({"result": "還在等編譯"}),
    )
    .await
    .unwrap();
    assert_eq!(s, Settled { moved: true, event_new: true });
    let a = assignment(&p, &a.id).await.unwrap().unwrap();
    assert_eq!(a.status, "awaiting_review");
    assert!(a.is_open(), "still owed to the manager");
    assert!(!a.is_executing(), "but the daemon has nothing left to do for it");
    assert_eq!(a.turn_status.as_deref(), Some("completed"), "the raw fact is kept");
    assert_eq!(awaiting_review_count(&p).await.unwrap(), 1);
    assert!(open_assignments(&p).await.unwrap().is_empty(), "not on the controller's work list");
    assert_eq!(unsettled_assignments(&p).await.unwrap().len(), 1, "but on the manager's");

    // "Still waiting on the build" is not an acceptance: blocking keeps it open.
    review(&p, &a.id, "block", "AGM", "cli", Some("等編譯"), None, None).await.unwrap();
    let a = assignment(&p, &a.id).await.unwrap().unwrap();
    assert_eq!(a.status, "blocked");
    assert!(a.is_open());
    assert_eq!(open_assignment_count(&p).await.unwrap(), 1);
}

/// Every decision names who made it, on what grounds. Without that the state machine is
/// just a different way of asserting the same unfalsifiable "it is done".
#[tokio::test]
async fn every_decision_is_recorded_with_its_actor_and_evidence() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let a = insert_assignment(&p, None, "bot1", "req-b", "x", &[], None, true).await.unwrap();
    settle_and_notify(&p, &a.id, "completed", true, Some("ok"), None, "k", "assignment_completed", &json!({}))
        .await
        .unwrap();
    review(&p, &a.id, "accept", "AGM", "cli", Some("測試通過"), Some("commit abc123"), None).await.unwrap();
    let log = reviews(&p, &a.id).await.unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0]["decision"], "accept");
    assert_eq!(log[0]["from_status"], "awaiting_review");
    assert_eq!(log[0]["to_status"], "completed");
    assert_eq!(log[0]["actor"], "AGM");
    assert_eq!(log[0]["evidence"], "commit abc123");
}

/// A continuation is a new assignment pointing back at the old one — never a rewrite of
/// text that has already been sent to a bot.
#[tokio::test]
async fn a_follow_up_is_a_new_assignment_linked_to_the_old_one() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let first = insert_assignment(&p, None, "bot1", "req-1", "做 A", &[], None, true).await.unwrap();
    settle_and_notify(&p, &first.id, "completed", true, Some("A 做了一半"), None, "k1", "assignment_completed", &json!({}))
        .await
        .unwrap();
    let next = insert_assignment(&p, None, "bot1", "req-1-follow", "把 A 做完", &[], Some(&first.id), true).await.unwrap();
    link_followup(&p, &first.id, &next.id).await.unwrap();
    review(&p, &first.id, "followup", "AGM", "cli", Some("還差一半"), None, Some(&next.id)).await.unwrap();
    let first = assignment(&p, &first.id).await.unwrap().unwrap();
    assert_eq!(first.status, "superseded");
    assert_eq!(first.followup_assignment_id.as_deref(), Some(next.id.as_str()));
    assert!(!first.is_open(), "the original is closed…");
    let next = assignment(&p, &next.id).await.unwrap().unwrap();
    assert_eq!(next.follow_up_of.as_deref(), Some(first.id.as_str()));
    assert!(next.is_open(), "…because the follow-up carries the work");
    assert_eq!(first.text, "做 A", "the delivered words are never rewritten");
}

fn spec<'a>(target: &'a str, crid: &'a str, text: &'a str, owns: &'a [String]) -> FollowupSpec<'a> {
    FollowupSpec { target_bot_id: target, client_request_id: crid, text, ownership: owns, request_id: None }
}

/// Two callers deciding the same assignment at once must not each派 a continuation. The
/// guard is the status the caller validated against: the loser matches no rows, rolls back
/// whole, and is told it lost — *before* anything has been sent to a bot.
#[tokio::test]
async fn two_concurrent_followups_on_one_parent_create_exactly_one_continuation() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let parent = insert_assignment(&p, None, "bot1", "req-p", "做 A", &[], None, true).await.unwrap();
    settle_and_notify(&p, &parent.id, "completed", true, Some("做一半"), None, "k", "assignment_completed", &json!({}))
        .await
        .unwrap();

    let first = review_with_followup(
        &p, &parent.id, "awaiting_review", "followup", "AGM", "cli", Some("還差一半"), None,
        Some(spec("bot1", "follow-A", "把 A 做完", &[])),
    )
    .await
    .unwrap()
    .expect("the first decision wins");
    assert_eq!(first.updated.status, "superseded");
    let first_child = first.followup.expect("a continuation was created");

    // The second caller read the same `awaiting_review` and is deciding with a *different*
    // request id — the case that used to produce two continuations.
    let second = review_with_followup(
        &p, &parent.id, "awaiting_review", "followup", "AGM", "web", Some("我也覺得要續作"), None,
        Some(spec("bot1", "follow-B", "另一份續作", &[])),
    )
    .await
    .unwrap();
    assert!(second.is_none(), "the loser writes nothing at all");

    let all = list_assignments(&p, 50).await.unwrap();
    assert_eq!(all.len(), 2, "one parent, one continuation — not two");
    assert!(all.iter().all(|a| a.client_request_id != "follow-B"), "the loser's row was never inserted");
    assert_eq!(reviews(&p, &parent.id).await.unwrap().len(), 1, "and no second audit entry");
    assert_eq!(
        assignment(&p, &parent.id).await.unwrap().unwrap().followup_assignment_id.as_deref(),
        Some(first_child.id.as_str())
    );

    // A byte-identical retry of the winner is idempotent at the assignment level: the same
    // request id finds the row that already exists rather than making a second one.
    let retry = review_with_followup(
        &p, &parent.id, "superseded", "followup", "AGM", "cli", Some("重送"), None,
        Some(spec("bot1", "follow-A", "把 A 做完", &[])),
    )
    .await
    .unwrap()
    .expect("the parent is still where this caller thinks it is");
    assert_eq!(retry.followup.unwrap().id, first_child.id);
    assert_eq!(list_assignments(&p, 50).await.unwrap().len(), 2, "still two rows");
}

/// If any part of the decision fails, none of it lands: no superseded parent, no audit row,
/// and above all no queued continuation that nobody is tracking.
#[tokio::test]
async fn a_failed_followup_rolls_back_and_leaves_no_orphan_queued_row() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let parent = insert_assignment(&p, None, "bot1", "req-p", "做 A", &[], None, true).await.unwrap();
    settle_and_notify(&p, &parent.id, "completed", true, Some("做一半"), None, "k", "assignment_completed", &json!({}))
        .await
        .unwrap();
    // Somebody else's assignment already owns this client_request_id.
    let other = insert_assignment(&p, None, "bot2", "taken-crid", "別人的工作", &[], None, true).await.unwrap();

    let err = review_with_followup(
        &p, &parent.id, "awaiting_review", "followup", "AGM", "cli", Some("續作"), None,
        Some(spec("bot1", "taken-crid", "把 A 做完", &[])),
    )
    .await;
    let taken = err.as_ref().err().and_then(|e| e.downcast_ref::<FollowupIdTaken>());
    assert!(
        taken.is_some_and(|t| t.assignment_id == other.id),
        "adopting someone else's request id is refused with a named error: {:?}",
        err.as_ref().err().map(ToString::to_string)
    );

    let parent_now = assignment(&p, &parent.id).await.unwrap().unwrap();
    assert_eq!(parent_now.status, "awaiting_review", "the parent was not superseded");
    assert!(parent_now.review_decision.is_none(), "and no decision was recorded");
    assert!(parent_now.followup_assignment_id.is_none());
    assert!(reviews(&p, &parent.id).await.unwrap().is_empty(), "no audit row either");
    let all = list_assignments(&p, 50).await.unwrap();
    assert_eq!(all.len(), 2, "no orphan continuation was left behind");
    assert!(all.iter().all(|a| a.follow_up_of.is_none()));
    assert_eq!(assignment(&p, &other.id).await.unwrap().unwrap().text, "別人的工作", "and the other row is untouched");
}

/// 通知送不出去再 followup：續作仍是通知，不會變回要驗收、兩小時後被當成卡住的工單（review 2026-09-16 c1 L6）。
#[tokio::test]
async fn a_followup_of_a_notice_is_still_a_notice() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let notice = insert_assignment(&p, None, "bot1", "hello-1", "收到，進 idle", &[], None, false).await.unwrap();
    let task = insert_assignment(&p, None, "bot1", "task-1", "做 A", &[], None, true).await.unwrap();
    for (parent, crid) in [(&notice, "hello-1-r"), (&task, "task-1-r")] {
        let d = review_with_followup(&p, &parent.id, "queued", "followup", "AGM", "cli", None, None, Some(spec("bot1", crid, "再送一次", &[])))
            .await
            .unwrap()
            .unwrap();
        let child = d.followup.unwrap();
        assert_eq!(child.expects_review, parent.expects_review, "{crid}");
    }
}

/// The transactional outbox: either the assignment moves and the event is queued, or
/// neither happens. A replay of the same outcome changes nothing.
#[tokio::test]
async fn settling_twice_moves_nothing_and_queues_one_event() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let a = insert_assignment(&p, None, "bot1", "req-c", "x", &[], None, true).await.unwrap();
    mark_delivered(&p, &a.id, "t9", "ok").await.unwrap();
    let key = "assignment_completed:a:t9";
    let first = settle_and_notify(&p, &a.id, "completed", true, Some("r"), None, key, "assignment_completed", &json!({}))
        .await
        .unwrap();
    let again = settle_and_notify(&p, &a.id, "completed", true, Some("r"), None, key, "assignment_completed", &json!({}))
        .await
        .unwrap();
    assert_eq!(first, Settled { moved: true, event_new: true });
    assert_eq!(again, Settled { moved: false, event_new: false }, "a duplicate completion is not a second event");
    assert_eq!(pending_inbox(&p).await.unwrap().len(), 1);
}

/// If the notification cannot be written, the assignment does not move either.
///
/// The failure is forced by dropping the inbox table — crude, but it exercises the real
/// rollback path, and the property is the one that matters: there is no state in which the
/// work is closed and nobody was told.
#[tokio::test]
async fn a_failed_notification_rolls_the_whole_settle_back() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let a = insert_assignment(&p, None, "bot1", "req-tx", "x", &[], None, true).await.unwrap();
    mark_delivered(&p, &a.id, "t1", "ok").await.unwrap();
    sqlx::query("DROP TABLE supervisor_inbox").execute(&p).await.unwrap();
    let err = settle_and_notify(&p, &a.id, "completed", true, Some("done"), None, "k", "assignment_completed", &json!({}))
        .await;
    assert!(err.is_err(), "the write failed");
    let a = assignment(&p, &a.id).await.unwrap().unwrap();
    assert_eq!(a.status, "delivered", "so the assignment is still open and will be reconciled again");
    assert!(a.turn_status.is_none(), "and nothing of the half-written outcome survived");
}

/// A result closed by an older daemon, with nobody ever told: the sweep finds it. Once the
/// write is transactional this can only be a pre-upgrade row — which is exactly the backlog
/// the review was worried about.
#[tokio::test]
async fn a_settled_assignment_with_no_event_is_found_by_the_sweep() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let a = insert_assignment(&p, None, "bot1", "req-d", "x", &[], None, true).await.unwrap();
    sqlx::query("UPDATE supervisor_assignments SET status='awaiting_review', turn_status='completed' WHERE id=?")
        .bind(&a.id)
        .execute(&p)
        .await
        .unwrap();
    assert_eq!(settled_without_event(&p, 10).await.unwrap().len(), 1);
    push_inbox(&p, "k", "assignment_completed", Some(&a.id), None, None, &json!({})).await.unwrap();
    assert!(settled_without_event(&p, 10).await.unwrap().is_empty(), "once queued it is not swept again");
}

/// #76：AGM 的 cancel 決定跟遲到的回合收尾（`settle_and_notify`）撞在一起——AGM 在回合結束的
/// 通知還沒送達前就決定 cancel（`review_with_followup` 先落地，是真正的 CAS 寫入，不是模擬），
/// 之後 `settle_and_notify` 才因為某個延遲的事件被呼叫。它自己的 guard
/// （`WHERE status IN ('queued','delivered','unknown')`）要擋下這一列，不能把已經 cancel
/// 的交辦蓋回 awaiting_review。用真的循序呼叫，不是 trigger：這裡要測的正是那句 guard 本身
/// 有沒有咬住，trigger 換掉的是「哪個值贏」，不是「guard 擋不擋得住」，兩者不一樣。
#[tokio::test]
async fn a_cancelled_assignment_is_not_resurrected_by_a_late_settle() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let a = insert_assignment(&p, None, "bot1", "req-race", "做 A", &[], None, true).await.unwrap();
    mark_delivered(&p, &a.id, "turn-race", "ok").await.unwrap();
    assert_eq!(assignment(&p, &a.id).await.unwrap().unwrap().status, "delivered");

    let cancelled = review_with_followup(&p, &a.id, "delivered", "cancel", "AGM", "cli", Some("改主意"), None, None)
        .await
        .unwrap()
        .expect("cancel 先落地");
    assert_eq!(cancelled.updated.status, "cancelled");

    // 遲到的回合收尾這時候才進來：它讀到的還是 dispatch 當下那顆舊的 assignment（`delivered`），
    // 但 `settle_and_notify` 自己會用當下 DB 裡的值當 guard，不是呼叫端傳的參數。
    let settled = settle_and_notify(&p, &a.id, "completed", true, Some("完成"), None, "k-race", "assignment_completed", &json!({}))
        .await
        .unwrap();
    assert!(!settled.moved, "guard 要擋下：這一列已經不是 queued/delivered/unknown");

    let row = assignment(&p, &a.id).await.unwrap().unwrap();
    assert_eq!(row.status, "cancelled", "已經取消的交辦不會被遲到的收尾救回 awaiting_review");
    assert_eq!(row.review_decision.as_deref(), Some("cancel"), "取消當時寫下的裁示紀錄原封不動，沒被收尾蓋掉");
}

/// #99：上一支測試釘住「不會復活」，這支釘「不會發出過期通知」——`moved=false` 時
/// `settle_and_notify` 以前照樣會排一則 `assignment_completed`／`needs_review:true` 的通知，
/// 已經取消的交辦讓 AGM 收到「這件做完了、請驗收」的訊息。
#[tokio::test]
async fn a_cancelled_assignment_gets_no_stale_completion_notification() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let a = insert_assignment(&p, None, "bot1", "req-race2", "做 A", &[], None, true).await.unwrap();
    mark_delivered(&p, &a.id, "turn-race2", "ok").await.unwrap();
    review_with_followup(&p, &a.id, "delivered", "cancel", "AGM", "cli", Some("改主意"), None, None)
        .await
        .unwrap()
        .expect("cancel 先落地");

    let settled = settle_and_notify(
        &p, &a.id, "completed", true, Some("完成"), None, "k-race2", "assignment_completed",
        &json!({"needs_review": true}),
    )
    .await
    .unwrap();
    assert!(!settled.moved, "guard 要擋下：這一列已經不是 queued/delivered/unknown");
    assert!(!settled.event_new, "沒有真的轉移，不該生出一則新事件");

    let events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE assignment_id=?")
        .bind(&a.id)
        .fetch_one(&p)
        .await
        .unwrap();
    assert_eq!(events, 0, "已經取消的交辦不該讓 AGM 收到「這件做完了、請驗收」的通知");
}

/// `unknown` delivery is its own state precisely so the controller reconciles it instead
/// of sending the same job a second time.
#[tokio::test]
async fn unknown_delivery_is_not_folded_into_delivered() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let a = insert_assignment(&p, None, "bot1", "req-2", "x", &[], None, true).await.unwrap();
    mark_delivered(&p, &a.id, "t2", "unknown").await.unwrap();
    let a = assignment(&p, &a.id).await.unwrap().unwrap();
    assert_eq!(a.status, "unknown");
    assert!(a.is_open());
    assert_eq!(assignment_by_turn(&p, "t2").await.unwrap().unwrap().id, a.id);
}

/// A phone message that reaches the daemon twice is one request, not two.
#[tokio::test]
async fn a_replayed_source_turn_is_the_same_request() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let a = upsert_request(&p, "manager_turn", Some("turn-1"), "幫我處理登入").await.unwrap();
    let b = upsert_request(&p, "manager_turn", Some("turn-1"), "幫我處理登入").await.unwrap();
    assert_eq!(a, b);
    // Without a stable id there is nothing to dedupe on, and two asks stay two asks.
    let c = upsert_request(&p, "assignment_text_fallback", None, "同樣的字").await.unwrap();
    let d = upsert_request(&p, "assignment_text_fallback", None, "同樣的字").await.unwrap();
    assert_ne!(c, d);
}

/// A model switch retires the old controller, and only a *successful* switch moves
/// `active_model`.
#[tokio::test]
async fn switching_bumps_the_generation_and_spends_the_budget() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let gen = set_active_model(&p, "opus", Some("2099-01-01T00:00:00Z")).await.unwrap();
    assert_eq!(gen, 1);
    let s = get_or_init(&p).await.unwrap();
    assert_eq!(s.active_model, "opus");
    assert_eq!(s.fallback_tries, 1);
    clear_fallback_budget(&p).await.unwrap();
    let s = get_or_init(&p).await.unwrap();
    assert_eq!(s.fallback_tries, 0);
    assert!(s.cooldown_until.is_none());
    assert_eq!(s.generation, 1, "clearing the budget is not a switch");
}

/// The 464-event night: the manager acks in the order shown, so the list has to be the
/// open ones, oldest first — not the newest 200 of everything.
#[tokio::test]
async fn the_open_inbox_is_oldest_first_and_drains_with_acks() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    for i in 0..5 {
        let id = push_inbox(&p, &format!("k{i}"), "health_changed", None, None, None, &json!({"i": i}))
            .await
            .unwrap()
            .unwrap();
        // Distinct timestamps: `now()` has millisecond resolution and the loop is faster.
        sqlx::query("UPDATE supervisor_inbox SET created_at=? WHERE id=?")
            .bind(format!("2026-09-11T00:00:0{i}Z"))
            .bind(&id)
            .execute(&p)
            .await
            .unwrap();
    }
    let open = open_inbox(&p, 10).await.unwrap();
    assert_eq!(open.iter().map(|e| e.event_key.as_str()).collect::<Vec<_>>(), ["k0", "k1", "k2", "k3", "k4"]);
    assert_eq!(open_inbox(&p, 2).await.unwrap().len(), 2, "--limit applies");
    assert!(ack_inbox(&p, &open[0].id).await.unwrap());
    mark_delivered_inbox(&p, &[open[1].id.clone()], "t1", "ok").await.unwrap();
    let open = open_inbox(&p, 10).await.unwrap();
    assert_eq!(open.iter().map(|e| e.event_key.as_str()).collect::<Vec<_>>(), ["k1", "k2", "k3", "k4"], "delivered is still open; handled is gone");
    assert_eq!(open_inbox_count(&p).await.unwrap(), 4);
    assert_eq!(open_assignment_count(&p).await.unwrap(), 0);
    assert_eq!(pending_count(&p).await.unwrap(), 4);
}

/// A manager started before the watchdog existed is still wanted; one that was only set
/// up (generation 0) is not — the watchdog must not be the first thing that starts it.
#[tokio::test]
async fn migration_backfills_desired_running_from_a_previous_start() {
    let p = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
    // The pre-watchdog schema: the same DDL minus the three columns.
    for stmt in DDL.split(";\n") {
        let s = stmt.trim();
        if !s.is_empty() {
            sqlx::query(s).execute(&p).await.unwrap();
        }
    }
    for (id, gen, bot) in [("AGM", 2, Some("b1")), ("OLD", 0, Some("b2")), ("NONE", 3, None)] {
        sqlx::query("INSERT INTO supervisors (id, bot_id, generation, created_at, updated_at) VALUES (?,?,?,'t','t')")
            .bind(id)
            .bind(bot)
            .bind(gen)
            .execute(&p)
            .await
            .unwrap();
    }
    assert!(!has_column(&p, "supervisors", "desired_running").await.unwrap());
    migrate(&p).await.unwrap();
    migrate(&p).await.unwrap(); // idempotent
    let wanted: Vec<(String, i64)> = sqlx::query_as("SELECT id, desired_running FROM supervisors ORDER BY id")
        .fetch_all(&p)
        .await
        .unwrap();
    assert_eq!(wanted, [("AGM".to_string(), 1), ("NONE".to_string(), 0), ("OLD".to_string(), 0)]);
    let s = get_or_init(&p).await.unwrap();
    assert!(s.wants_running());
    set_desired_running(&p, false).await.unwrap();
    assert!(!get_or_init(&p).await.unwrap().wants_running());
    set_watchdog(&p, 3, Some("2026-09-11T00:00:00Z")).await.unwrap();
    set_desired_running(&p, true).await.unwrap();
    let s = get_or_init(&p).await.unwrap();
    assert_eq!((s.watchdog_attempts, s.watchdog_next_at.as_deref()), (0, None), "a fresh start clears the streak");
    set_status_detail(&p, Some("x")).await.unwrap();
    let s = get_or_init(&p).await.unwrap();
    assert_eq!((s.status.as_str(), s.status_detail.as_deref()), ("", Some("x")), "detail alone leaves status");
}

#[tokio::test]
async fn interrupted_desired_running_migration_rolls_back_and_recovers() {
    let p = pre_additive_schema().await;
    sqlx::query("INSERT INTO supervisors (id, bot_id, generation, created_at, updated_at) VALUES ('AGM','b1',2,'t','t')")
        .execute(&p).await.unwrap();
    sqlx::query("CREATE TRIGGER fail_supervisor_migration BEFORE UPDATE ON supervisors BEGIN SELECT RAISE(ABORT, 'interrupted'); END")
        .execute(&p).await.unwrap();

    assert!(migrate(&p).await.is_err(), "fault injection stops the backfill");
    assert!(!has_column(&p, "supervisors", "desired_running").await.unwrap(), "column and backfill must be atomic");
    sqlx::query("DROP TRIGGER fail_supervisor_migration").execute(&p).await.unwrap();
    migrate(&p).await.expect("the next startup must resume the migration");
    let desired: i64 = sqlx::query_scalar("SELECT desired_running FROM supervisors WHERE id='AGM'")
        .fetch_one(&p).await.unwrap();
    assert_eq!(desired, 1, "the previously started manager remains wanted");
}

#[tokio::test]
async fn interrupted_turn_status_backfill_rolls_back_and_recovers() {
    let p = pre_additive_schema().await;
    sqlx::query("INSERT INTO supervisor_assignments (id,supervisor_id,target_bot_id,client_request_id,text,status,created_at,updated_at)
                 VALUES ('a1','AGM','b1','c1','x','completed','t','t')")
        .execute(&p).await.unwrap();
    sqlx::query("CREATE TRIGGER fail_turn_status_migration BEFORE UPDATE ON supervisor_assignments BEGIN SELECT RAISE(ABORT, 'interrupted'); END")
        .execute(&p).await.unwrap();

    assert!(migrate(&p).await.is_err(), "fault injection stops the turn_status backfill");
    assert!(!has_column(&p, "supervisor_assignments", "turn_status").await.unwrap(), "column and backfill must be atomic");
    sqlx::query("DROP TRIGGER fail_turn_status_migration").execute(&p).await.unwrap();
    migrate(&p).await.expect("the next startup must resume the assignment migration");
    let row: (Option<String>, i64) = sqlx::query_as("SELECT turn_status, legacy_closed FROM supervisor_assignments WHERE id='a1'")
        .fetch_one(&p).await.unwrap();
    assert_eq!(row, (Some("completed".into()), 1));
}

#[tokio::test]
async fn interrupted_legacy_closed_backfill_rolls_back_and_recovers() {
    let p = pre_additive_schema().await;
    sqlx::query("ALTER TABLE supervisors ADD COLUMN desired_running INTEGER NOT NULL DEFAULT 0").execute(&p).await.unwrap();
    sqlx::query("ALTER TABLE supervisor_assignments ADD COLUMN turn_status TEXT").execute(&p).await.unwrap();
    sqlx::query("INSERT INTO supervisor_assignments (id,supervisor_id,target_bot_id,client_request_id,text,status,created_at,updated_at)
                 VALUES ('a1','AGM','b1','c1','x','completed','t','t')")
        .execute(&p).await.unwrap();
    sqlx::query("CREATE TRIGGER fail_legacy_closed_migration BEFORE UPDATE ON supervisor_assignments BEGIN SELECT RAISE(ABORT, 'interrupted'); END")
        .execute(&p).await.unwrap();

    assert!(migrate(&p).await.is_err(), "fault injection stops the legacy_closed backfill");
    assert!(!has_column(&p, "supervisor_assignments", "legacy_closed").await.unwrap(), "column and backfill must be atomic");
    sqlx::query("DROP TRIGGER fail_legacy_closed_migration").execute(&p).await.unwrap();
    migrate(&p).await.expect("the next startup must resume the legacy_closed migration");
    let closed: i64 = sqlx::query_scalar("SELECT legacy_closed FROM supervisor_assignments WHERE id='a1'")
        .fetch_one(&p).await.unwrap();
    assert_eq!(closed, 1);
}

/// The ACK / mark_delivered race. The manager reads the digest out of the prompt and acks
/// before the delivery write lands; without the guard that write reopens the event and the
/// same news is pushed at it again.
#[tokio::test]
async fn an_acknowledgement_never_goes_backwards() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let id = push_inbox(&p, "k1", "assignment_completed", None, None, None, &json!({})).await.unwrap().unwrap();
    assert!(ack_inbox(&p, &id).await.unwrap());
    mark_delivered_inbox(&p, &[id.clone()], "turn-1", "ok").await.unwrap();
    let e = inbox(&p, 10).await.unwrap().into_iter().find(|e| e.id == id).unwrap();
    assert_eq!(e.state, "handled", "the late delivery write must not un-ack it");
    assert_eq!(open_inbox_count(&p).await.unwrap(), 0);
}

/// Delivery is not an answer. A failed send leaves everything pending with a backoff; a
/// delivered-but-unacked event can be handed back; the retry budget is bounded, and an
/// exhausted event is still kept rather than dropped.
#[tokio::test]
async fn notifications_are_retried_a_bounded_number_of_times_and_never_dropped() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let id = push_inbox(&p, "k1", "assignment_completed", None, None, None, &json!({})).await.unwrap().unwrap();
    let ids = [id.clone()];

    // A send that failed: still pending, but not due until the backoff passes.
    defer_notify(&p, &ids, "2099-01-01T00:00:00Z", "delivery failed").await.unwrap();
    assert!(due_inbox(&p, "2026-09-12T00:00:00Z", 5).await.unwrap().is_empty(), "backoff holds it");
    assert_eq!(due_inbox(&p, "2099-06-01T00:00:00Z", 5).await.unwrap().len(), 1, "and then releases it");

    // Delivered, unanswered, handed back: the notify turn is kept so the next attempt can
    // check it instead of blindly sending a second copy.
    mark_delivered_inbox(&p, &ids, "turn-7", "unknown").await.unwrap();
    assert!(pending_inbox(&p).await.unwrap().is_empty());
    assert_eq!(delivered_inbox(&p).await.unwrap().len(), 1);
    assert!(requeue_inbox(&p, &id, "delivered but never acknowledged").await.unwrap());
    let e = pending_inbox(&p).await.unwrap().remove(0);
    assert_eq!(e.notify_turn_id.as_deref(), Some("turn-7"));
    assert_eq!(e.notify_delivery.as_deref(), Some("unknown"), "unknown delivery stays unknown");
    assert!(!requeue_inbox(&p, &id, "again").await.unwrap(), "only a delivered event can be requeued");

    // Spend the budget. The event never leaves the inbox; what stops is the pushing.
    for _ in 0..5 {
        defer_notify(&p, &ids, "2000-01-01T00:00:00Z", "nope").await.unwrap();
    }
    assert!(due_inbox(&p, "2099-01-01T00:00:00Z", 5).await.unwrap().is_empty(), "budget spent, no more sends");
    assert_eq!(exhausted_inbox(&p, 5).await.unwrap().total, 1, "and it is visible as an incident source");
    assert_eq!(open_inbox_count(&p).await.unwrap(), 1, "still owed, never swallowed");
}

/// issue #504：兩個探針的歸屬要用同一個定義（`roles::OWNER`），否則中間破一個洞。
///
/// `role='responder'` 而 `claimed_by='patrol'` 的列是**巡檢的**——單角色時期
/// `due_for(Patrol, responder_configured=false)` 撈走、`mark_delivered` 寫下 `claimed_by`，
/// 之後協調者建起來也不會搬回去（SPEC §18.15）；`roles::migrate` 的回填也會長出同一個組合。
/// 它照 `due_for` 有 `notify_attempts < max` 的上限，額度用完就不再被送。
/// 所以額度用完那一刻它必須出現在 `exhausted_inbox`，而**不能**出現在 `responder_undelivered`
/// （那是「協調者自己收不到」的探針，數字會被算進 `responder_undeliverable` 的 incident detail）。
/// 兩邊都漏掉的話，事件永遠停在 pending：沒有人被叫醒，也沒有任何 incident。
#[tokio::test]
async fn a_patrol_claimed_event_that_routes_to_the_responder_is_the_patrols_when_it_runs_out_of_retries() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    // 三種列，`notify_attempts` 都已經用完巡檢的 5 次額度。
    let claimed = push_inbox(&p, "k-claimed", "approval_requested", None, None, None, &json!({})).await.unwrap().unwrap();
    let responders = push_inbox(&p, "k-responder", "approval_requested", None, None, None, &json!({})).await.unwrap().unwrap();
    let unclassified = push_inbox(&p, "k-none", "approval_requested", None, None, None, &json!({})).await.unwrap().unwrap();
    // 巡檢收走的協調類事件：路由表說協調者，實際收的是巡檢。
    sqlx::query("UPDATE supervisor_inbox SET role='responder', claimed_by='patrol', notify_attempts=5 WHERE id=?")
        .bind(&claimed).execute(&p).await.unwrap();
    // 真的在協調者手上的（還沒送出，所以 claimed_by 還是 NULL）。
    sqlx::query("UPDATE supervisor_inbox SET role='responder', notify_attempts=5 WHERE id=?")
        .bind(&responders).execute(&p).await.unwrap();
    // 還沒分類的：兩欄都是 NULL，照舊算巡檢的。
    sqlx::query("UPDATE supervisor_inbox SET notify_attempts=5 WHERE id=?")
        .bind(&unclassified).execute(&p).await.unwrap();

    let mut exhausted: Vec<String> = exhausted_inbox(&p, 5).await.unwrap().events.into_iter().map(|e| e.id).collect();
    exhausted.sort();
    let mut want = vec![claimed.clone(), unclassified.clone()];
    want.sort();
    assert_eq!(exhausted, want, "巡檢收走的（含 role=responder 的那一筆）與還沒分類的都是巡檢的");

    let undelivered: Vec<String> = responder_undelivered(&p, 5).await.unwrap().into_iter().map(|e| e.id).collect();
    assert_eq!(undelivered, vec![responders], "只有真的還在協調者手上的才算它送不出去");
}

/// issue #504 審核：`notify_exhausted` 是**一張** incident 帶筆數，所以查詢只需要一批樣本。
/// 巡檢停在登入失效時整個佇列會一起用盡額度（現場是上百則），把它們全讀回來只是把記憶體與
/// incident 的 JSON 撐大——但 `total` 必須是真的，不然 detail 上的數字會騙人。
#[tokio::test]
async fn the_exhausted_probe_samples_a_bounded_batch_but_counts_them_all() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let n = EXHAUSTED_SCAN_LIMIT + 7;
    for i in 0..n {
        let id = push_inbox(&p, &format!("k{i}"), "health_changed", None, None, None, &json!({})).await.unwrap().unwrap();
        sqlx::query("UPDATE supervisor_inbox SET notify_attempts=5, created_at=? WHERE id=?")
            .bind(format!("2026-09-24T00:00:{:02}.000Z", i))
            .bind(&id)
            .execute(&p)
            .await
            .unwrap();
    }
    let got = exhausted_inbox(&p, 5).await.unwrap();
    assert_eq!(got.total, n, "總數是真的");
    assert_eq!(got.events.len() as i64, EXHAUSTED_SCAN_LIMIT, "樣本有上限");
    assert_eq!(got.events[0].event_key, "k0", "樣本最舊的在前，detail 指得到最早卡住的那一則");
}

/// Incidents are one row per resource, deduplicated across restarts, with a fresh row for a
/// recurrence so "it happened again" is not lost in an old timestamp.
#[tokio::test]
async fn an_incident_is_one_row_per_resource_and_a_recurrence_is_a_new_one() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let (first, opened) = open_incident(&p, "host_disconnected", "mac2", "degraded", &json!({"host": "mac2"}))
        .await
        .unwrap();
    assert!(opened);
    let (again, opened) = open_incident(&p, "host_disconnected", "mac2", "degraded", &json!({})).await.unwrap();
    assert!(!opened, "a second tick refreshes the incident, it does not open another");
    assert_eq!(again.id, first.id);
    assert_eq!(again.occurrences, 2);
    assert_eq!(open_incidents(&p).await.unwrap().len(), 1);

    let resolved = resolve_incident(&p, "host_disconnected", "mac2").await.unwrap().unwrap();
    assert_eq!(resolved.status, "resolved");
    assert!(resolved.resolved_at.is_some());
    assert!(open_incidents(&p).await.unwrap().is_empty());
    assert!(resolve_incident(&p, "host_disconnected", "mac2").await.unwrap().is_none(), "resolving twice is a no-op");

    let (third, opened) = open_incident(&p, "host_disconnected", "mac2", "degraded", &json!({})).await.unwrap();
    assert!(opened, "the same fault coming back is a new occurrence");
    assert_ne!(third.id, first.id);
    assert_eq!(incidents(&p, 10).await.unwrap().len(), 2, "the resolved one is still on the record");
}

/// The upgrade path. Rows closed under the old "turn ended = done" rule stay closed — a
/// backfill must not re-dispatch a month of work — but they are marked `legacy_closed`, so
/// nothing presents them as having been accepted by anyone.
#[tokio::test]
async fn the_migration_marks_old_rows_legacy_instead_of_reopening_them() {
    let p = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
    for stmt in DDL.split(";\n") {
        let s = stmt.trim();
        if !s.is_empty() {
            sqlx::query(s).execute(&p).await.unwrap();
        }
    }
    for (id, status) in [("a1", "completed"), ("a2", "failed"), ("a3", "delivered"), ("a4", "cancelled")] {
        sqlx::query(
            "INSERT INTO supervisor_assignments
               (id, supervisor_id, target_bot_id, client_request_id, text, status, attempts, created_at, updated_at)
             VALUES (?,?, 'bot1', ?, 'x', ?, 0, 't', 't')",
        )
        .bind(id)
        .bind(SUPERVISOR_ID)
        .bind(format!("crid-{id}"))
        .bind(status)
        .execute(&p)
        .await
        .unwrap();
    }
    assert!(!has_column(&p, "supervisor_assignments", "legacy_closed").await.unwrap());
    migrate(&p).await.unwrap();
    migrate(&p).await.unwrap(); // idempotent
    let rows: Vec<(String, String, i64, Option<String>)> =
        sqlx::query_as("SELECT id, status, legacy_closed, turn_status FROM supervisor_assignments ORDER BY id")
            .fetch_all(&p)
            .await
            .unwrap();
    assert_eq!(rows[0], ("a1".into(), "completed".into(), 1, Some("completed".into())));
    assert_eq!(rows[1], ("a2".into(), "failed".into(), 1, None));
    assert_eq!(rows[2], ("a3".into(), "delivered".into(), 0, None), "work in flight is untouched");
    assert_eq!(rows[3], ("a4".into(), "cancelled".into(), 1, None));
    assert_eq!(open_assignment_count(&p).await.unwrap(), 1, "the backfill does not re-open closed work");
    // And the sweep leaves them alone: nobody wants a month of old results pushed at AGM.
    assert!(settled_without_event(&p, 10).await.unwrap().is_empty());
}

/// Ownership is recorded on the assignment so an overlap can be reported. It is data for
/// AGM to arbitrate with, not a lock.
#[tokio::test]
async fn an_assignment_remembers_the_files_it_was_handed() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let owns = vec!["daemon/src/supervisor".to_string(), "scripts/agm.py".to_string()];
    let a = insert_assignment(&p, None, "bot1", "req-o", "x", &owns, None, true).await.unwrap();
    assert_eq!(a.ownership(), owns);
    assert_eq!(a.to_json()["ownership"][1], "scripts/agm.py");
    let b = insert_assignment(&p, None, "bot2", "req-p", "x", &[], None, true).await.unwrap();
    assert!(b.ownership().is_empty());
}

/// Two executors racing for the same rebuild window: exactly one wins. This is the whole
/// reason the lease exists — the old flow let both read "nothing is working" and proceed.
#[tokio::test]
async fn two_executors_race_for_one_window_and_only_one_gets_it() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let soon = "2099-01-01T00:00:00Z";
    let first = acquire_lease(&p, "rebuild", "bot-a", Some("ap1"), Some("abc"), soon, false, None, &json!({})).await.unwrap();
    let second = acquire_lease(&p, "rebuild", "bot-b", Some("ap2"), Some("abc"), soon, false, None, &json!({})).await.unwrap();
    let first = first.expect("first acquire wins");
    assert!(second.is_none(), "the second executor is refused while the window is held");
    assert_eq!(first.owner.as_deref(), Some("bot-a"));
    assert!(first.held_at(&crate::db::now()));

    // The holder can renew; nobody else can, and neither can a stale fence.
    assert!(renew_lease(&p, "rebuild", "bot-a", first.fence, soon).await.unwrap());
    assert!(!renew_lease(&p, "rebuild", "bot-b", first.fence, soon).await.unwrap(), "not your lease");
    assert!(!renew_lease(&p, "rebuild", "bot-a", first.fence - 1, soon).await.unwrap(), "stale fence");

    // Released: the next executor gets it, with a higher fence.
    assert!(release_lease(&p, "rebuild", "bot-a", first.fence).await.unwrap());
    assert!(!release_lease(&p, "rebuild", "bot-a", first.fence).await.unwrap(), "releasing twice is a no-op");
    let second = acquire_lease(&p, "rebuild", "bot-b", Some("ap2"), Some("abc"), soon, false, None, &json!({}))
        .await
        .unwrap()
        .expect("free again");
    assert!(second.fence > first.fence, "the fence only goes up");
}

/// A holder that died does not lock the window forever, and cannot carry on afterwards: its
/// fence is stale the moment somebody else takes over.
#[tokio::test]
async fn an_expired_lease_is_taken_over_and_the_old_token_stops_working() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let dead = acquire_lease(&p, "restart", "bot-a", None, None, "2000-01-01T00:00:00Z", false, None, &json!({}))
        .await
        .unwrap()
        .unwrap();
    assert!(!dead.held_at(&crate::db::now()), "already expired");
    let next = acquire_lease(&p, "restart", "bot-b", None, None, "2099-01-01T00:00:00Z", false, None, &json!({}))
        .await
        .unwrap()
        .expect("an expired lease does not block the next window");
    assert_eq!(next.owner.as_deref(), Some("bot-b"));
    assert!(
        !renew_lease(&p, "restart", "bot-a", dead.fence, "2099-01-01T00:00:00Z").await.unwrap(),
        "the old holder's token is worthless even though its approval may still say approved"
    );
    assert!(!release_lease(&p, "restart", "bot-a", dead.fence).await.unwrap());
}

/// The approval is a record, and revoking it takes effect for the next acquire even though
/// the words "AGM said yes" were true yesterday.
/// 起因：2026-09-16 k8bw2f 對同一顆 commit 送了兩筆一樣的 restart 申請（它讀錯回應欄位，
/// 以為沒送出去），AGM 只能核一筆、駁一筆。同一個 request id 重送要回原本那一筆。
#[tokio::test]
async fn one_request_id_is_one_approval() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    async fn ask(p: &SqlitePool, rid: Option<&str>) -> ApprovalOutcome {
        create_approval(p, "k8bw2f", "restart", "daemon", Some("ca7b22d"), None, rid).await.unwrap()
    }
    let first = ask(&p, Some("restart-ca7b22d")).await;
    assert!(first.created);
    assert_eq!(first.approval.client_request_id.as_deref(), Some("restart-ca7b22d"));

    let again = ask(&p, Some("restart-ca7b22d")).await;
    assert!(!again.created, "重送不是新的一筆");
    assert_eq!(again.approval.id, first.approval.id);
    assert_eq!(approvals(&p, 10).await.unwrap().len(), 1, "DB 裡還是只有一列");

    // 不同 id 就是不同的申請。
    let other = ask(&p, Some("restart-ca7b22d-retry")).await;
    assert!(other.created);
    assert_ne!(other.approval.id, first.approval.id);
    assert_eq!(approvals(&p, 10).await.unwrap().len(), 2);

    // 沒帶 id 的照舊：每次都開一筆新的（既有腳本不受影響）。
    let a = ask(&p, None).await;
    let b = ask(&p, None).await;
    assert!(a.created && b.created);
    assert_ne!(a.approval.id, b.approval.id);
    assert_eq!(approvals(&p, 10).await.unwrap().len(), 4);
}

/// 同一個 id 換了內容不是重送：回 409（`ApprovalRequestMismatch`），原本那筆一個字都不動。
#[tokio::test]
async fn the_same_request_id_with_different_content_is_refused() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let first = create_approval(&p, "bot", "rebuild", "daemon", Some("abc123"), None, Some("nightly"))
        .await
        .unwrap()
        .approval;

    for (purpose, scope, commit, field) in [
        ("rebuild", "daemon", Some("def456"), "target_commit"),
        ("restart", "daemon", Some("abc123"), "purpose"),
        ("rebuild", "web", Some("abc123"), "scope"),
    ] {
        let err = create_approval(&p, "bot", purpose, scope, commit, None, Some("nightly")).await.unwrap_err();
        let m = err.downcast::<ApprovalRequestMismatch>().expect("回的是內容不符，不是別的錯");
        assert_eq!(m.field, field);
        assert_eq!(m.approval_id, first.id, "錯誤訊息要指回原本那筆");
    }
    let kept = approval(&p, &first.id).await.unwrap().unwrap();
    assert_eq!((kept.purpose.as_str(), kept.scope.as_str(), kept.target_commit.as_deref()), ("rebuild", "daemon", Some("abc123")));
    assert_eq!(approvals(&p, 10).await.unwrap().len(), 1, "被拒絕的那幾次什麼都沒寫進去");
}

/// 已經裁示過的同 id 再送，回的還是它本人——重送的人要看到的是「這件事已經有裁示了」。
#[tokio::test]
async fn a_decided_request_id_comes_back_as_itself() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    for status in ["approved", "denied", "revoked", "consumed"] {
        let rid = format!("win-{status}");
        let first = create_approval(&p, "bot", "rebuild", "daemon", Some("abc123"), None, Some(&rid))
            .await
            .unwrap()
            .approval;
        decide_approval(&p, &first.id, status, "AGM", None, None).await.unwrap();
        let again = create_approval(&p, "bot", "rebuild", "daemon", Some("abc123"), None, Some(&rid)).await.unwrap();
        assert!(!again.created, "{status}：不該因為已經決定就再開一筆");
        assert_eq!(again.approval.id, first.id);
        assert_eq!(again.approval.status, status, "{status}：回的是它現在的裁示");
    }
    assert_eq!(approvals(&p, 10).await.unwrap().len(), 4);
}

/// 舊資料庫（沒有 `client_request_id` 那一欄）升級後照常可讀，既有列留白。
#[tokio::test]
async fn an_old_database_gains_the_column_without_losing_rows() {
    let p = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
    sqlx::query(
        "CREATE TABLE supervisor_approvals (
           id TEXT PRIMARY KEY, supervisor_id TEXT NOT NULL, requester TEXT NOT NULL,
           purpose TEXT NOT NULL, scope TEXT NOT NULL, target_commit TEXT,
           status TEXT NOT NULL DEFAULT 'pending', decided_by TEXT, decided_at TEXT, reason TEXT,
           expires_at TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL)",
    )
    .execute(&p)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO supervisor_approvals (id, supervisor_id, requester, purpose, scope, status, created_at, updated_at)
         VALUES ('old-1', 'AGM', 'bot', 'rebuild', 'daemon', 'approved', '2026-09-01T00:00:00Z', '2026-09-01T00:00:00Z')",
    )
    .execute(&p)
    .await
    .unwrap();

    migrate(&p).await.unwrap();
    migrate(&p).await.unwrap(); // 可重入

    let old = approval(&p, "old-1").await.unwrap().expect("舊列還在");
    assert_eq!(old.client_request_id, None, "既有列留白");
    assert_eq!(old.status, "approved");
    // 新的還是能帶 id 進來，而且不會跟留白的舊列撞唯一鍵。
    let a = create_approval(&p, "bot", "rebuild", "daemon", None, None, Some("r-1")).await.unwrap();
    assert!(a.created);
    assert!(!create_approval(&p, "bot", "rebuild", "daemon", None, None, Some("r-1")).await.unwrap().created);
    assert_eq!(approvals(&p, 10).await.unwrap().len(), 2);
}

/// 「未結案」以前被抄成四份、三種答案：`quota_blocked` 從 ownership 衝突與未結案計數裡消失，
/// AGM 於是回報「沒有人握著這塊」，把同一個模組派給第二顆 bot（review 2026-09-16）。
#[tokio::test]
async fn every_open_state_shows_up_where_ownership_is_checked() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    for (i, st) in OPEN_STATES.iter().enumerate() {
        let a = insert_assignment(&p, None, "b", &format!("crid-{i}"), "做事", &["daemon/src".to_string()], None, true).await.unwrap();
        sqlx::query("UPDATE supervisor_assignments SET status=? WHERE id=?").bind(st).bind(&a.id).execute(&p).await.unwrap();
    }
    assert_eq!(open_assignment_count(&p).await.unwrap(), OPEN_STATES.len() as i64, "未結案計數要涵蓋每一個未結案狀態");
    let unsettled: Vec<String> = unsettled_assignments(&p).await.unwrap().into_iter().map(|a| a.status).collect();
    for st in OPEN_STATES {
        assert!(unsettled.iter().any(|s| s == st), "{st} 不在 ownership 衝突看的清單裡：{unsettled:?}");
    }
    // 「卡住沒人管」是另一張表，而且刻意不含這兩個（都在等一個確定的東西）。
    assert!(!STALLED_STATES.contains(&"quota_blocked"));
    assert!(!STALLED_STATES.contains(&"blocked"));
}

/// 前一個持有者過期而不是 release 時，那張「可以」以前還是 approved——同一筆核准可以開好幾個
/// 重啟窗口，而且決定歷程上看不出來被用過（review 2026-09-16）。
#[tokio::test]
async fn taking_over_an_expired_lease_consumes_the_approval_it_rested_on() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let first = create_approval(&p, "bot-a", "restart", "daemon", None, None, None).await.unwrap().approval;
    decide_approval(&p, &first.id, "approved", "AGM", None, None).await.unwrap();
    let past = (chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    acquire_lease(&p, "restart", "runner-1", Some(&first.id), None, &past, false, None, &json!({})).await.unwrap().unwrap();
    assert_eq!(approval(&p, &first.id).await.unwrap().unwrap().status, "approved");

    // 沒有人 release，租約自己過期，下一個人接手。
    let second = create_approval(&p, "bot-a", "restart", "daemon", None, None, None).await.unwrap().approval;
    decide_approval(&p, &second.id, "approved", "AGM", None, None).await.unwrap();
    let later = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    acquire_lease(&p, "restart", "runner-2", Some(&second.id), None, &later, false, None, &json!({})).await.unwrap().unwrap();

    assert_eq!(approval(&p, &first.id).await.unwrap().unwrap().status, "consumed", "過期沒 release 的那張要被消耗掉");
    assert_eq!(approval(&p, &second.id).await.unwrap().unwrap().status, "approved", "接手的這張還在用");
    let notes = approval_decisions(&p).await.unwrap();
    assert!(notes.get(&first.id).is_some_and(|v| !v.is_empty()), "消耗要留在決定歷程裡");
}

/// 同一毫秒的兩筆裁示（核准後馬上撤銷）照寫入順序排，不看 ULID 的亂數段（review 2026-09-16：偶發紅）。
#[tokio::test]
async fn decisions_written_in_the_same_millisecond_keep_their_order() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let at = "2026-09-16T12:00:00.000Z";
    // id 故意跟寫入順序相反：第一筆的 ULID 比第二筆大。
    for (id, from, to) in [("01ZZZZZZZZZZZZZZZZZZZZZZZZ", "pending", "approved"), ("01AAAAAAAAAAAAAAAAAAAAAAAA", "approved", "revoked")] {
        sqlx::query("INSERT INTO supervisor_notes (id, supervisor_id, kind, body, version, created_at) VALUES (?,?,?,?,1,?)")
            .bind(id)
            .bind(SUPERVISOR_ID)
            .bind("approval_decision")
            .bind(json!({"approval_id": "ap", "from": from, "to": to}).to_string())
            .bind(at)
            .execute(&p)
            .await
            .unwrap();
    }
    let history = approval_decisions(&p).await.unwrap();
    let order: Vec<&str> = history["ap"].iter().map(|d| d["to"].as_str().unwrap()).collect();
    assert_eq!(order, vec!["approved", "revoked"]);
}

/// #972：`approval_decisions_for` 只取要的那幾筆歷程，跟全表版本的結果一致；不是 JSON 的列略過，不讓整句查詢報錯。
#[tokio::test]
async fn approval_decisions_for_returns_only_the_requested_approvals() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let rows = [
        ("01AAAAAAAAAAAAAAAAAAAAAAA1", json!({"approval_id": "a", "to": "approved"}).to_string(), "2026-09-16T12:00:00.000Z"),
        ("01AAAAAAAAAAAAAAAAAAAAAAA2", json!({"approval_id": "a", "to": "revoked"}).to_string(), "2026-09-16T12:00:01.000Z"),
        ("01AAAAAAAAAAAAAAAAAAAAAAA3", json!({"approval_id": "b", "to": "approved"}).to_string(), "2026-09-16T12:00:02.000Z"),
        ("01AAAAAAAAAAAAAAAAAAAAAAA4", "not json {".to_string(), "2026-09-16T12:00:03.000Z"),
    ];
    for (id, body, at) in rows {
        sqlx::query("INSERT INTO supervisor_notes (id, supervisor_id, kind, body, version, created_at) VALUES (?,?,?,?,1,?)")
            .bind(id)
            .bind(SUPERVISOR_ID)
            .bind("approval_decision")
            .bind(body)
            .bind(at)
            .execute(&p)
            .await
            .unwrap();
    }
    let only_a = approval_decisions_for(&p, &["a".to_string()]).await.unwrap();
    assert_eq!(only_a.len(), 1, "只有要的那一筆");
    let order: Vec<&str> = only_a["a"].iter().map(|d| d["to"].as_str().unwrap()).collect();
    assert_eq!(order, vec!["approved", "revoked"], "最舊在前");
    assert!(only_a["a"].iter().all(|d| d["at"].is_string()), "每筆帶 at");
    assert_eq!(only_a["a"], approval_decisions(&p).await.unwrap()["a"], "跟全表版本的結果一致");
    assert!(approval_decisions_for(&p, &[]).await.unwrap().is_empty());
    assert!(approval_decisions_for(&p, &["zzz".to_string()]).await.unwrap().is_empty());
}

/// AGM 背靠背派出的兩件交辦（同一毫秒）照寫入順序排，不看 ULID 的亂數段。
///
/// 同一毫秒建的兩件不能看 ULID 的亂數段。
#[tokio::test]
async fn open_and_unsettled_assignments_written_in_the_same_millisecond_keep_their_order() {
    let p = pool().await;
    let at = "2026-09-17T12:00:00.000Z";
    for id in ["01ZZZZZZZZZZZZZZZZZZZZZZZZ", "01AAAAAAAAAAAAAAAAAAAAAAAA"] {
        sqlx::query(
            "INSERT INTO supervisor_assignments (id, supervisor_id, target_bot_id, client_request_id, text, status, attempts, created_at, updated_at)
             VALUES (?,?, 'bot1', ?, 'x', 'queued', 0, ?, ?)",
        )
        .bind(id)
        .bind(SUPERVISOR_ID)
        .bind(format!("crid-{id}"))
        .bind(at)
        .bind(at)
        .execute(&p)
        .await
        .unwrap();
    }
    let want = vec!["01ZZZZZZZZZZZZZZZZZZZZZZZZ", "01AAAAAAAAAAAAAAAAAAAAAAAA"];
    let ids = |v: Vec<Assignment>| v.into_iter().map(|a| a.id).collect::<Vec<_>>();
    assert_eq!(ids(open_assignments(&p).await.unwrap()), want, "open_assignments 照寫入順序");
    assert_eq!(ids(unsettled_assignments(&p).await.unwrap()), want, "unsettled_assignments 照寫入順序");
}

#[tokio::test]
async fn an_approval_is_decided_once_and_revocable() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let a = create_approval(&p, "bot-a", "rebuild", "daemon/", Some("abc123"), Some("2099-01-01T00:00:00Z"), None)
        .await
        .unwrap()
        .approval;
    assert_eq!(a.status, "pending");
    assert_eq!(a.refusal("2026-09-12T12:00:00Z", "rebuild", Some("abc123")), Some("approval_not_decided"));
    let a = decide_approval(&p, &a.id, "approved", "AGM", Some("沒有人在跑"), None).await.unwrap().unwrap();
    assert_eq!(a.refusal("2026-09-12T12:00:00Z", "rebuild", Some("abc123")), None);
    assert_eq!(a.decided_by.as_deref(), Some("AGM"));
    let a = decide_approval(&p, &a.id, "revoked", "AGM", Some("使用者開始新回合"), None).await.unwrap().unwrap();
    assert_eq!(a.refusal("2026-09-12T12:00:00Z", "rebuild", Some("abc123")), Some("approval_revoked"));
    assert_eq!(approvals(&p, 10).await.unwrap().len(), 1);
}

/// The downgrade the review found: an old binary running `setup` writing its compiled-in
/// persona back over one somebody had just updated. Seeding only fills an empty slot.
#[tokio::test]
async fn a_stored_persona_is_never_overwritten_by_the_embedded_default() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    let s = get_or_init(&p).await.unwrap();
    assert_eq!((s.persona_version, s.persona_text.clone()), (0, None), "a fresh install has none");

    // First install: the embedded text seeds it.
    assert!(seed_persona_if_empty(&p, "內嵌版 v1").await.unwrap());
    let s = get_or_init(&p).await.unwrap();
    assert_eq!(s.persona_text.as_deref(), Some("內嵌版 v1"));
    assert_eq!(s.persona_version, 1);
    assert_eq!(s.persona_source.as_deref(), Some("embedded"));
    assert_eq!(s.persona_seed_hash, s.persona_hash, "it remembers which embedded text it came from");

    // Somebody updates it through the API.
    assert_eq!(set_persona(&p, "使用者改過的 v2", "api", None).await.unwrap(), 2);

    // An older binary runs `setup` again: it must not win.
    assert!(!seed_persona_if_empty(&p, "內嵌版 v1").await.unwrap(), "seeding is refused once there is a persona");
    let s = get_or_init(&p).await.unwrap();
    assert_eq!(s.persona_text.as_deref(), Some("使用者改過的 v2"));
    assert_eq!(s.persona_version, 2, "and the version does not move");
    assert_eq!(s.persona_source.as_deref(), Some("api"));
    assert_eq!(s.persona_hash.as_deref(), Some(super::super::persona::hash("使用者改過的 v2").as_str()));
    // The seed hash still points at the embedded text it started from, which is what makes
    // "there is a newer embedded version" a different question from "this was customised".
    assert_eq!(s.persona_seed_hash.as_deref(), Some(super::super::persona::hash("內嵌版 v1").as_str()));

    // An explicit migration is the one way the embedded text takes over.
    let v = set_persona(&p, "內嵌版 v3", "embedded", Some(&super::super::persona::hash("內嵌版 v3"))).await.unwrap();
    let s = get_or_init(&p).await.unwrap();
    assert_eq!((v, s.persona_text.as_deref()), (3, Some("內嵌版 v3")));
}

#[tokio::test]
async fn every_summary_is_kept_as_a_versioned_note() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    assert_eq!(set_summary(&p, "第一版").await.unwrap(), 1);
    assert_eq!(set_summary(&p, "第二版").await.unwrap(), 2);
    assert_eq!(get_or_init(&p).await.unwrap().summary.as_deref(), Some("第二版"));
    let notes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_notes WHERE kind='handoff'")
        .fetch_one(&p)
        .await
        .unwrap();
    assert_eq!(notes, 2, "the earlier summary is still recoverable");
}

#[tokio::test]
async fn a_failed_handoff_note_write_does_not_commit_the_summary_version() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    set_summary(&p, "第一版").await.unwrap();
    sqlx::query("CREATE TRIGGER fail_handoff_note BEFORE INSERT ON supervisor_notes WHEN NEW.kind='handoff' BEGIN SELECT RAISE(ABORT, 'injected note failure'); END")
        .execute(&p)
        .await
        .unwrap();

    assert!(set_summary(&p, "第二版").await.is_err());
    sqlx::query("DROP TRIGGER fail_handoff_note").execute(&p).await.unwrap();
    let sup = get_or_init(&p).await.unwrap();
    assert_eq!(sup.summary.as_deref(), Some("第一版"), "a summary without its audit note must roll back");
    assert_eq!(sup.summary_version, 1, "a failed note write must not consume a version");
    let notes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_notes WHERE kind='handoff'")
        .fetch_one(&p)
        .await
        .unwrap();
    assert_eq!(notes, 1);
}

/// 事件表只增不減：小的事件（`compact_handled_payloads` 不碰 <16 KB）永遠留著。流水帳類（健康變化、ops 警報、bot 申請）處理完放了
/// 60 天就整列刪掉；交辦結果等靠 `event_key` 去重的種類、沒處理完的、比較新的都不動。
#[tokio::test]
async fn old_handled_log_events_are_pruned_but_dedupe_keys_and_open_work_stay() {
    let p = pool().await;
    let old = crate::db::iso_in(-61 * 24 * 3600);
    let recent = crate::db::iso_in(-30 * 24 * 3600);
    let mut ids = std::collections::HashMap::new();
    for (name, kind, state, updated) in [
        ("old_health", "health_changed", "handled", old.clone()),
        ("old_ops", "ops_alert", "handled", old.clone()),
        ("old_request", "bot_request", "handled", old.clone()),
        ("recent_request", "bot_request", "handled", recent.clone()),
        ("old_assignment", "assignment_completed", "handled", old.clone()),
        ("old_pending_request", "bot_request", "pending", old.clone()),
        ("old_delivered_ops", "ops_alert", "delivered", old.clone()),
        ("old_gave_up_request", "bot_request", "gave_up", old.clone()),
    ] {
        let id = push_inbox(&p, &format!("k:{name}"), kind, None, None, None, &serde_json::json!({"n": name})).await.unwrap().unwrap();
        sqlx::query("UPDATE supervisor_inbox SET state=?, updated_at=? WHERE id=?").bind(state).bind(&updated).bind(&id).execute(&p).await.unwrap();
        ids.insert(name, id);
    }
    assert_eq!(prune_handled_events(&p, PRUNE_HANDLED_AFTER_SECS).await.unwrap(), 3, "old_health、old_ops、old_request");
    let left: Vec<String> = sqlx::query_scalar("SELECT event_key FROM supervisor_inbox ORDER BY rowid").fetch_all(&p).await.unwrap();
    assert_eq!(
        left,
        ["k:recent_request", "k:old_assignment", "k:old_pending_request", "k:old_delivered_ops", "k:old_gave_up_request"],
        "交辦結果的去重鍵、沒處理完的、比較新的都留著"
    );
    // 冪等。
    assert_eq!(prune_handled_events(&p, PRUNE_HANDLED_AFTER_SECS).await.unwrap(), 0);
    // 被刪掉的那種申請，之後同一句再來就是新的一則（不會被當成重播吞掉）。
    assert!(push_inbox(&p, "k:old_request", "bot_request", None, None, None, &serde_json::json!({})).await.unwrap().is_some());
}

#[tokio::test]
async fn only_the_most_recent_handoff_notes_are_kept() {
    let p = pool().await;
    get_or_init(&p).await.unwrap();
    for n in 1..=(HANDOFF_NOTES_KEPT + 5) {
        assert_eq!(set_summary(&p, &format!("第{n}版")).await.unwrap(), n);
    }
    let bodies: Vec<String> = sqlx::query_scalar("SELECT body FROM supervisor_notes WHERE kind='handoff' ORDER BY version")
        .fetch_all(&p)
        .await
        .unwrap();
    assert_eq!(bodies.len() as i64, HANDOFF_NOTES_KEPT);
    assert_eq!(bodies.first().map(String::as_str), Some("第6版"), "最舊的 5 版修掉了");
    assert_eq!(bodies.last().map(String::as_str), Some(format!("第{}版", HANDOFF_NOTES_KEPT + 5).as_str()));
    assert_eq!(get_or_init(&p).await.unwrap().summary.as_deref(), Some(format!("第{}版", HANDOFF_NOTES_KEPT + 5).as_str()), "現值不受影響");
}

/// 處理完的 inbox 列永遠不清，而 `health_changed` 的 payload 把整份 `supervisor.assignments` 塞進去（約 280 KB 一筆）：
/// 正式庫 10 天 166 筆、43 MB，是整顆 DB 最大的一塊。**不能刪列**：`event_key` 是去重鍵，`sweep_missing_events`
/// 看到「已結算卻沒有事件」的交辦會再補一筆——刪了就會被重新生出來。所以只把「處理完、放了一天、又很大」的
/// payload 換成一個小的標記，列、鍵、狀態、時間都留著；還沒處理完的（pending／delivered）一個字都不動。
#[tokio::test]
async fn old_large_handled_inbox_payloads_are_compacted_but_nothing_else_is_touched() {
    let p = pool().await;
    let big = serde_json::json!({"supervisor": {"assignments": vec!["x".repeat(100); 400]}});
    let small = serde_json::json!({"note": "small"});
    let day_ago = crate::db::iso_in(-26 * 3600);
    let mut ids = std::collections::HashMap::new();
    for (name, payload, state, updated) in [
        ("old_big_handled", &big, "handled", day_ago.clone()),
        ("new_big_handled", &big, "handled", crate::db::now()),
        ("old_small_handled", &small, "handled", day_ago.clone()),
        ("old_big_pending", &big, "pending", day_ago.clone()),
        ("old_big_delivered", &big, "delivered", day_ago.clone()),
    ] {
        let id = push_inbox(&p, &format!("k:{name}"), "health_changed", None, None, None, payload).await.unwrap().unwrap();
        sqlx::query("UPDATE supervisor_inbox SET state=?, updated_at=? WHERE id=?").bind(state).bind(&updated).bind(&id).execute(&p).await.unwrap();
        ids.insert(name, id);
    }
    let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox").fetch_one(&p).await.unwrap();

    let n = compact_handled_payloads(&p, 24 * 3600, 16 * 1024).await.unwrap();
    assert_eq!(n, 1, "只有「處理完、放超過一天、payload 很大」的那一筆");
    let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox").fetch_one(&p).await.unwrap();
    assert_eq!(after, before, "不刪列：event_key 是去重鍵");

    let payload = |name: &str| {
        let id = ids[name].clone();
        let p = p.clone();
        async move { sqlx::query_scalar::<_, String>("SELECT payload_json FROM supervisor_inbox WHERE id=?").bind(id).fetch_one(&p).await.unwrap() }
    };
    let compacted: serde_json::Value = serde_json::from_str(&payload("old_big_handled").await).unwrap();
    assert_eq!(compacted["compacted"], true);
    assert!(compacted["original_bytes"].as_i64().unwrap() > 16 * 1024);
    let key: String = sqlx::query_scalar("SELECT event_key FROM supervisor_inbox WHERE id=?").bind(&ids["old_big_handled"]).fetch_one(&p).await.unwrap();
    assert_eq!(key, "k:old_big_handled", "去重鍵還在");
    for untouched in ["new_big_handled", "old_big_pending", "old_big_delivered"] {
        assert_eq!(serde_json::from_str::<serde_json::Value>(&payload(untouched).await).unwrap(), big, "{untouched} 不動");
    }
    assert_eq!(serde_json::from_str::<serde_json::Value>(&payload("old_small_handled").await).unwrap(), small, "小的不動");
    // 冪等：再跑一次什麼都不會再動（壓過的已經很小）。
    assert_eq!(compact_handled_payloads(&p, 24 * 3600, 16 * 1024).await.unwrap(), 0);
}
