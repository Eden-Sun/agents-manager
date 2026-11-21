//! #1862 S1：從 `crates/am-supervisor/src/supervisor/failover.rs` 搬來的 daemon 整合測試（要 `state::App`／`testing::env`／runner）；
//! 測試名 `supervisor::failover::tests::…` → `supervisor_tests::failover::tests::…`。測試本文與斷言不變，只有 `use super::*` 改成 `use crate::supervisor::failover::*`。

#[allow(unused_imports)]
mod tests {
    use crate::supervisor::failover::*;
    use serde_json::{json, Value};
    use sqlx::SqlitePool;
    use crate::supervisor::roles::{self, Role};
    use crate::supervisor::store;
    use crate::state::App;
    use std::sync::Arc;

    /// `should_reassign` 的組合表。它**就是**生產路徑用的那一份判斷（`reassign_stale_approvals`
    /// 逐則呼叫它），所以這裡的邊界斷言不再是死碼；真實路徑另有
    /// `the_five_minute_line_is_exact_on_the_path_that_actually_runs` 從 SQL 進來再走一次。
    #[test]
    fn only_an_unavailable_responder_and_a_five_minute_wait_move_an_approval() {
        assert!(should_reassign(300, "responder", Some("needs_login")), "滿 5 分鐘＋不可用 → 改派");
        assert!(should_reassign(9000, "responder", Some("waiting_quota")), "撞限也算不可用（票上第 1 點）");
        assert!(!should_reassign(299, "responder", Some("needs_login")), "還沒滿 5 分鐘不動");
        assert!(!should_reassign(9000, "responder", None), "可用／讀不到都不動");
        assert!(!should_reassign(9000, "patrol", Some("needs_login")), "已經在巡檢手上的不再動");
    }

    /// 我依賴 `health::responder_state`（#420 後續）的哪些格子。這不是重測它的實作，而是釘住
    /// **改派要靠的契約**：三個 reason 字串、沒建立就不動、以及「讀不到不算不可用」。
    /// 它哪天多回一種 Unavailable（#427 的 notify_stalled）不會讓這條紅——多的只會更早改派。
    #[tokio::test]
    async fn the_contract_reassignment_depends_on_is_the_one_role_state_publishes() {
        let app = app().await;
        assert_eq!(responder_unavailable(&app).await, None, "沒建立協調者");

        sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p',?)").bind(crate::db::now()).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('resp','p','AGM-responder','claude','tok',?)")
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .unwrap();
        roles::set_env(&app.db, Role::Responder, "resp", "p", "/tmp").await.unwrap();
        assert_eq!(responder_unavailable(&app).await, Some("no_run"), "登記了卻沒有 run");

        sqlx::query(
            "INSERT INTO runs (id,bot_id,state,agent_status,workspace_id,pane_id,agent_name,herdr_session,pane_typed,started_at)
             VALUES ('r1','resp','running','idle','ws','pane-r','AGM-responder','test',1,?)",
        )
        .bind(crate::db::now())
        .execute(&app.db)
        .await
        .unwrap();
        assert_eq!(responder_unavailable(&app).await, None, "有 run 就當可用");

        for (status, want) in [("needs_login", "needs_login"), ("waiting_quota", "waiting_quota")] {
            roles::set_status(&app.db, Role::Responder, status, None, None).await.unwrap();
            assert_eq!(responder_unavailable(&app).await, Some(want), "status={status}");
        }
        roles::set_status(&app.db, Role::Responder, "", None, None).await.unwrap();
        assert_eq!(responder_unavailable(&app).await, None);

        sqlx::query("UPDATE bots SET deleted_at=? WHERE id='resp'").bind(crate::db::now()).execute(&app.db).await.unwrap();
        assert_eq!(responder_unavailable(&app).await, Some("no_run"), "軟刪但 run 還在 running");
        sqlx::query("UPDATE runs SET state='stopped' WHERE id='r1'").execute(&app.db).await.unwrap();
        assert_eq!(responder_unavailable(&app).await, Some("no_run"), "登記過但沒有在跑的 run");
    }

