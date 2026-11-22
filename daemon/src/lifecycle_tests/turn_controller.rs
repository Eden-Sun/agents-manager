#![allow(unused_imports)]
//! turn_controller 的測試（#1862 L2-3e 自 crate 搬來）。
mod tests {
    use crate::lifecycle::turn_controller::*;
    use crate::lifecycle::*;
    use crate::lifecycle_tests::prelude::*;

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

        sqlx::query("UPDATE turns SET status='queued' WHERE id='t1'").execute(&app.db).await.expect("in_flight -> queued 是合法的");
        sqlx::query("UPDATE turns SET status='in_flight' WHERE id='t1'").execute(&app.db).await.unwrap();
        sqlx::query("UPDATE turns SET status='completed' WHERE id='t1'").execute(&app.db).await.unwrap();

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
        sqlx::query("UPDATE turns SET status='completed' WHERE id='t1'").execute(&app.db).await.unwrap();
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

        let t = turn_in(app, &conv, Some(&run), "in_flight", "unknown").await;
        let mut c = app.db.acquire().await.unwrap();
        assert_eq!(complete_with_native_evidence(&mut c, &t, &admitted, ev("s1", "p1")).await.unwrap(), Outcome::Applied);
        let r = row(app, &t).await;
        assert_eq!((r.status.as_str(), r.delivery.as_str()), ("completed", "ok"));
        assert_eq!((r.native_session_id.as_deref(), r.native_turn_id.as_deref()), (Some("s1"), Some("p1")));
        let first_done = r.completed_at.clone();
        assert!(first_done.is_some());
        assert_eq!(
            complete_with_native_evidence(&mut c, &t, &admitted, ev("s1", "p2")).await.unwrap(),
            Outcome::Raced { now: "completed".into() }
        );
        assert_eq!(fail_with_native_evidence(&mut c, &t, &admitted, ev("s1", "p3")).await.unwrap(), Outcome::Raced { now: "completed".into() });
        let r = row(app, &t).await;
        assert_eq!((r.native_turn_id.as_deref(), r.completed_at), (Some("p1"), first_done));

        let other_run = crate::testing::fake_run(app, &crate::testing::claude_bot(app, &e.project_id, "shape-other").await.id).await;
        let stale = turn_in(app, &conv, Some(&other_run), "in_flight", "ok").await;
        assert!(matches!(complete_with_native_evidence(&mut c, &stale, &admitted, ev("s9", "p9")).await.unwrap(), Outcome::Fenced(_)));
        assert!(matches!(fail_with_native_evidence(&mut c, &stale, &admitted, ev("s9", "p9")).await.unwrap(), Outcome::Fenced(_)));
        let r = row(app, &stale).await;
        assert_eq!((r.status.as_str(), r.native_turn_id), ("in_flight", None), "一個欄位都沒改");

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

        let q = turn_in(app, &conv, None, "queued", "pending").await;
        assert_eq!(claim_queued(&mut c, &q, &run).await.unwrap(), Outcome::Applied);
        assert_eq!(retract_queued(&mut c, &q).await.unwrap(), Outcome::Raced { now: "in_flight".into() });
        let r = row(app, &q).await;
        assert_eq!((r.status.as_str(), r.delivery.as_str(), r.run_id.as_deref()), ("in_flight", "pending", Some(run.as_str())));

        assert_eq!(return_to_queue(&mut c, &q, "2099-01-01T00:00:00.000Z").await.unwrap(), Outcome::Applied);
        let r = row(app, &q).await;
        assert_eq!((r.status.as_str(), r.run_id.as_deref(), r.flush_retries), ("queued", None, 1));
        assert_eq!(return_to_queue(&mut c, &q, "2099-01-01T00:00:00.000Z").await.unwrap(), Outcome::Raced { now: "queued".into() });
        assert_eq!(row(app, &q).await.flush_retries, 1, "沒放回去就不多記一次");

        assert_eq!(retract_queued(&mut c, &q).await.unwrap(), Outcome::Applied);
        let r = row(app, &q).await;
        assert_eq!((r.status.as_str(), r.delivery.as_str(), r.next_flush_at), ("failed", "failed", None));
        assert!(r.completed_at.is_some());
        assert_eq!(claim_queued(&mut c, &q, &run).await.unwrap(), Outcome::Raced { now: "failed".into() });
        assert_eq!(retract_queued(&mut c, &q).await.unwrap(), Outcome::Raced { now: "failed".into() }, "重複撤銷冪等");

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

        assert_eq!(
            set_status(&app.db, "s1", "in_flight", "failed", "測試").await.unwrap(),
            Outcome::Raced { now: "failed".into() },
        );
        assert_eq!(set_status(&app.db, "nope", "in_flight", "failed", "測試").await.unwrap(), Outcome::Missing);
        let err = set_status(&app.db, "s1", "failed", "in_flight", "測試").await.unwrap_err();
        assert!(format!("{err}").contains("illegal turn status transition"), "{err}");
    }
}
