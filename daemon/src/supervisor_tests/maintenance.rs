//! #1862 S3b：從 `crates/am-supervisor/src/supervisor/maintenance.rs` 搬來的 daemon 整合測試；``supervisor::maintenance::tests::…` → `supervisor_tests::maintenance::tests::…``。
//! 測試本文與斷言不變；私有項由 am-supervisor 的 `*_for_test`（test-hooks）轉呼叫，下面用別名維持原名。

#![allow(unused_imports)]

use crate::supervisor::maintenance::*;
use crate::lifecycle::LcError;
use sqlx::SqlitePool;
use serde_json::{json, Value};
use crate::supervisor::store;
use crate::state::App;
use std::sync::Arc;
use crate::supervisor::maintenance::{delivery_critical_for_test as delivery_critical, held_leases_for_test as held_leases, iso_in_for_test as iso_in};

mod tests {
    use super::*;
    use crate::state::App;
    use std::sync::Arc;
    use crate::supervisor::store::Approval;

    /// 升級門檻的環境變數：只有「正整數」算數，其他一律回預設 30 分鐘。
    #[test]
    fn a_broken_threshold_env_falls_back_to_thirty_minutes() {
        assert!(!behind_escalates(2) && behind_escalates(3) && behind_escalates(10), "累積三個程式碼 commit 就放寬（使用者 2026-10-04）");
        assert_eq!(parse_escalate_mins(None), ESCALATE_AFTER_MINS);
        assert_eq!(ESCALATE_AFTER_MINS, 5, "使用者 2026-10-04：5 分鐘");
        assert_eq!(parse_escalate_mins(Some("45")), 45);
        assert_eq!(parse_escalate_mins(Some("  45 ")), 45);
        for bad in ["0", "-5", "abc", "", "30m"] {
            assert_eq!(parse_escalate_mins(Some(bad)), ESCALATE_AFTER_MINS, "{bad:?} 不該被當成門檻");
        }
    }

    /// 排著的 queued 只有在 bot 還有活著的 run 時才算送達臨界區；run 沒了的遺留那筆不擋。
    #[tokio::test]
    async fn a_queued_turn_without_a_live_run_is_not_a_delivery_in_progress() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let now = crate::db::now();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('q',?,'q','claude','tok-q',?)")
            .bind(&e.project_id).bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO conversations (id,bot_id,created_at) VALUES ('cq','q',?)").bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES ('rq','q','running','idle',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO turns (id,conversation_id,origin,status,delivery,prompt_text,created_at) VALUES ('tq','cq','web','queued','pending','x',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        assert_eq!(delivery_critical(&app.db, "q").await.unwrap().as_deref(), Some("tq"), "還有 run：真的在等送出");

        sqlx::query("UPDATE runs SET state='stopped', ended_at=? WHERE id='rq'").bind(&now).execute(&app.db).await.unwrap();
        assert_eq!(delivery_critical(&app.db, "q").await.unwrap(), None, "run 沒了：遺留的那筆不擋");
        let s = safety(app, &[]).await.unwrap();
        assert!(s["delivering"].as_array().unwrap().is_empty(), "{s}");

        sqlx::query("UPDATE turns SET status='failed', delivery='failed' WHERE id='tq'").execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES ('rq2','q','running','idle',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,created_at) VALUES ('tp','cq','rq2','web','in_flight','pending',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        assert_eq!(delivery_critical(&app.db, "q").await.unwrap().as_deref(), Some("tp"));
    }

    /// 等使用者回答的 bot（run `blocked`）排著的 queued 不擋窗口；一離開 blocked 立刻回到臨界區；
    /// 已經開始送的（in_flight＋pending）不管 blocked 與否照擋。用 acquire 真的走一遍：它在鎖內重判。
    #[tokio::test]
    async fn a_queued_prompt_behind_a_bot_waiting_for_the_user_holds_the_window_only_once_it_can_flush() {
        let e = crate::testing::env().await;
        let (app, pid) = (&e.app, e.project_id.clone());
        let now = crate::db::now();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('ask',?,'ask','claude','tok-ask',?)")
            .bind(&pid).bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO conversations (id,bot_id,created_at) VALUES ('ca','ask',?)").bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES ('ra','ask','running','working',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,created_at) VALUES ('tq_ask','ca','ra','web','in_flight','ok',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO turns (id,conversation_id,origin,status,delivery,prompt_text,created_at) VALUES ('tq_next','ca','web','queued','pending','x',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        let waited = approved_window(app, 40).await;
        let set = |status: &'static str| {
            let db = app.db.clone();
            async move { sqlx::query("UPDATE runs SET agent_status=? WHERE id='ra'").bind(status).execute(&db).await.unwrap(); }
        };

        set("idle").await;
        assert_eq!(delivery_critical(&app.db, "ask").await.unwrap().as_deref(), Some("tq_next"));
        let refused = acquire(app, "rebuild", "ops", &waited, None, 300, true, &[]).await.unwrap_err();
        let LcError::Conflict(detail) = &refused else { panic!("expected a conflict, got {refused:?}") };
        assert_eq!(detail["safety"]["delivering"][0]["turn_id"], "tq_next", "{detail}");

        set("blocked").await;
        assert_eq!(delivery_critical(&app.db, "ask").await.unwrap(), None);
        let s = safety_for(app, &[], Some(&waited)).await.unwrap();
        assert_eq!((s["safe"].as_bool(), s["delivering"].as_array().map(Vec::len)), (Some(true), Some(0)), "{s}");
        assert_eq!(s["blocked_waiting_for_user"][0]["bot_id"], "ask", "照實回報在等使用者");

        set("idle").await;
        assert_eq!(delivery_critical(&app.db, "ask").await.unwrap().as_deref(), Some("tq_next"));
        assert_eq!(safety_for(app, &[], Some(&waited)).await.unwrap()["safe"], false);

        sqlx::query("UPDATE turns SET status='completed' WHERE id='tq_ask'").execute(&app.db).await.unwrap();
        sqlx::query("UPDATE turns SET status='in_flight', run_id='ra' WHERE id='tq_next'").execute(&app.db).await.unwrap();
        set("blocked").await;
        assert_eq!(delivery_critical(&app.db, "ask").await.unwrap().as_deref(), Some("tq_next"));