    async fn app() -> Arc<App> {
        let dir = crate::testing::scratch_dir("agm-failover");
        let db = crate::app_ports_p1::open(&dir.join("test.sqlite")).await.unwrap();
        let cfg = crate::runners::app_ports_p2::load_config(dir.join("config.toml")).await.unwrap();
        let client = crate::herdr::HerdrClient::new(dir.join("absent.sock"));
        let app = App::new(db, client.clone(), client, cfg, dir.clone(), dir.join("daemon"), 7799, "test".into(), "test".into(), false);
        store::get_or_init(&app.db).await.unwrap();
        app
    }

    /// 寫一則協調者手上的 `approval_requested`，`created_at` 往回調 `age` 秒。
    async fn approval_event(app: &Arc<App>, key: &str, age: i64) -> String {
        let id = store::push_inbox(&app.db, key, "approval_requested", None, None, None, &json!({"id": key})).await.unwrap().unwrap();
        sqlx::query("UPDATE supervisor_inbox SET role='responder', wake=1, created_at=? WHERE id=?")
            .bind(crate::db::iso_in(-age))
            .bind(&id)
            .execute(&app.db)
            .await
            .unwrap();
        id
    }

    async fn payload(app: &Arc<App>, id: &str) -> Value {
        let s: String = sqlx::query_scalar("SELECT payload_json FROM supervisor_inbox WHERE id=?").bind(id).fetch_one(&app.db).await.unwrap();
        serde_json::from_str(&s).unwrap()
    }