        set("blocked").await;
        sqlx::query("UPDATE turns SET delivery='ok' WHERE id='tq_next'").execute(&app.db).await.unwrap();
        let taken = acquire(app, "rebuild", "ops", &waited, None, 300, true, &[]).await.unwrap();
        assert_eq!(taken["lease"]["owner"], "ops", "送達之後只剩思考中，縮小封鎖面就放行");
    }

    /// issue #1677：bot 還在 working，排在後面的 queued 不算送達臨界區——flush 在 working 時不會開始打字
    /// （排隊跨重啟保得住，回合結束的邊在新 daemon 照樣收得到），所以不能讓它把部署（含使用者按了「現在換版」）擋死。
    /// 對照：同一則訊息在 bot 閒下來之後（flush 隨時會領）照擋，已經開始送的（in_flight＋pending）也照擋。
    #[tokio::test]
    async fn a_queued_prompt_behind_a_working_turn_does_not_hold_the_window() {
        let e = crate::testing::env().await;
        let (app, pid) = (&e.app, e.project_id.clone());
        thinking_bot(app, &pid, "w").await;
        let now = crate::db::now();
        sqlx::query("INSERT INTO turns (id,conversation_id,origin,status,delivery,prompt_text,created_at) VALUES ('tw_next','w','web','queued','pending','x',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        let waited = approved_window(app, 40).await;

        assert_eq!(delivery_critical(&app.db, "w").await.unwrap(), None, "working：排隊的還沒有人會去送它");
        let s = safety_for(app, &[], Some(&waited)).await.unwrap();
        assert_eq!((s["safe"].as_bool(), s["delivering"].as_array().map(Vec::len)), (Some(true), Some(0)), "放寬之後只剩 working 不擋：{s}");
        assert_eq!(s["working"][0]["bot_id"], "w", "照實回報它還在 working");
        let until = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let lease = |quiet: bool| {
            let db = app.db.clone();
            let until = until.clone();
            async move { store::acquire_lease(&db, "restart", "ops", None, None, &until, quiet, None, &json!({})).await.unwrap() }
        };
        assert!(lease(true).await.is_some(), "acquire 那一句的條件也放過它");
        store::release_lease(&app.db, "restart", "ops", 1).await.unwrap();

        sqlx::query("UPDATE runs SET agent_status='idle' WHERE id='w'").execute(&app.db).await.unwrap();
        assert_eq!(delivery_critical(&app.db, "w").await.unwrap().as_deref(), Some("tw_next"));
        assert!(lease(true).await.is_none(), "閒著的 bot 排著一則：窗口拿不到（raced）");

        sqlx::query("UPDATE turns SET status='completed' WHERE id='w'").execute(&app.db).await.unwrap();
        sqlx::query("UPDATE turns SET status='in_flight', run_id='w' WHERE id='tw_next'").execute(&app.db).await.unwrap();
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id='w'").execute(&app.db).await.unwrap();
        assert_eq!(delivery_critical(&app.db, "w").await.unwrap().as_deref(), Some("tw_next"));
        assert!(lease(true).await.is_none(), "正在打字的那一刻仍然擋");
        let s = safety_for(app, &[], Some(&waited)).await.unwrap();
        assert_eq!(s["safe"], false, "{s}");
        assert_eq!(s["delivering"][0]["turn_id"], "tw_next");
    }

    /// 一顆有 active run、正在 working、而且該筆 turn 已經送達的 bot：思考中的樣子。
    async fn thinking_bot(app: &Arc<App>, project: &str, id: &str) {
        let now = crate::db::now();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES (?,?,?,'claude',?,?)")
            .bind(id).bind(project).bind(id).bind(format!("tok-{id}")).bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES (?,?,'running','working',?)")
            .bind(id).bind(id).bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO conversations (id,bot_id,created_at) VALUES (?,?,?)")
            .bind(id).bind(id).bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
            .bind(id).bind(id).bind(id).bind(&now).execute(&app.db).await.unwrap();
    }

    /// 已核准、還沒用掉的 rebuild 窗口，決定時間往前推 `mins` 分鐘。申請者是 `ops`（acquire 的 owner 要跟它一樣）。
    async fn approved_window(app: &Arc<App>, mins: i64) -> String {
        let a = store::create_approval(&app.db, "ops", "rebuild", "release", None, None, None).await.unwrap().approval;
        store::decide_approval(&app.db, &a.id, "approved", "AGM", None, None).await.unwrap();
        let at = (chrono::Utc::now() - chrono::Duration::minutes(mins)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        sqlx::query("UPDATE supervisor_approvals SET decided_at=? WHERE id=?").bind(&at).bind(&a.id).execute(&app.db).await.unwrap();
        a.id
    }

    /// 核准沒寫有效期（申請、裁示都沒給 `expires_in_secs`）不等於永遠有效：放了一天沒用，當時的安全判斷與 commit 早就過時。
    /// 預設一天；有寫有效期的照寫的算。過期的不能開窗口，也不能再替升級計時（放寬）開門。
    #[tokio::test]
    async fn an_approval_nobody_gave_an_expiry_still_lapses_after_a_day() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let fresh = approved_window(app, 23 * 60).await;
        let old = approved_window(app, 25 * 60).await;
        let explicit = approved_window(app, 25 * 60).await;
        sqlx::query("UPDATE supervisor_approvals SET expires_at=? WHERE id=?").bind(crate::db::iso_in(3600)).bind(&explicit).execute(&app.db).await.unwrap();

        let refused = acquire(app, "rebuild", "ops", &old, None, 300, true, &[]).await.unwrap_err();
        let LcError::Conflict(detail) = &refused else { panic!("expected a conflict, got {refused:?}") };
        assert_eq!(detail["reason"], "approval_expired", "{detail}");
        assert!(store::lease(&app.db, "rebuild").await.unwrap().is_none());

        let l = acquire(app, "rebuild", "ops", &explicit, None, 300, true, &[]).await.expect("明寫的有效期照算");
        let fence = l["lease"]["fence"].as_i64().unwrap();
        release(app, "rebuild", "ops", fence, store::LeaseProof::Token(l["lease_token"].as_str().unwrap())).await.unwrap();

        let l = acquire(app, "rebuild", "ops", &fresh, None, 3 * 3600, true, &[]).await.expect("23 小時前的核准還有效");
        let expires = l["lease"]["expires_at"].as_str().unwrap();
        let implied = store::approval(&app.db, &fresh).await.unwrap().unwrap().effective_expiry().unwrap();
        assert!(crate::db::cmp_ts(expires, &implied).is_le(), "租約 {expires} 不能比核准隱含的期限 {implied} 久");
    }

    /// 放了好幾天的舊核准不能讓「等太久就放寬」的計時一開始就算滿（`safety` 不帶核准時看最早那筆還活著的）。
    #[tokio::test]
    async fn a_forgotten_ancient_approval_does_not_start_the_escalation_clock() {
        let e = crate::testing::env().await;
        let (app, pid) = (&e.app, e.project_id.clone());
        thinking_bot(app, &pid, "user").await;
        let _ancient = approved_window(app, 5 * 24 * 60).await;
        let s = safety(app, &[]).await.unwrap();
        assert_eq!(s["escalated"], false, "{s}");
        assert_eq!(s["waited_secs"], serde_json::Value::Null, "過期的核准不計時");
        assert_eq!(s["safe"], false);
    }

    /// commit 是自由字串會被寫進核准、租約與稽核紀錄、也會被腳本拿去用：只收 7～64 碼十六進位。
    #[test]
    fn a_commit_must_look_like_a_commit() {
        for ok in ["abc1234", "0123456789abcdef0123456789abcdef01234567", "ABCDEF1"] {
            assert!(valid_commit(ok), "{ok}");
        }
        for bad in ["", "abc", "main", "HEAD~1", "abc1234; rm -rf /", "abc1234\n", "../x", " abc1234", "g123456", &"a".repeat(65)] {
            assert!(!valid_commit(bad), "{bad:?}");
        }
    }

    /// 窗口拿著的時候，同一個 owner 用另一張核准再 acquire 同一個 resource：不重入、不換 fence、不消耗第二張核准。
    #[tokio::test]
    async fn the_same_owner_cannot_re_enter_a_window_it_already_holds() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let a = approved_window(app, 0).await;
        let b = approved_window(app, 0).await;
        let first = acquire(app, "rebuild", "ops", &a, None, 300, true, &[]).await.unwrap();
        let fence = first["lease"]["fence"].as_i64().unwrap();
        let err = acquire(app, "rebuild", "ops", &b, None, 300, true, &[]).await.unwrap_err();
        let LcError::Conflict(d) = &err else { panic!("{err:?}") };
        assert_eq!(d["reason"], "lease_held", "{d}");
        assert_eq!(store::lease(&app.db, "rebuild").await.unwrap().unwrap().fence, fence);
        assert_eq!(store::approval(&app.db, &b).await.unwrap().unwrap().status, "approved", "第二張沒被消耗");
    }

    /// 持有者還在做事、TTL 已經到了：續約被拒（`lease_lost`），不是靜靜復活；舊 token 在別人接手後也不能再用。
    #[tokio::test]
    async fn an_expired_lease_cannot_be_renewed_and_an_old_token_dies_with_it() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let a = approved_window(app, 0).await;
        let first = acquire(app, "rebuild", "ops", &a, None, 300, true, &[]).await.unwrap();
        let (fence, token) = (first["lease"]["fence"].as_i64().unwrap(), first["lease_token"].as_str().unwrap().to_string());
        let past = (chrono::Utc::now() - chrono::Duration::seconds(5)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        sqlx::query("UPDATE supervisor_leases SET expires_at=? WHERE resource='rebuild'").bind(&past).execute(&app.db).await.unwrap();
        let deadline = iso_in(300);
        assert!(!store::renew_lease(&app.db, "rebuild", "ops", fence, &deadline).await.unwrap(), "過期的租約不能續");
        let b = store::create_approval(&app.db, "other", "rebuild", "x", None, None, None).await.unwrap().approval;
        store::decide_approval(&app.db, &b.id, "approved", "AGM", None, None).await.unwrap();
        let second = acquire(app, "rebuild", "other", &b.id, None, 300, true, &[]).await.unwrap();
        assert!(second["lease"]["fence"].as_i64().unwrap() > fence);
        let released = release(app, "rebuild", "ops", fence, store::LeaseProof::Token(&token)).await;
        assert!(matches!(released, Ok(false)) || released.is_err(), "{released:?}");
        assert!(store::lease(&app.db, "rebuild").await.unwrap().unwrap().held_at(&crate::db::now()), "新持有者的租約還在");
    }

    /// A replacement lease may be acquired after the old row is released but before its approval
    /// is consumed. The release path must consume the approval attached to the released fence,
    /// never whichever row happens to be current when it resumes.
    #[tokio::test]
    async fn releasing_an_old_lease_does_not_consume_the_next_holders_approval() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let first_approval = approved_window(app, 0).await;
        let next_approval = approved_window(app, 0).await;
        let first = store::acquire_lease(
            &app.db,
            "rebuild",
            "ops",
            Some(&first_approval),
            None,
            &iso_in(300),
            false,
            None,
            &json!({}),
        )
        .await
        .unwrap()
        .unwrap();
        let token = first.lease_token.clone().unwrap();
        let next_app = app.clone();
        let next_id = next_approval.clone();
        crate::lifecycle::race_point::arm("lease_release_after_mark", &token, move || async move {
            let replacement = store::acquire_lease(
                &next_app.db,
                "rebuild",
                "ops",
                Some(&next_id),
                None,
                &iso_in(300),
                false,
                None,
                &json!({}),
            )
            .await
            .unwrap();
            assert!(replacement.is_some(), "the released slot accepts a new, separately approved lease");
        });

        assert!(release(app, "rebuild", "ops", first.fence, store::LeaseProof::Token(&token)).await.unwrap());
        let current = store::lease(&app.db, "rebuild").await.unwrap().unwrap();
        assert_eq!(current.approval_id.as_deref(), Some(next_approval.as_str()));
        assert!(current.released_at.is_none(), "the new holder remains active");
        assert_eq!(store::approval(&app.db, &next_approval).await.unwrap().unwrap().status, "approved",
            "releasing fence {} must not consume the replacement's approval", first.fence);
    }

    /// issue #949：`lease_release_after_mark` 的 key 是租約 token，不是共用的 `"rebuild"`：別條測試（或別張租約）的 release
    /// 不會吃掉掛在另一張租約上的動作，動作也不會在別人的 release 裡 panic。
    #[tokio::test]
    async fn an_armed_release_action_fires_only_for_its_own_lease_token() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let e = crate::testing::env().await;
        let app = &e.app;
        let other_fired = Arc::new(AtomicBool::new(false));
        let flag = other_fired.clone();
        crate::lifecycle::race_point::arm("lease_release_after_mark", "someone-elses-token", move || async move {
            flag.store(true, Ordering::SeqCst);
        });

        let a = approved_window(app, 0).await;
        let first = acquire(app, "rebuild", "ops", &a, None, 300, true, &[]).await.unwrap();
        let (fence, token) = (first["lease"]["fence"].as_i64().unwrap(), first["lease_token"].as_str().unwrap().to_string());
        assert!(release(app, "rebuild", "ops", fence, store::LeaseProof::Token(&token)).await.unwrap());
        assert!(!other_fired.load(Ordering::SeqCst), "另一張租約的 token 掛的動作，不會被這張的 release 觸發");

        let b = approved_window(app, 0).await;
        let second = acquire(app, "rebuild", "ops", &b, None, 300, true, &[]).await.unwrap();
        let (fence2, token2) = (second["lease"]["fence"].as_i64().unwrap(), second["lease_token"].as_str().unwrap().to_string());
        let own_fired = Arc::new(AtomicBool::new(false));
        let flag = own_fired.clone();
        crate::lifecycle::race_point::arm("lease_release_after_mark", &token2, move || async move {
            flag.store(true, Ordering::SeqCst);
        });
        assert!(release(app, "rebuild", "ops", fence2, store::LeaseProof::Token(&token2)).await.unwrap());
        assert!(own_fired.load(Ordering::SeqCst), "掛在自己 token 上的動作要觸發");
        assert!(!other_fired.load(Ordering::SeqCst));
    }

    /// 核准後一直等不到全靜止時才放寬，而且只放寬「思考中」這一項。
    #[tokio::test]
    async fn only_a_window_that_waited_past_the_threshold_stops_blocking_on_thinking() {
        let e = crate::testing::env().await;
        let (app, pid) = (&e.app, e.project_id.clone());
        thinking_bot(app, &pid, "user").await;

        let s = safety(app, &[]).await.unwrap();
        assert_eq!((s["safe"].as_bool(), s["escalated"].as_bool()), (Some(false), Some(false)));
        assert_eq!(s["waited_secs"], serde_json::Value::Null);
        assert_eq!(s["working"].as_array().unwrap().len(), 1);

        let id = approved_window(app, 2).await;
        let s = safety(app, &[]).await.unwrap();
        assert_eq!(s["escalated"], false, "還沒到門檻就放寬的話這條規則等於沒有界線");
        assert_eq!(s["safe"], false);
        assert!(s["waited_secs"].as_i64().unwrap() >= 120);
        assert_eq!(s["escalation_approval_id"], id);

        sqlx::query("UPDATE supervisor_approvals SET decided_at=? WHERE id=?")
            .bind((chrono::Utc::now() - chrono::Duration::minutes(40)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
            .bind(&id).execute(&app.db).await.unwrap();
        let s = safety(app, &[]).await.unwrap();
        assert_eq!((s["escalated"].as_bool(), s["safe"].as_bool()), (Some(true), Some(true)));
        assert!(s["waited_secs"].as_i64().unwrap() >= 2400);
        assert_eq!(s["working"].as_array().unwrap().len(), 1, "還是照實回報誰在跑，只是不再擋");
        assert_eq!(s["escalate_after_secs"], ESCALATE_AFTER_MINS * 60);
    }

    /// 放寬只會放寬：全靜止時是 safe 的窗口，等超過門檻之後不能變成 unsafe。
    /// 以前的反例：大家都閒著，但有一筆放回佇列、正在等退避的 queued（bot 還有 run），或別人握著租約——
    /// 全靜止判 safe，等滿 30 分鐘改走縮小封鎖面的規則反而判 unsafe。
    #[tokio::test]
    async fn waiting_past_the_threshold_never_makes_a_quiet_box_unsafe() {
        let e = crate::testing::env().await;
        let (app, pid) = (&e.app, e.project_id.clone());
        let now = crate::db::now();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('idle',?,'idle','claude','tok-idle',?)")
            .bind(&pid).bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES ('ri','idle','running','idle',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO conversations (id,bot_id,created_at) VALUES ('ci','idle',?)").bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO turns (id,conversation_id,origin,status,delivery,prompt_text,next_flush_at,created_at) VALUES ('tb','ci','web','queued','pending','x','2099-01-01T00:00:00Z',?)")
            .bind(&now).execute(&app.db).await.unwrap();

        let fresh = approved_window(app, 0).await;
        let quiet = safety_for(app, &[], Some(&fresh)).await.unwrap();
        assert_eq!((quiet["escalated"].as_bool(), quiet["safe"].as_bool()), (Some(false), Some(true)), "全靜止：safe");

        let waited = approved_window(app, 40).await;
        let relaxed = safety_for(app, &[], Some(&waited)).await.unwrap();
        assert_eq!(relaxed["escalated"], true);
        assert_eq!(relaxed["delivering"].as_array().unwrap().len(), 1, "照實列出那一則");
        assert_eq!(relaxed["safe"], true, "等得越久不能越難開：{relaxed}");

        thinking_bot(app, &pid, "busy").await;
        let blocked = safety_for(app, &[], Some(&waited)).await.unwrap();
        assert_eq!(blocked["safe"], false, "{blocked}");
    }

    /// 誰等太久就放寬誰：A 放了很久沒用掉，不能替剛核下來的 B 開門。
    /// 這是這條規則的界線——否則一張被遺忘的核准等於把所有人的窗口都打開。
    #[tokio::test]
    async fn a_window_is_only_relaxed_by_its_own_wait() {
        let e = crate::testing::env().await;
        let (app, pid) = (&e.app, e.project_id.clone());
        thinking_bot(app, &pid, "user").await;
        let old = approved_window(app, 40).await;
        let fresh = approved_window(app, 0).await;

        let by_old = safety_for(app, &[], Some(&old)).await.unwrap();
        assert_eq!((by_old["escalated"].as_bool(), by_old["safe"].as_bool()), (Some(true), Some(true)));
        assert_eq!(by_old["escalation_approval_id"], old);
        let by_fresh = safety_for(app, &[], Some(&fresh)).await.unwrap();
        assert_eq!((by_fresh["escalated"].as_bool(), by_fresh["safe"].as_bool()), (Some(false), Some(false)));
        assert_eq!(by_fresh["escalation_approval_id"], fresh);
        assert!(by_fresh["waited_secs"].as_i64().unwrap() < 60);

        let plain = safety(app, &[]).await.unwrap();
        assert_eq!((plain["escalated"].as_bool(), plain["escalation_approval_id"].as_str()), (Some(true), Some(old.as_str())));

        for bad in [crate::db::ulid().as_str(), ""] {
            let s = safety_for(app, &[], Some(bad)).await.unwrap();
            assert_eq!((s["escalated"].as_bool(), s["safe"].as_bool()), (Some(false), Some(false)), "{bad:?}");
        }
        store::decide_approval(&app.db, &old, "consumed", "AGM", None, None).await.unwrap();
        let used = safety_for(app, &[], Some(&old)).await.unwrap();
        assert_eq!(used["escalated"], false, "用掉的核准不能再拿來計時");
    }

    /// 真的走 acquire：拿著剛核下來的 B，即使 A 已經等了很久，窗口一樣拿不到。
    #[tokio::test]
    async fn acquire_with_a_fresh_approval_does_not_borrow_another_ones_wait() {
        let e = crate::testing::env().await;
        let (app, pid) = (&e.app, e.project_id.clone());
        thinking_bot(app, &pid, "user").await;
        let old = approved_window(app, 40).await;
        let fresh = approved_window(app, 0).await;

        let refused = acquire(app, "rebuild", "ops", &fresh, None, 300, true, &[]).await.unwrap_err();
        let LcError::Conflict(detail) = &refused else { panic!("expected a conflict, got {refused:?}") };
        assert_eq!(detail["reason"], "not_idle");
        assert_eq!(detail["safety"]["escalated"], false, "B 自己才剛核下來");
        assert!(store::leases(&app.db).await.unwrap().is_empty(), "拿不到就什麼都不該留下");

        let taken = acquire(app, "rebuild", "ops", &old, None, 300, true, &[]).await.unwrap();
        assert_eq!(taken["safety"]["escalated"], true);
        assert_eq!(taken["safety"]["escalation_approval_id"], old);
        assert_eq!(taken["lease"]["owner"], "ops");
    }

    /// 放寬之後仍然擋的三件事：正在送出、排隊中待送、別人還握著租約。
    #[tokio::test]
    async fn the_relaxed_window_still_waits_for_delivery_and_leases() {
        let e = crate::testing::env().await;
        let (app, pid) = (&e.app, e.project_id.clone());
        thinking_bot(app, &pid, "user").await;
        approved_window(app, 40).await;
        assert_eq!(safety(app, &[]).await.unwrap()["safe"], true, "先確認這個盤面本來是安全的");

        sqlx::query("UPDATE turns SET delivery='pending' WHERE id='user'").execute(&app.db).await.unwrap();
        let s = safety(app, &[]).await.unwrap();
        assert_eq!(s["safe"], false, "送達臨界區不能被打斷");
        assert_eq!(s["delivering"][0]["bot_id"], "user");

        sqlx::query("UPDATE turns SET delivery='ok' WHERE id='user'").execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO turns (id,conversation_id,origin,status,delivery,created_at) VALUES ('q','user','web','queued','pending',?)")
            .bind(crate::db::now()).execute(&app.db).await.unwrap();
        let s = safety(app, &[]).await.unwrap();
        assert_eq!((s["safe"].as_bool(), s["delivering"].as_array().map(Vec::len)), (Some(true), Some(0)), "working 的 bot 身上排隊的不擋：{s}");
        sqlx::query("UPDATE runs SET agent_status='idle' WHERE id='user'").execute(&app.db).await.unwrap();
        let s = safety(app, &[]).await.unwrap();
        assert_eq!(s["safe"], false, "閒著的 bot 排隊中待送的 prompt 也算臨界區");
        assert_eq!(s["delivering"][0]["turn_id"], "q");
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id='user'").execute(&app.db).await.unwrap();

        sqlx::query("DELETE FROM turns WHERE id='q'").execute(&app.db).await.unwrap();
        assert_eq!(safety(app, &[]).await.unwrap()["safe"], true);
        store::acquire_lease(&app.db, "rebuild", "someone-else", None, None,
            &(chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true), false, None, &json!({}))
            .await.unwrap().unwrap();
        let s = safety(app, &[]).await.unwrap();
        assert_eq!(s["safe"], false, "窗口一次只給一個人");
        assert_eq!(s["held_leases"][0]["owner"], "someone-else");
    }

    /// #1944：claude-api 的 in_flight 回合只有在 run 真的 running、且回合在 15 分鐘內才擋窗口；
    /// 卡在 stopping run 裡的舊回合（2026-11-01 擋了換版兩小時多）不擋。
    #[tokio::test]
    async fn a_stuck_claude_api_turn_does_not_block_the_window_forever() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let now = crate::db::now();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('ab',?,'ab','claude-api','tok-ab',?)")
            .bind(&e.project_id).bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO conversations (id,bot_id,created_at) VALUES ('ca','ab',?)").bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES ('ra','ab','running','idle',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,created_at) VALUES ('ta','ca','ra','web','in_flight','pending',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        assert!(store::delivery_critical_anywhere(&app.db).await.unwrap(), "running 的 run、剛送出的回合：擋");
        sqlx::query("UPDATE runs SET state='stopping' WHERE id='ra'").execute(&app.db).await.unwrap();
        assert!(!store::delivery_critical_anywhere(&app.db).await.unwrap(), "run 卡在 stopping：不擋");
        sqlx::query("UPDATE runs SET state='running' WHERE id='ra'").execute(&app.db).await.unwrap();
        sqlx::query("UPDATE turns SET created_at=? WHERE id='ta'").bind(crate::db::iso_in(-3600)).execute(&app.db).await.unwrap();
        assert!(!store::delivery_critical_anywhere(&app.db).await.unwrap(), "回合掛超過 15 分：不擋");
    }

    /// issue #86 的 TOCTOU：safety 是在拿租約**之前**讀的，中間還是可能有一則 prompt 把 turn
    /// commit 進來。`acquire_lease` 那一句 UPDATE 自己帶了同一份送達臨界區條件，所以寫下去的那一刻
    /// 有人在打字就拿不到——跟 `lifecycle::prompt` 那邊的
    /// `a_held_restart_window_refuses_new_prompts_and_says_who_holds_it` 是一對：兩句都是單句寫入、
    /// 由 SQLite 排序，先 commit 的那個贏，兩個不會同時進送達臨界區。
    #[tokio::test]
    async fn a_window_is_not_taken_while_a_prompt_is_in_the_delivery_critical_section() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let now = crate::db::now();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('tb',?,'tb','claude','tok-tb',?)")
            .bind(&e.project_id).bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO conversations (id,bot_id,created_at) VALUES ('ct','tb',?)").bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES ('rt','tb','running','idle',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        let until = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,created_at) VALUES ('tt','ct','rt','web','in_flight','pending',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        assert!(store::delivery_critical_anywhere(&app.db).await.unwrap());
        let taken = store::acquire_lease(&app.db, "restart", "k8bw2f", None, None, &until, true, None, &json!({})).await.unwrap();
        assert!(taken.is_none(), "有人在送達臨界區：窗口拿不走");
        assert!(window_held(app).await.unwrap().is_none(), "沒拿到就不該有閘門");

        sqlx::query("UPDATE turns SET delivery='ok' WHERE id='tt'").execute(&app.db).await.unwrap();
        assert!(!store::delivery_critical_anywhere(&app.db).await.unwrap());
        let taken = store::acquire_lease(&app.db, "restart", "k8bw2f", None, None, &until, true, None, &json!({})).await.unwrap();
        assert!(taken.is_some(), "沒有人在送達臨界區：拿得到");
        let w = window_held(app).await.unwrap().expect("拿到了就有閘門");
        assert_eq!((w.resource, w.owner.as_str()), ("restart", "k8bw2f"));
        assert!(w.retry_after_secs(&crate::db::now()) > 0);

        sqlx::query("UPDATE turns SET delivery='pending' WHERE id='tt'").execute(&app.db).await.unwrap();
        let rebuild = store::acquire_lease(&app.db, "rebuild", "k8bw2f", None, None, &until, false, None, &json!({})).await.unwrap();
        assert!(rebuild.is_some(), "rebuild 不看送達臨界區");
    }

    /// 2026-09-18：申請者在**自己的回合裡**拿 restart 永遠拿不到——`lease safety --owner X
    /// --exclude-bot X` 說 safe=true，`acquire` 卻連 30 次都 409 `not_idle` / `raced=true`，因為
    /// 送達臨界區那句條件不吃排除，而申請者自己的回合要等 acquire 回來才結束。結果只剩「把腳本
    /// 丟背景、回合先結束」一條路，正好是任務規則 6a 禁止的。
    ///
    /// 修法：放過**申請者自己那一顆**（且必須出現在 `--exclude-bot` 裡），其他 bot 照擋。
    #[tokio::test]
    async fn the_requester_is_not_blocked_by_its_own_turn_but_everyone_else_still_blocks() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let now = crate::db::now();
        for (bot, conv, run) in [("mine", "cm", "rm"), ("other", "co", "ro")] {
            sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES (?,?,?,'claude',?,?)")
                .bind(bot).bind(&e.project_id).bind(bot).bind(format!("tok-{bot}")).bind(&now).execute(&app.db).await.unwrap();
            sqlx::query("INSERT INTO conversations (id,bot_id,created_at) VALUES (?,?,?)")
                .bind(conv).bind(bot).bind(&now).execute(&app.db).await.unwrap();
            sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES (?,?,'running','idle',?)")
                .bind(run).bind(bot).bind(&now).execute(&app.db).await.unwrap();
        }
        let until = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,created_at) VALUES ('t1','cm','rm','web','in_flight','pending',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        assert!(store::acquire_lease(&app.db, "restart", "mine", None, None, &until, true, None, &json!({})).await.unwrap().is_none(),
            "不排除的話，申請者自己的回合就把自己擋在門外——這就是原本的 bug");
        let taken = store::acquire_lease(&app.db, "restart", "mine", None, None, &until, true, Some("mine"), &json!({})).await.unwrap();
        assert!(taken.is_some(), "放過申請者自己那顆之後就拿得到");
        sqlx::query("UPDATE supervisor_leases SET released_at=? WHERE resource='restart'").bind(&now).execute(&app.db).await.unwrap();

        sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,created_at) VALUES ('t2','co','ro','web','in_flight','pending',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        assert!(store::acquire_lease(&app.db, "restart", "mine", None, None, &until, true, Some("mine"), &json!({})).await.unwrap().is_none(),
            "別顆 bot 正在送達臨界區，窗口就不該拿得到");

        sqlx::query("UPDATE turns SET delivery='ok' WHERE id='t2'").execute(&app.db).await.unwrap();
        assert!(store::acquire_lease(&app.db, "restart", "mine", None, None, &until, true, Some("mine"), &json!({})).await.unwrap().is_some());
    }

    /// 排除對象綁核准的 requester：拿自己的核准去排除**別顆** bot，等於把別人正在打字的 pane
    /// 算成閒置，一律拒絕（巡檢 2026-09-18 定的規則）。
    #[tokio::test]
    async fn a_restart_window_may_only_ignore_the_requesters_own_bot() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let now = crate::db::now();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('mine',?,'mine','claude','tok-mine',?)")
            .bind(&e.project_id).bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('other',?,'other','claude','tok-other',?)")
            .bind(&e.project_id).bind(&now).execute(&app.db).await.unwrap();
        let a = store::create_approval_principal_notifying(&app.db, "mine", "restart", "release", None, None, None, None, None,
            &store::ApprovalPrincipal::Bot { bot_id: "mine".into() }).await.unwrap().approval;
        store::decide_approval(&app.db, &a.id, "approved", "AGM", None, None).await.unwrap();

        let err = acquire(app, "restart", "mine", &a.id, None, 300, true, &["other".to_string()]).await.unwrap_err();
        let LcError::Conflict(v) = err else { panic!("要是 409 conflict") };
        assert_eq!(v.get("reason").and_then(|r| r.as_str()), Some("exclude_not_requester"), "{v}");
        assert_eq!(v.get("excluded").and_then(|r| r.as_str()), Some("other"), "{v}");

        let ok = acquire(app, "restart", "mine", &a.id, None, 300, true, &["mine".to_string()]).await.unwrap();
        assert_eq!(ok.get("lease").and_then(|l| l.get("owner")).and_then(|o| o.as_str()), Some("mine"), "{ok}");
    }

    /// 顯示名稱即使唯一也不能證明 requester；只有穩定 bot id 可解析。
    #[tokio::test]
    async fn the_requester_must_be_a_bot_id_instead_of_a_display_name() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let now = crate::db::now();
        sqlx::query(
            "INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('b-m3',?,'AM-m3','claude','tok',?)",
        )
        .bind(&e.project_id).bind(&now).execute(&app.db).await.unwrap();
        sqlx::query(
            "INSERT INTO runs (id,bot_id,state,agent_status,agent_name,started_at) VALUES ('r-m3','b-m3','running','idle','agents-manager-15m2dg',?)",
        )
        .bind(&now).execute(&app.db).await.unwrap();

        assert_eq!(requester_bot_id(app, "b-m3").await.as_deref(), Some("b-m3"), "bot id");
        assert_eq!(requester_bot_id(app, "AM-m3").await.as_deref(), None, "bot 名不是身份");
        assert_eq!(requester_bot_id(app, "agents-manager-15m2dg").await.as_deref(), None, "agent 名不是身份");
        assert!(requester_bot_id(app, "daemon-update-kick").await.is_none(), "腳本身份對不到 bot");
        assert!(requester_bot_id(app, "  ").await.is_none());
    }

    /// issue #681：bot 名只在專案內唯一，不能把跨專案同名的第一筆當成申請者。
    #[tokio::test]
    async fn requester_names_are_unknown_even_when_unique() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let now = crate::db::now();
        let second_project = crate::db::ulid();
        sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES (?,'/tmp/issue-681-second','second',?)")
            .bind(&second_project).bind(&now).execute(&app.db).await.unwrap();
        for (id, project, agent) in [("fixer-a", &e.project_id, "agent-a"), ("fixer-b", &second_project, "agent-b")] {
            sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES (?,?, 'fixer','claude',?,?)")
                .bind(id).bind(project).bind(format!("tok-{id}")).bind(&now).execute(&app.db).await.unwrap();
            sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,agent_name,started_at) VALUES (?,?,'running','idle',?,?)")
                .bind(format!("run-{id}")).bind(id).bind(agent).bind(&now).execute(&app.db).await.unwrap();
        }
        assert_eq!(try_requester_bot_id(&app.db, "agent-a").await.unwrap().as_deref(), None, "唯一 agent 名也不是身份");
        sqlx::query("UPDATE runs SET agent_name='shared-agent' WHERE bot_id IN ('fixer-a','fixer-b')")
            .execute(&app.db).await.unwrap();

        assert_eq!(try_requester_bot_id(&app.db, "fixer-a").await.unwrap().as_deref(), Some("fixer-a"), "bot id 無歧義");
        assert_eq!(try_requester_bot_id(&app.db, "fixer").await.unwrap(), None, "名稱不是身份");
        assert_eq!(requester_bot_id(app, "fixer").await, None, "舊呼叫端遇到歧義必須 fail closed");
        assert_eq!(try_requester_bot_id(&app.db, "shared-agent").await.unwrap(), None, "名稱不是身份");
    }

    #[tokio::test]
    async fn nr_relay_requester_names_are_never_identity_evidence() {
        let e = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&e.app, &e.project_id, "old-requester").await;
        crate::testing::fake_run(&e.app, &bot.id).await;
        assert_eq!(requester_bot_id(&e.app, "old-requester").await, None);
        sqlx::query("UPDATE bots SET name='renamed' WHERE id=?").bind(&bot.id).execute(&e.app.db).await.unwrap();
        crate::testing::claude_bot(&e.app, &e.project_id, "old-requester").await;
        assert_eq!(requester_bot_id(&e.app, "old-requester").await, None);
        assert_eq!(requester_bot_id(&e.app, &bot.id).await, Some(bot.id));
        assert_eq!(requester_bot_id(&e.app, "agent").await, None);
    }

    /// 拿不到窗口時要講得出「還要等多久」：只回 not_idle 會讓人以為沒有出路（2026-09-19 實測）。
    #[tokio::test]
    async fn a_refused_window_says_when_the_approval_escalates() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let now = crate::db::now();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('busy',?,'busy','claude','tok',?)")
            .bind(&e.project_id).bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES ('r-busy','busy','running','working',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        let ap = approved_restart(app, 1).await;

        let err = acquire(app, "restart", "k8bw2f", &ap, None, 300, true, &[]).await.unwrap_err();
        let LcError::Conflict(v) = err else { panic!("要是 409") };
        assert_eq!(v.get("reason").and_then(Value::as_str), Some("not_idle"));
        assert!(v.get("waited_secs").and_then(Value::as_i64).is_some(), "{v}");
        assert_eq!(v.get("escalate_after_secs").and_then(Value::as_i64), Some(escalate_after_secs()), "{v}");
        assert!(v.get("escalates_at").and_then(Value::as_str).is_some(), "要說得出預計什麼時候可以拿：{v}");
        assert!(v.get("hint").and_then(Value::as_str).is_some_and(|h| h.contains("不再擋")), "{v}");
    }

    /// 過期沒 release 的窗口不再是閘門：`held_at` 看 `expires_at`，不需要任何人來收尾（issue #86）。
    #[tokio::test]
    async fn an_expired_window_stops_fencing_by_itself() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let soon = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        store::acquire_lease(&app.db, "restart", "k8bw2f", None, None, &soon, true, None, &json!({})).await.unwrap().unwrap();
        assert!(window_held(app).await.unwrap().is_some());

        let past = (chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        sqlx::query("UPDATE supervisor_leases SET expires_at=? WHERE resource='restart'").bind(&past).execute(&app.db).await.unwrap();
        assert!(store::lease(&app.db, "restart").await.unwrap().unwrap().released_at.is_none(), "沒有人 release");
        assert!(window_held(app).await.unwrap().is_none(), "過期就自動不再擋");
        assert!(dispatch_paused(app).await.unwrap().is_none(), "派工那條讀的是同一份");
    }

    /// issue #127：入場閘門是三態——明確有窗口、明確沒有窗口、**讀不到**。讀不到不等於沒有：
    /// 觀測不到 durable 的租約狀態，不代表已經證明沒有租約。三種讀不到（DB 出錯、那一列解不開、沒放掉卻沒有
    /// 讀得懂的到期時間）都回 `Err`；DB 一恢復就重新判斷，不會永久卡住。
    #[tokio::test]
    async fn the_gate_tells_no_window_from_a_window_it_cannot_read() {
        let e = crate::testing::env().await;
        let app = &e.app;
        assert!(window_held(app).await.unwrap().is_none(), "明確沒有窗口：放行");

        let until = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        store::acquire_lease(&app.db, "restart", "k8bw2f", None, None, &until, false, None, &json!({})).await.unwrap().unwrap();
        let fence = store::lease(&app.db, "restart").await.unwrap().unwrap().fence;
        assert!(window_held(app).await.unwrap().is_some(), "明確有窗口：擋");

        fault::break_lease_reads(&app.db).await;
        let u = window_held(app).await.expect_err("讀不到要回 Err，不能當成沒有窗口");
        assert_eq!(u.resource, "restart");
        assert!(!u.error.is_empty());
        assert!(dispatch_paused(app).await.is_err(), "派工那條讀的是同一份");
        fault::restore_lease_reads(&app.db).await;
        assert!(window_held(app).await.unwrap().is_some(), "DB 恢復後重新判斷：窗口還在就擋，沒有卡在 Err");

        fault::corrupt_lease_row(&app.db).await;
        assert!(window_held(app).await.is_err(), "解不開＝讀不到");
        sqlx::query("UPDATE supervisor_leases SET fence=? WHERE resource='restart'").bind(fence).execute(&app.db).await.unwrap();
        assert!(window_held(app).await.unwrap().is_some(), "修好之後恢復");

        for bad in [None, Some("not-a-time")] {
            sqlx::query("UPDATE supervisor_leases SET expires_at=? WHERE resource='restart'").bind(bad).execute(&app.db).await.unwrap();
            let u = window_held(app).await.expect_err("到期時間讀不懂＝讀不到");
            assert!(u.error.contains("expires_at"), "{}", u.error);
        }
        sqlx::query("UPDATE supervisor_leases SET released_at=? WHERE resource='restart'").bind(crate::db::now()).execute(&app.db).await.unwrap();
        assert!(window_held(app).await.unwrap().is_none(), "放掉了就是放掉了");
    }

    /// 錯誤要**可觀察、可區分**：503 `maintenance_state_unavailable` 跟 409 `maintenance_window` 分得開
    /// （使用者才知道是「真的有人在維護」還是「狀態暫時讀不到」），而且明講一個字都沒送、可以原樣重送。
    /// 不是偽裝成一個假的長 TTL 租約。
    #[tokio::test]
    async fn an_unreadable_window_is_a_503_that_says_nothing_was_sent_and_a_real_window_stays_a_409() {
        use axum::response::IntoResponse as _;
        let e = crate::testing::env().await;
        let app = &e.app;
        let until = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        store::acquire_lease(&app.db, "restart", "k8bw2f", None, None, &until, false, None, &json!({})).await.unwrap().unwrap();

        let real = window_held(app).await.unwrap().unwrap().refusal();
        assert_eq!(real.into_response().status(), axum::http::StatusCode::CONFLICT, "真的有窗口：409");

        fault::break_lease_reads(&app.db).await;
        let LcError::Unavailable(body) = window_held(app).await.unwrap_err().refusal() else { panic!("要是 Unavailable") };
        assert_eq!(body["reason"], "maintenance_state_unavailable", "{body}");
        assert_eq!(body["retryable"], true, "{body}");
        assert_eq!(body["sent"], false, "{body}");
        assert_eq!(body["resource"], "restart", "{body}");
        assert!(body["retry_after_secs"].as_i64().unwrap_or(0) > 0, "{body}");
        assert!(body.get("held_by").is_none() && body.get("expires_at").is_none(), "不編一個假的持有者／到期時間：{body}");

        let resp = window_held(app).await.unwrap_err().refusal().into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE, "讀不到：5xx（可重試），不是 409");
        assert!(resp.headers().get(axum::http::header::RETRY_AFTER).is_some(), "帶 Retry-After");
    }

    /// 已核准、還沒用掉的 restart 窗口（同 [`approved_window`]，只是用途不同）。申請者是 `k8bw2f`。
    async fn approved_restart(app: &Arc<App>, mins: i64) -> String {
        let a = store::create_approval(&app.db, "k8bw2f", "restart", "release", None, None, None).await.unwrap().approval;
        store::decide_approval(&app.db, &a.id, "approved", "AGM", None, None).await.unwrap();
        let at = (chrono::Utc::now() - chrono::Duration::minutes(mins)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        sqlx::query("UPDATE supervisor_approvals SET decided_at=? WHERE id=?").bind(&at).bind(&a.id).execute(&app.db).await.unwrap();
        a.id
    }

    async fn hold_rebuild(app: &Arc<App>, owner: &str) {
        store::acquire_lease(&app.db, "rebuild", owner, None, None,
            &(chrono::Utc::now() + chrono::Duration::minutes(50)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true), false, None, &json!({}))
            .await.unwrap().unwrap();
    }

    /// 2026-09-16 09:26Z 實況：k8bw2f 先拿 rebuild（fence 21）、build 完要拿 restart，`held_leases` 列出來的就是它自己那把。
    /// 標準換版流程（同一人先 rebuild 後 restart）不能被自己卡到 rebuild 到期。
    #[tokio::test]
    async fn my_own_rebuild_lease_does_not_block_my_escalated_restart() {
        let e = crate::testing::env().await;
        let (app, pid) = (&e.app, e.project_id.clone());
        thinking_bot(app, &pid, "user").await;
        hold_rebuild(app, "k8bw2f").await;
        let restart = approved_restart(app, 40).await;

        let taken = acquire(app, "restart", "k8bw2f", &restart, None, 300, true, &[]).await.unwrap();
        assert_eq!(taken["safety"]["escalated"], true);
        assert_eq!(taken["safety"]["held_leases"][0]["owner"], "k8bw2f");
        assert_eq!(taken["safety"]["held_leases"][0]["own"], true, "自己的那把要標出來，而且不算擋");
        assert_eq!(taken["lease"]["owner"], "k8bw2f");
    }

    /// 別人握著 rebuild 時照擋：「窗口一次只給一個人」的語意不變。
    #[tokio::test]
    async fn someone_elses_rebuild_lease_still_blocks_an_escalated_restart() {
        let e = crate::testing::env().await;
        let (app, pid) = (&e.app, e.project_id.clone());
        thinking_bot(app, &pid, "user").await;
        hold_rebuild(app, "someone-else").await;
        let restart = approved_restart(app, 40).await;

        let refused = acquire(app, "restart", "k8bw2f", &restart, None, 300, true, &[]).await.unwrap_err();
        let LcError::Conflict(detail) = &refused else { panic!("expected a conflict, got {refused:?}") };
        assert_eq!(detail["reason"], "not_idle");
        assert_eq!(detail["safety"]["escalated"], true, "有升級，擋下來的是別人的租約");
        assert_eq!(detail["safety"]["held_leases"][0]["owner"], "someone-else");
        assert_eq!(detail["safety"]["held_leases"][0]["own"], false);
        assert!(store::lease(&app.db, "restart").await.unwrap().is_none(), "拿不到就什麼都不該留下");
    }

    /// 唯讀 safety：不帶 owner 維持舊行為（全部列出、全部算擋）；帶了 owner 用同一條規則排除自己的。
    #[tokio::test]
    async fn read_only_safety_only_excludes_leases_when_asked_as_their_owner() {
        let e = crate::testing::env().await;
        let (app, pid) = (&e.app, e.project_id.clone());
        thinking_bot(app, &pid, "user").await;
        hold_rebuild(app, "k8bw2f").await;
        let restart = approved_restart(app, 40).await;

        let anonymous = safety_for(app, &[], Some(&restart)).await.unwrap();
        assert_eq!((anonymous["escalated"].as_bool(), anonymous["safe"].as_bool()), (Some(true), Some(false)), "不帶 owner：每一把都算擋");
        assert_eq!(anonymous["held_leases"][0]["owner"], "k8bw2f");
        assert_eq!(anonymous["held_leases"][0]["own"], false);
        assert_eq!(anonymous["owner"], Value::Null);

        let mine = safety_as(app, &[], Some(&restart), Some("k8bw2f")).await.unwrap();
        assert_eq!(mine["safe"], true, "以握著它的人問：自己的租約不擋");
        assert_eq!(mine["held_leases"][0]["own"], true);
        assert_eq!(mine["owner"], "k8bw2f");

        assert_eq!(safety_as(app, &[], Some(&restart), Some("someone-else")).await.unwrap()["safe"], false, "別人問照擋");

        sqlx::query("DELETE FROM supervisor_approvals").execute(&app.db).await.unwrap();
        let strict = safety_as(app, &[], None, Some("k8bw2f")).await.unwrap();
        assert_eq!(strict["escalated"], false);
        assert_eq!(strict["safe"], false, "非升級模式行為不變：有人在思考就不安全");
    }

    /// AGM 三顆（巡檢、協調者、建置 child）由呼叫端排除，門檻高低都一樣——放寬不是「連排除都不管了」。
    #[tokio::test]
    async fn excluded_agm_bots_are_ignored_at_both_thresholds() {
        let e = crate::testing::env().await;
        let (app, pid) = (&e.app, e.project_id.clone());
        for id in ["manager", "responder", "builder"] {
            thinking_bot(app, &pid, id).await;
        }
        sqlx::query("UPDATE turns SET delivery='pending' WHERE id='builder'").execute(&app.db).await.unwrap();
        let exclude = ["manager".to_string(), "responder".to_string(), "builder".to_string()];

        let strict = safety(app, &exclude).await.unwrap();
        assert_eq!((strict["escalated"].as_bool(), strict["safe"].as_bool()), (Some(false), Some(true)));
        assert!(strict["working"].as_array().unwrap().is_empty());

        approved_window(app, 40).await;
        let relaxed = safety(app, &exclude).await.unwrap();
        assert_eq!((relaxed["escalated"].as_bool(), relaxed["safe"].as_bool()), (Some(true), Some(true)));
        assert!(relaxed["delivering"].as_array().unwrap().is_empty(), "被排除的 bot 連送達中都不算數");
        assert_eq!(safety(app, &[]).await.unwrap()["safe"], false);
    }

    /// 核准是給申請的那個人的：bot B 拿 bot A 已核准的申請 acquire，不開窗口、也不借走 A 的等待（review2 sup #5）。
    #[tokio::test]
    async fn someone_elses_approval_does_not_open_my_window() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let a = approved_window(app, 40).await;
        let refused = acquire(app, "rebuild", "bot-b", &a, None, 300, true, &[]).await.unwrap_err();
        let LcError::Conflict(detail) = &refused else { panic!("expected a conflict, got {refused:?}") };
        assert_eq!(detail["reason"], "approval_owner_mismatch");
        assert!(store::lease(&app.db, "rebuild").await.unwrap().is_none(), "什麼都沒拿到");
        assert_eq!(store::approval(&app.db, &a).await.unwrap().unwrap().status, "approved", "A 的核准原封不動");
        acquire(app, "rebuild", "ops", &a, None, 300, true, &[]).await.expect("申請者自己照常拿得到");
    }

    /// 執行端拿了窗口之後掛掉、租約過期沒 release：用**同一張**核准重試不能再開一個窗口（review2 sup #6）。
    #[tokio::test]
    async fn an_approval_whose_window_expired_unreleased_cannot_open_another() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let a = approved_window(app, 0).await;
        acquire(app, "rebuild", "ops", &a, None, 300, true, &[]).await.unwrap();
        let decided_at = store::approval(&app.db, &a).await.unwrap().unwrap().decided_at;
        sqlx::query("UPDATE supervisor_leases SET expires_at='2000-01-01T00:00:00Z' WHERE resource='rebuild'").execute(&app.db).await.unwrap();

        let refused = acquire(app, "rebuild", "ops", &a, None, 300, true, &[]).await.unwrap_err();
        let LcError::Conflict(detail) = &refused else { panic!("expected a conflict, got {refused:?}") };
        assert_eq!(detail["reason"], "approval_already_used");
        let after = store::approval(&app.db, &a).await.unwrap().unwrap();
        assert_eq!(after.status, "consumed", "那個窗口已經用掉它，當場記下來");
        assert_eq!(after.decided_at, decided_at, "消耗不是裁示，不覆寫核准時間");
        assert_eq!(after.decided_by.as_deref(), Some("AGM"));
        let notes = store::approval_decisions(&app.db).await.unwrap();
        assert!(notes.get(&a).is_some_and(|v| v.iter().any(|n| n["to"] == "consumed")), "誰在什麼時候消耗的記在歷程裡");

        let b = approved_window(app, 0).await;
        acquire(app, "rebuild", "ops", &b, None, 300, true, &[]).await.expect("別張核准照常接手");
    }

    /// release 把窗口收掉、但「消耗核准」那一步沒寫成（DB 出錯，或兩步之間 daemon 死了）：核准還是 `approved`，
    /// 租約卻已 released——過期的檢查只看「沒 release 的過期租約」，同一張核准就能再開第二個窗口。
    /// 一次核准一個窗口：租約已經用這張開過，不管後來是過期還是 release 掉，都不能再用。
    #[tokio::test]
    async fn an_approval_whose_window_was_released_but_never_consumed_cannot_open_another() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let a = approved_window(app, 0).await;
        let lease = acquire(app, "rebuild", "ops", &a, None, 300, true, &[]).await.unwrap();
        let fence = lease["lease"]["fence"].as_i64().unwrap();
        let token = lease["lease_token"].as_str().unwrap().to_string();
        sqlx::query("CREATE TRIGGER no_consume BEFORE UPDATE ON supervisor_approvals WHEN NEW.status='consumed' BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END")
            .execute(&app.db).await.unwrap();
        assert!(release(app, "rebuild", "ops", fence, store::LeaseProof::Token(&token)).await.unwrap());
        sqlx::query("DROP TRIGGER no_consume").execute(&app.db).await.unwrap();
        assert_eq!(store::approval(&app.db, &a).await.unwrap().unwrap().status, "approved", "前提：消耗沒寫成");

        let refused = acquire(app, "rebuild", "ops", &a, None, 300, true, &[]).await.unwrap_err();
        let LcError::Conflict(detail) = &refused else { panic!("expected a conflict, got {refused:?}") };
        assert_eq!(detail["reason"], "approval_already_used");
        assert_eq!(store::approval(&app.db, &a).await.unwrap().unwrap().status, "consumed", "順手補記消耗");
    }

    /// acquire 的回應（含只出現一次的 `lease_token`）送丟了：帶同一個 `request_id` 重送要拿回**同一張**租約
    /// （同 fence、同 token），不能多開一張，也不能讓窗口握到 TTL 沒人收得掉。request_id 是只有申請端知道的祕密，
    /// 所以只有它拿得回 token；沒帶、換一個、窗口已經還掉，都照舊走完整的 acquire。
    #[tokio::test]
    async fn a_retried_acquire_with_the_same_request_id_gets_the_same_lease_back() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let a = approved_window(app, 0).await;
        let first = acquire_with_request(app, "rebuild", "ops", &a, None, 300, true, &[], Some("req-secret-1")).await.unwrap();
        assert!(first.get("replayed").is_none(), "第一次不是重送：{first}");

        let again = acquire_with_request(app, "rebuild", "ops", &a, None, 300, true, &[], Some("req-secret-1")).await.unwrap();
        assert_eq!(again["replayed"], true);
        assert_eq!(again["lease_token"], first["lease_token"], "同一張租約的同一個 token");
        assert_eq!(again["lease"]["fence"], first["lease"]["fence"], "fence 不能再往上加");
        assert_eq!(again["lease"]["expires_at"], first["lease"]["expires_at"], "重送不延長租約");
        assert_eq!(store::lease(&app.db, "rebuild").await.unwrap().unwrap().fence, 1);

        for rid in [Some("req-other"), None] {
            let refused = acquire_with_request(app, "rebuild", "ops", &a, None, 300, true, &[], rid).await.unwrap_err();
            let LcError::Conflict(detail) = &refused else { panic!("expected a conflict, got {refused:?}") };
            assert_eq!(detail["reason"], "lease_held", "{rid:?}");
        }
        assert!(matches!(
            acquire_with_request(app, "rebuild", "ops", &a, None, 300, true, &[], Some("   ")).await,
            Err(LcError::Bad(_))
        ));

        assert!(store::release_lease(&app.db, "rebuild", "ops", 1).await.unwrap());
        let refused = acquire_with_request(app, "rebuild", "ops", &a, None, 300, true, &[], Some("req-secret-1")).await.unwrap_err();
        let LcError::Conflict(detail) = &refused else { panic!("expected a conflict, got {refused:?}") };
        assert_eq!(detail["reason"], "approval_already_used", "已還掉的租約不能靠 request_id 救回來");
    }

    /// 租約沒 release、只是過期（執行端掛了）：同一個 `request_id` 重送不能把它救回來，也不能延長——
    /// 即使 `expires_at` 是舊版寫的秒格式、這一秒稍早已過期（#101 的同一類誤判：字串比會把它當成還握著）。
    #[tokio::test]
    async fn an_expired_lease_cannot_be_rescued_by_replaying_its_request_id() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let a = approved_window(app, 0).await;
        let first = acquire_with_request(app, "rebuild", "ops", &a, None, 300, true, &[], Some("req-exp")).await.unwrap();
        assert!(first.get("replayed").is_none());
        let now = chrono::Utc::now();
        let earlier_this_second = now.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        assert!(crate::db::cmp_ts(&earlier_this_second, &crate::db::now()).is_le(), "前提");
        sqlx::query("UPDATE supervisor_leases SET expires_at=? WHERE resource='rebuild'").bind(&earlier_this_second).execute(&app.db).await.unwrap();

        let refused = acquire_with_request(app, "rebuild", "ops", &a, None, 300, true, &[], Some("req-exp")).await.unwrap_err();
        let LcError::Conflict(detail) = &refused else { panic!("expected a conflict, got {refused:?}") };
        assert_eq!(detail["reason"], "approval_already_used", "過期的租約不能靠 request_id 救回來：{detail}");
        let l = store::lease(&app.db, "rebuild").await.unwrap().unwrap();
        assert_eq!(l.expires_at.as_deref(), Some(earlier_this_second.as_str()), "也沒有被延長");
    }

    /// 換 commit 重新申請（supersedes）：舊的標 superseded、它的等待接過來，升級計時不因為 main 動了就歸零（review2 sup 新發現 2）。
    #[tokio::test]
    async fn a_new_commit_carries_the_wait_of_the_approval_it_supersedes() {
        let e = crate::testing::env().await;
        let (app, pid) = (&e.app, e.project_id.clone());
        thinking_bot(app, &pid, "user").await;
        let old = approved_window(app, 40).await;
        let decided_old = store::approval(&app.db, &old).await.unwrap().unwrap().decided_at.unwrap();

        let out = store::create_approval_superseding(&app.db, "ops", "rebuild", "release", Some("h2"), None, None, Some(&old), None).await.unwrap();
        assert_eq!(out.superseded.as_deref(), Some(old.as_str()));
        let new = out.approval;
        assert_eq!(new.wait_since.as_deref(), Some(decided_old.as_str()), "接過來的是舊那筆被核准的時間");
        let o = store::approval(&app.db, &old).await.unwrap().unwrap();
        assert_eq!(o.status, "superseded");
        assert_eq!(o.refusal(&crate::db::now(), "rebuild", None), Some("approval_superseded"));
        assert_eq!(o.decided_at.as_deref(), Some(decided_old.as_str()), "取代不覆寫核准時間");

        store::decide_approval(&app.db, &new.id, "approved", "AGM", None, None).await.unwrap();
        let s = safety_for(app, &[], Some(&new.id)).await.unwrap();
        assert_eq!((s["escalated"].as_bool(), s["safe"].as_bool()), (Some(true), Some(true)), "{s}");
        assert!(s["waited_secs"].as_i64().unwrap() >= 2400);

        let third = store::create_approval_superseding(&app.db, "ops", "rebuild", "release", Some("h3"), None, None, Some(&new.id), None).await.unwrap().approval;
        assert_eq!(third.wait_since.as_deref(), Some(decided_old.as_str()));
        for (who, purpose) in [("bot-b", "rebuild"), ("ops", "restart")] {
            let err = store::create_approval_superseding(&app.db, who, purpose, "release", Some("h4"), None, None, Some(&third.id), None).await.unwrap_err();
            assert!(err.downcast_ref::<store::ApprovalSupersedeRefused>().is_some(), "{who}/{purpose}: {err}");
        }
        assert_eq!(store::approval(&app.db, &third.id).await.unwrap().unwrap().status, "pending", "被拒的取代什麼都不動");

        store::decide_approval(&app.db, &third.id, "denied", "AGM", None, None).await.unwrap();
        let fresh = store::create_approval_superseding(&app.db, "ops", "rebuild", "release", Some("h5"), None, None, Some(&third.id), None).await.unwrap();
        assert_eq!((fresh.superseded, fresh.approval.wait_since), (None, None));
        assert_eq!(store::approval(&app.db, &third.id).await.unwrap().unwrap().status, "denied");
    }

    /// 被取代那筆還沒送給協調者的 `approval_requested` 一起收掉，不要叫它醒來裁示一筆作廢的申請。
    #[tokio::test]
    async fn superseding_retires_the_old_requests_unsent_inbox_event() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let old = store::create_approval(&app.db, "ops", "rebuild", "release", Some("h1"), None, None).await.unwrap().approval;
        store::push_inbox(&app.db, &format!("approval:{}:requested", old.id), "approval_requested", None, None, None, &json!({})).await.unwrap();
        store::create_approval_superseding(&app.db, "ops", "rebuild", "release", Some("h2"), None, None, Some(&old.id), None).await.unwrap();
        let (state, acked): (String, Option<String>) = sqlx::query_as("SELECT state, acked_by FROM supervisor_inbox WHERE event_key=?")
            .bind(format!("approval:{}:requested", old.id))
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!((state.as_str(), acked.as_deref()), ("handled", Some("daemon")));
    }

    fn approval(status: &str, purpose: &str, commit: Option<&str>, expires: Option<&str>) -> Approval {
        Approval {
            request_reason: None,
            requester_unverified: 1,
            id: "ap1".into(),
            supervisor_id: "AGM".into(),
            requester: "bot-x".into(),
            purpose: purpose.into(),
            scope: "daemon".into(),
            target_commit: commit.map(str::to_string),
            status: status.into(),
            decided_by: Some("AGM".into()),
            decided_at: Some("2026-09-12T00:00:00Z".into()),
            reason: None,
            expires_at: expires.map(str::to_string),
            client_request_id: None,
            created_at: "2026-09-12T00:00:00Z".into(),
            updated_at: "2026-09-12T00:00:00Z".into(),
            wait_since: None,
        }
    }

    const NOW: &str = "2026-09-12T12:00:00Z";

    #[test]
    fn only_an_approved_unexpired_approval_for_this_commit_counts() {
        let ok = approval("approved", "rebuild", Some("abc123"), Some("2026-09-12T13:00:00Z"));
        assert_eq!(ok.refusal(NOW, "rebuild", Some("abc123")), None);
        assert_eq!(ok.refusal(NOW, "rebuild", Some("def456")), Some("approval_commit_mismatch"));
        assert_eq!(ok.refusal(NOW, "restart", Some("abc123")), Some("approval_purpose_mismatch"));
        assert_eq!(ok.refusal(NOW, "rebuild", None), Some("approval_commit_required"));
        let expired = approval("approved", "rebuild", Some("abc123"), Some("2026-09-12T11:00:00Z"));
        assert_eq!(expired.refusal(NOW, "rebuild", Some("abc123")), Some("approval_expired"));
    }

    #[test]
    fn a_revoked_or_undecided_approval_is_not_a_yes() {
        for (status, why) in [
            ("pending", "approval_not_decided"),
            ("denied", "approval_denied"),
            ("revoked", "approval_revoked"),
            ("consumed", "approval_already_used"),
        ] {
            let a = approval(status, "rebuild", None, None);
            assert_eq!(a.refusal(NOW, "rebuild", None), Some(why), "{status}");
        }
        assert_eq!(approval("approved", "rebuild", None, None).refusal(NOW, "rebuild", Some("x")), None);
    }

    #[test]
    fn a_restart_window_pauses_dispatch_but_a_rebuild_does_not() {
        assert!(EXCLUSIVE.contains(&"restart"));
        assert!(!EXCLUSIVE.contains(&"rebuild"));
        assert!(pause_note("2026-09-12T12:15:00Z").contains("12:15"));
    }

    /// A lease is a permission with a clock on it, so it cannot outlive the permission — on
    /// acquire *or* on renew. Otherwise "approved until 14:00" quietly becomes "holding the box
    /// until 14:45".
    #[test]
    fn a_lease_never_outlives_its_approval() {
        let asked = "2026-09-12T12:45:00Z";
        assert_eq!(lease_deadline(asked, Some("2026-09-12T14:00:00Z")), asked, "approval outlasts it: keep the ask");
        assert_eq!(
            lease_deadline(asked, Some("2026-09-12T12:10:00Z")),
            "2026-09-12T12:10:00Z",
            "approval lapses first: the lease ends with it"
        );
        assert_eq!(lease_deadline(asked, None), asked);
    }
}