    /// 票上的主線：協調者不可用、核准開著超過 5 分鐘 → 改派巡檢並叫醒它。
    #[tokio::test]
    async fn an_unavailable_responder_hands_its_stale_approvals_to_patrol() {
        let app = app().await;
        let old = approval_event(&app, "approval:a-old:requested", 600).await;
        let fresh = approval_event(&app, "approval:a-fresh:requested", 60).await;

        assert_eq!(reassign_stale_approvals(&app, Some("needs_login")).await, 1, "只動等超過 5 分鐘的那則");
        assert_eq!(owner_of(&app.db, &old).await.unwrap().as_deref(), Some("patrol"));
        assert_eq!(owner_of(&app.db, &fresh).await.unwrap().as_deref(), Some("responder"), "剛進來的留在協調者手上");

        let p = payload(&app, &old).await;
        assert_eq!(p["reassigned_from"], json!("responder"));
        assert_eq!(p["reassigned_reason"], json!("needs_login"));
        assert!(p["reassigned_at"].as_str().is_some_and(|s| !s.is_empty()));

        let due = roles::due_for(&app.db, Role::Patrol, true, &crate::db::now(), 5).await.unwrap();
        assert_eq!(due.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), vec![old.as_str()]);
        assert_eq!(due[0].wake, Some(1));
    }

    /// 5 分鐘那條線**走真實路徑**測（i407 review 2026-09-24）：299 秒不動、300 秒動。
    /// 以前只有 `should_reassign` 的純函式測試釘這個邊界，而生產路徑的門檻寫在 SQL 裡、沒人測到——
    /// 把 SQL 的 cutoff 正負號弄反照樣全綠。現在門檻只有一份（`should_reassign`），而這條從
    /// `reassign_stale_approvals` 進去，所以那份真的被跑到。
    #[tokio::test]
    async fn the_five_minute_line_is_exact_on_the_path_that_actually_runs() {
        let app = app().await;
        let just_under = approval_event(&app, "approval:a-299:requested", REASSIGN_AFTER_SECS - 1).await;
        assert_eq!(reassign_stale_approvals(&app, Some("needs_login")).await, 0, "299 秒還不動");
        assert_eq!(owner_of(&app.db, &just_under).await.unwrap().as_deref(), Some("responder"));

        let exactly = approval_event(&app, "approval:a-300:requested", REASSIGN_AFTER_SECS).await;
        assert_eq!(reassign_stale_approvals(&app, Some("needs_login")).await, 1, "滿 300 秒就動");
        assert_eq!(owner_of(&app.db, &exactly).await.unwrap().as_deref(), Some("patrol"));
        assert_eq!(owner_of(&app.db, &just_under).await.unwrap().as_deref(), Some("responder"));
    }

    /// 協調者恢復之後**不搶回**。這條靠 `roles::classify` 只補 `role IS NULL` 的列才成立，
    /// 所以直接跑 classify 一次釘住：以後有人把它改成「重新分類全部」時這裡要紅。
    #[tokio::test]
    async fn a_recovered_responder_does_not_take_the_approval_back() {
        let app = app().await;
        let id = approval_event(&app, "approval:a1:requested", 600).await;
        reassign_stale_approvals(&app, Some("needs_login")).await;
        assert_eq!(owner_of(&app.db, &id).await.unwrap().as_deref(), Some("patrol"));

        for _ in 0..3 {
            assert_eq!(reassign_stale_approvals(&app, None).await, 0);
            roles::classify(&app.db).await.unwrap();
        }
        assert_eq!(owner_of(&app.db, &id).await.unwrap().as_deref(), Some("patrol"), "恢復之後不能搶回去");
        let p = payload(&app, &id).await;
        assert_eq!(roles::route("approval_requested", &p, None).role, Role::Patrol);
    }

    /// 協調者可用、或狀態讀不到（兩者都是 `None`）→ 一則都不動。
    #[tokio::test]
    async fn nothing_moves_while_the_responder_is_fine_or_unknown() {
        let app = app().await;
        let id = approval_event(&app, "approval:a1:requested", 9000).await;
        assert_eq!(reassign_stale_approvals(&app, None).await, 0);
        assert_eq!(owner_of(&app.db, &id).await.unwrap().as_deref(), Some("responder"));
    }

    /// 只有核准會改派：bot 的申請與 mission 事件留在協調者佇列（SPEC §18.15）。
    #[tokio::test]
    async fn only_approvals_are_reassigned() {
        let app = app().await;
        let mut ids = vec![];
        for kind in ["bot_request", "mission_question", "mission_created"] {
            let id = store::push_inbox(&app.db, &format!("k-{kind}"), kind, None, None, None, &json!({})).await.unwrap().unwrap();
            sqlx::query("UPDATE supervisor_inbox SET role='responder', wake=1, created_at=? WHERE id=?")
                .bind(crate::db::iso_in(-9000))
                .bind(&id)
                .execute(&app.db)
                .await
                .unwrap();
            ids.push(id);
        }
        assert_eq!(reassign_stale_approvals(&app, Some("needs_login")).await, 0);
        for id in &ids {
            assert_eq!(owner_of(&app.db, id).await.unwrap().as_deref(), Some("responder"), "{id} 不該被改派");
        }
    }

    /// 協調者累積的補送次數不能跟著搬過去：巡檢的 `due_for` 有 `notify_attempts < max` 的上限，
    /// 帶著 5 次過去等於改派的同一刻就已經超過上限，巡檢永遠撈不到它。
    #[tokio::test]
    async fn the_responders_failed_attempts_do_not_count_against_patrol() {
        let app = app().await;
        let id = approval_event(&app, "approval:a1:requested", 600).await;
        sqlx::query("UPDATE supervisor_inbox SET notify_attempts=5, notify_error='x', delivered_at=?, state='delivered', notify_turn_id='t1' WHERE id=?")
            .bind(crate::db::now())
            .bind(&id)
            .execute(&app.db)
            .await
            .unwrap();

        assert_eq!(reassign_stale_approvals(&app, Some("no_run")).await, 1);
        let (attempts, state, turn): (i64, String, Option<String>) =
            sqlx::query_as("SELECT notify_attempts, state, notify_turn_id FROM supervisor_inbox WHERE id=?").bind(&id).fetch_one(&app.db).await.unwrap();
        assert_eq!((attempts, state.as_str(), turn), (0, "pending", None));
        let due = roles::due_for(&app.db, Role::Patrol, true, &crate::db::now(), 5).await.unwrap();
        assert_eq!(due.len(), 1, "巡檢撈得到它");
    }

    /// 同一則跑兩拍只會改派一次（第二拍的 SQL 條件已經不成立）。
    #[tokio::test]
    async fn reassigning_twice_is_a_no_op() {
        let app = app().await;
        approval_event(&app, "approval:a1:requested", 600).await;
        assert_eq!(reassign_stale_approvals(&app, Some("needs_login")).await, 1);
        assert_eq!(reassign_stale_approvals(&app, Some("needs_login")).await, 0, "第二拍不再動它");
    }

    /// 30 分鐘沒裁示就報出來，而且用**接過來的等待起點**算：重申請取代舊的那筆時等待是接續的，
    /// 從新 id 的 `created_at` 重新算會讓「已經等兩小時」看起來像剛剛才申請。
    #[tokio::test]
    async fn an_approval_nobody_decided_for_thirty_minutes_is_reported() {
        let app = app().await;
        let fresh = store::create_approval(&app.db, "kick", "rebuild", "s", Some("c1"), None, None).await.unwrap().approval.id;
        assert!(stalled_approvals(&app.db, STALLED_AFTER_SECS).await.unwrap().is_empty(), "剛申請的不算");

        sqlx::query("UPDATE supervisor_approvals SET created_at=? WHERE id=?")
            .bind(crate::db::iso_in(-STALLED_AFTER_SECS - 60))
            .bind(&fresh)
            .execute(&app.db)
            .await
            .unwrap();
        let out = stalled_approvals(&app.db, STALLED_AFTER_SECS).await.unwrap();
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, fresh);
        assert!(out[0].1 >= STALLED_AFTER_SECS);
        assert_eq!(out[0].2, "kick");

        store::decide_approval(&app.db, &fresh, "approved", "AGM", None, None).await.unwrap();
        assert!(stalled_approvals(&app.db, STALLED_AFTER_SECS).await.unwrap().is_empty(), "裁示過的不算");
    }

    /// 接續的等待起點（`wait_since`）優先於新那筆的 `created_at`。
    #[tokio::test]
    async fn a_superseding_request_keeps_the_original_wait_for_the_stall_clock() {
        let app = app().await;
        let first = store::create_approval(&app.db, "kick", "rebuild", "s", Some("c1"), None, None).await.unwrap().approval.id;
        sqlx::query("UPDATE supervisor_approvals SET created_at=?, wait_since=? WHERE id=?")
            .bind(crate::db::iso_in(-7200))
            .bind(crate::db::iso_in(-7200))
            .bind(&first)
            .execute(&app.db)
            .await
            .unwrap();
        let second = store::create_approval(&app.db, "kick", "rebuild", "s", Some("c2"), None, None).await.unwrap().approval.id;
        let out = stalled_approvals(&app.db, STALLED_AFTER_SECS).await.unwrap();
        assert_eq!(out.iter().map(|(id, _, _)| id.as_str()).collect::<Vec<_>>(), vec![second.as_str()], "只剩新那筆 pending");
        assert!(out[0].1 >= 7200, "等待是接續的，不是從新 id 重新算：{}", out[0].1);
    }

    async fn pending(app: &Arc<App>, requester: &str, commit: &str) -> String {
        store::create_approval(&app.db, requester, "rebuild", "s", Some(commit), None, None).await.unwrap().approval.id
    }

    async fn age_and_expire(app: &Arc<App>, id: &str, expires_in: i64) {
        sqlx::query("UPDATE supervisor_approvals SET created_at=?, expires_at=? WHERE id=?")
            .bind(crate::db::iso_in(-STALLED_AFTER_SECS - 60))
            .bind(crate::db::iso_in(expires_in))
            .bind(id)
            .execute(&app.db)
            .await
            .unwrap();
    }

    /// #881：過期的 pending 沒人需要裁示，不算卡住；還沒過期的照常報（既有正向測試不能壞）。
    #[tokio::test]
    async fn an_expired_pending_approval_is_not_stalled() {
        let app = app().await;
        let id = pending(&app, "kick", "c1").await;
        age_and_expire(&app, &id, -10).await;
        assert!(stalled_approvals(&app.db, STALLED_AFTER_SECS).await.unwrap().is_empty(), "已過期的 pending 不算 stalled");
        age_and_expire(&app, &id, 3600).await;
        let out = stalled_approvals(&app.db, STALLED_AFTER_SECS).await.unwrap();
        assert_eq!(out.iter().map(|(i, _, _)| i.as_str()).collect::<Vec<_>>(), vec![id.as_str()], "還沒過期的照報");
        let past_secs = chrono::Utc::now().checked_sub_signed(chrono::Duration::seconds(10)).unwrap().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        sqlx::query("UPDATE supervisor_approvals SET expires_at=? WHERE id=?").bind(past_secs).bind(&id).execute(&app.db).await.unwrap();
        assert!(stalled_approvals(&app.db, STALLED_AFTER_SECS).await.unwrap().is_empty(), "秒格式的過期值也要認得");
    }

    /// #881：重申請（不帶 supersedes）時，同申請者同用途的 pending 已過期也要收掉，只剩一筆 pending。
    #[tokio::test]
    async fn re_requesting_supersedes_an_expired_pending_approval() {
        let app = app().await;
        let first = pending(&app, "kick", "c1").await;
        age_and_expire(&app, &first, -10).await;
        store::push_inbox(&app.db, &format!("approval:{first}:requested"), "approval_requested", None, None, None, &json!({"id": first})).await.unwrap();
        let second = store::create_approval(&app.db, "kick", "rebuild", "s", Some("c2"), None, None).await.unwrap();
        assert_eq!(second.superseded.as_deref(), Some(first.as_str()));
        let status = |id: String| {
            let db = app.db.clone();
            async move { sqlx::query_scalar::<_, String>("SELECT status FROM supervisor_approvals WHERE id=?").bind(id).fetch_one(&db).await.unwrap() }
        };
        assert_eq!(status(first.clone()).await, "superseded");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_approvals WHERE status='pending'").fetch_one(&app.db).await.unwrap();
        assert_eq!(n, 1, "只剩新那筆 pending");
        let wait: Option<String> = sqlx::query_scalar("SELECT wait_since FROM supervisor_approvals WHERE id=?").bind(&second.approval.id).fetch_one(&app.db).await.unwrap();
        assert!(wait.is_none(), "過期的舊申請沒有在等，不接續等待起點");
        let handled: String = sqlx::query_scalar("SELECT state FROM supervisor_inbox WHERE event_key=?").bind(format!("approval:{first}:requested")).fetch_one(&app.db).await.unwrap();
        assert_eq!(handled, "handled", "舊申請的 approval_requested 一起收掉");
    }

    /// #881：協調者不可用、一則 approval_requested 指向已過期的 pending → 不改派，直接收掉那則 inbox 事件。
    #[tokio::test]
    async fn an_event_for_an_expired_approval_is_closed_not_reassigned() {
        let app = app().await;
        let expired = pending(&app, "kick", "c1").await;
        sqlx::query("UPDATE supervisor_approvals SET expires_at=? WHERE id=?").bind(crate::db::iso_in(-10)).bind(&expired).execute(&app.db).await.unwrap();
        let live = pending(&app, "other", "c9").await;
        let mk = |key: &'static str, approval: String| {
            let app = app.clone();
            async move {
                let id = store::push_inbox(&app.db, key, "approval_requested", None, None, None, &json!({"approval_id": approval})).await.unwrap().unwrap();
                sqlx::query("UPDATE supervisor_inbox SET role='responder', wake=1, created_at=? WHERE id=?")
                    .bind(crate::db::iso_in(-600))
                    .bind(&id)
                    .execute(&app.db)
                    .await
                    .unwrap();
                id
            }
        };
        let dead = mk("approval:expired:requested", expired).await;
        let alive = mk("approval:live:requested", live).await;
        assert_eq!(reassign_stale_approvals(&app, Some("needs_login")).await, 1, "只改派還需要裁示的那則");
        let state: String = sqlx::query_scalar("SELECT state FROM supervisor_inbox WHERE id=?").bind(&dead).fetch_one(&app.db).await.unwrap();
        assert_eq!(state, "handled");
        assert_eq!(owner_of(&app.db, &dead).await.unwrap().as_deref(), Some("responder"), "過期的沒有被改派");
        assert_eq!(owner_of(&app.db, &alive).await.unwrap().as_deref(), Some("patrol"));
    }

    #[tokio::test]
    async fn an_expired_approvals_event_is_closed_even_when_the_responder_is_fine() {
        let app = app().await;
        let expired = pending(&app, "req1", "c1").await;
        sqlx::query("UPDATE supervisor_approvals SET expires_at=? WHERE id=?")
            .bind(crate::db::iso_in(-10))
            .bind(&expired)
            .execute(&app.db)
            .await
            .unwrap();
        let dead = store::push_inbox(&app.db, "approval:exp:requested", "approval_requested", None, None, None, &json!({"approval_id": expired}))
            .await
            .unwrap()
            .unwrap();
        sqlx::query("UPDATE supervisor_inbox SET role='responder', state='delivered', created_at=? WHERE id=?")
            .bind(crate::db::iso_in(-600))
            .bind(&dead)
            .execute(&app.db)
            .await
            .unwrap();

        let live = pending(&app, "req2", "c2").await;
        let alive = store::push_inbox(&app.db, "approval:live:requested", "approval_requested", None, None, None, &json!({"approval_id": live}))
            .await
            .unwrap()
            .unwrap();
        sqlx::query("UPDATE supervisor_inbox SET role='responder', state='delivered', created_at=? WHERE id=?")
            .bind(crate::db::iso_in(-600))
            .bind(&alive)
            .execute(&app.db)
            .await
            .unwrap();

        assert_eq!(reassign_stale_approvals(&app, None).await, 0);

        let (dead_state, dead_acked): (String, Option<String>) = sqlx::query_as("SELECT state, acked_by FROM supervisor_inbox WHERE id=?")
            .bind(&dead)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(dead_state, "handled");
        assert_eq!(dead_acked.as_deref(), Some("daemon"));

        let (alive_state, _): (String, Option<String>) = sqlx::query_as("SELECT state, role FROM supervisor_inbox WHERE id=?")
            .bind(&alive)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(alive_state, "delivered");
        assert_eq!(owner_of(&app.db, &alive).await.unwrap().as_deref(), Some("responder"));
    }

    #[tokio::test]
    async fn a_reassigned_event_is_closed_once_its_approval_is_decided() {
        let app = app().await;
        let apprv = pending(&app, "req3", "c3").await;
        let event_id = store::push_inbox(&app.db, "approval:reassigned:requested", "approval_requested", None, None, None, &json!({"approval_id": apprv}))
            .await
            .unwrap()
            .unwrap();
        sqlx::query("UPDATE supervisor_inbox SET role='patrol', state='delivered', created_at=? WHERE id=?")
            .bind(crate::db::iso_in(-600))
            .bind(&event_id)
            .execute(&app.db)
            .await
            .unwrap();

        sqlx::query("UPDATE supervisor_approvals SET status='superseded' WHERE id=?")
            .bind(&apprv)
            .execute(&app.db)
            .await
            .unwrap();

        assert_eq!(reassign_stale_approvals(&app, None).await, 0);

        let (state, acked_by): (String, Option<String>) = sqlx::query_as("SELECT state, acked_by FROM supervisor_inbox WHERE id=?")
            .bind(&event_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(state, "handled");
        assert_eq!(acked_by.as_deref(), Some("daemon"));
    }
}
