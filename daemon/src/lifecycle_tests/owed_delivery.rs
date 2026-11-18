#![allow(unused_imports)]
//! #1862 L2-1：從 am-lifecycle 搬來、需要真 daemon App／DB 的測試。

mod tests {
    use crate::lifecycle_tests::prelude::*;
    use crate::lifecycle::owed_delivery::test_seams::*;
    // parent-uses
    use crate::lifecycle::*;
    use crate::capabilities::Db;
    use crate::lifecycle::s6_ports::OwedDeliveryContext;
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use crate::lifecycle::owed_delivery::*;
    use crate::testing as tt;
    /// 一顆閒著、打字進 pane 的 bot（同 `delivery::api_tests::idle_bot`）。
    async fn idle_bot(env: &tt::Env, kind: &str) -> (String, String, String) {
        let app = &env.app;
        let bot = tt::claude_bot(app, &env.project_id, &format!("owed-{}", db::ulid())).await;
        sqlx::query("UPDATE bots SET kind=? WHERE id=?").bind(kind).bind(&bot.id).execute(app.db()).await.unwrap();
        let conv = db::conversation_id(app.db(), &bot.id).await.unwrap();
        let run = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, pane_typed, started_at)
             VALUES (?,?,'running','idle','ws-1','pane-api','api-bot','test',1,?)",
        )
        .bind(&run)
        .bind(&bot.id)
        .bind(db::now())
        .execute(app.db())
        .await
        .unwrap();
        (bot.id, conv, run)
    }
    /// SQLite 這一刻寫不進 `turns.delivery`（磁碟、鎖、I/O 錯誤）：送達結果寫回、收成 `failed`（它也寫 delivery）都寫不進去。
    async fn lose_delivery_writes(app: &Arc<App>) {
        sqlx::query("CREATE TRIGGER lost_delivery_write BEFORE UPDATE OF delivery ON turns BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END")
            .execute(app.db())
            .await
            .unwrap();
    }
    async fn heal_delivery_writes(app: &Arc<App>) {
        sqlx::query("DROP TRIGGER lost_delivery_write").execute(app.db()).await.unwrap();
    }
    async fn turn(app: &Arc<App>, id: &str) -> db::Turn {
        sqlx::query_as("SELECT * FROM turns WHERE id=?").bind(id).fetch_one(app.db()).await.unwrap()
    }
    fn typed(env: &tt::Env) -> usize {
        env.herdr.methods().iter().filter(|m| *m == "pane.send_text").count()
    }
    fn uncommitted(res: LcResult<PromptOut>) -> Value {
        match res {
            Err(LcError::Uncommitted(body)) => body,
            Ok(out) => panic!("送達結果沒寫成，卻回了普通的 {:?}", out.delivery),
            Err(e) => panic!("要 503 delivery_state_uncommitted，拿到 {e:?}"),
        }
    }
    /// daemon 重啟：記憶體裡的帳跟著上一個行程走了。
    fn process_restarts(bot_id: &str) {
        ledger().lock().unwrap().remove(bot_id);
    }
    /// #149 驗收一：字真的打進去、證據也有（`Submitted`），送達結果卻寫不進 DB——API 不能回普通的 `ok`。
    /// DB 那一筆停在 in_flight＋pending，而且「不自動重送」在打字之前就寫死了（驗收二）。
    #[tokio::test]
    async fn a_typed_prompt_whose_delivery_write_is_lost_is_not_reported_as_delivered() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, _conv, _run) = idle_bot(&env, "claude").await;
        env.herdr.live_pane("pane-api", tt::LivePane { width: Some(120), ..Default::default() });
        lose_delivery_writes(&app).await;

        let body = uncommitted(prompt(&app, &bot, "Reply with PONG please", "crid-lost").await);
        assert_eq!(body["error"], "delivery_state_uncommitted", "{body}");
        assert_eq!(body["delivery"], "ok", "看到的是送到了：{body}");
        assert_eq!(body["sent"], true, "{body}");
        assert_eq!(body["retryable"], true, "{body}");
        let t = turn(&app, body["turn_id"].as_str().unwrap()).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("in_flight", "pending"), "寫不進去就是還沒寫：不假裝寫了");
        assert_eq!(t.auto_resend, 0, "打字之前就寫死「不自動重送」：送達結果丟了也不會變成可以重送");
        assert_eq!(typed(&env), 1);
    }
    /// #149 驗收二：打過字、證不明（grok 多行）的那條路，正確紀錄是 `auto_resend=0`——寫回失敗時 DB 也不能留著預設的 1。
    /// DB 好了之後同一個 request id 重問，拿到的是寫好的 `unverified`，重送額度也照樣用掉。
    #[tokio::test]
    async fn a_typed_but_unprovable_prompt_keeps_its_no_resend_evidence_through_a_lost_write() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, _conv, _run) = idle_bot(&env, "grok").await;
        env.herdr.live_pane("pane-api", tt::LivePane { width: Some(120), boxed: true, ..Default::default() });
        lose_delivery_writes(&app).await;

        let body = uncommitted(prompt(&app, &bot, "第一行\n第二行", "crid-grok").await);
        assert_eq!(body["delivery"], "unverified", "{body}");
        let turn_id = body["turn_id"].as_str().unwrap().to_string();
        assert_eq!(turn(&app, &turn_id).await.auto_resend, 0, "打過字的那一則絕不可自動重送");

        heal_delivery_writes(&app).await;
        let out = prompt(&app, &bot, "第一行\n第二行", "crid-grok").await.expect("DB 好了：同一個 request id 拿到寫好的結果");
        assert_eq!((out.turn_id.as_str(), out.delivery.as_str()), (turn_id.as_str(), "unverified"));
        let t = turn(&app, &turn_id).await;
        assert_eq!((t.delivery.as_str(), t.delivery_verified, t.auto_resend), ("ok", 0, 0));
        let resends: i64 = sqlx::query_scalar("SELECT resend_count FROM turns WHERE id=?").bind(&turn_id).fetch_one(app.db()).await.unwrap();
        assert_eq!(resends, MAX_PROMPT_RESENDS, "回滾到舊 binary 也不會重打");
        assert_eq!(typed(&env), 1, "從頭到尾只打了一次");
    }
    /// 修法方向：同一個 request id 重試不能再送一次——那一筆已經存在、字可能已經進去了。DB 還壞著時重試照樣不是成功；
    /// 好了之後拿到寫好的結果，`delivered_at` 是當初送出的那一刻，不是補寫的那一刻。
    #[tokio::test]
    async fn retrying_the_same_request_never_types_it_again_and_answers_once_the_write_lands() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, _conv, _run) = idle_bot(&env, "claude").await;
        env.herdr.live_pane("pane-api", tt::LivePane { width: Some(120), ..Default::default() });
        lose_delivery_writes(&app).await;

        let body = uncommitted(prompt(&app, &bot, "Reply with PONG please", "crid-retry").await);
        let turn_id = body["turn_id"].as_str().unwrap().to_string();
        let sent_by = db::now();
        let again = uncommitted(prompt(&app, &bot, "Reply with PONG please", "crid-retry").await);
        assert_eq!(again["turn_id"], turn_id.as_str(), "{again}");
        assert_eq!(typed(&env), 1, "DB 還壞著：重試不再打字");

        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        heal_delivery_writes(&app).await;
        let out = prompt(&app, &bot, "Reply with PONG please", "crid-retry").await.expect("DB 好了：重試拿到寫好的結果");
        assert_eq!((out.turn_id.as_str(), out.delivery.as_str()), (turn_id.as_str(), "ok"));
        let t = turn(&app, &turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str(), t.delivery_verified), ("in_flight", "ok", 1));
        let at = t.delivered_at.expect("送達時間補上了");
        assert!(at <= sent_by, "送達時間是送出的那一刻（{at}），不是補寫的時候（{sent_by} 之後）");
        assert_eq!(typed(&env), 1);
    }
    /// #149 驗收三：停在這個半套狀態時 daemon 馬上重啟（帳只在記憶體，沒了）——不重送，收斂成看得見的 `unknown`，
    /// 不是永遠 pending（那一筆會一直佔著維護窗口的送達臨界區）。「不自動重送」與送達時間都還在。
    #[tokio::test]
    async fn a_restart_in_the_partial_state_converges_to_unknown_without_a_second_send() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, _conv, _run) = idle_bot(&env, "claude").await;
        env.herdr.live_pane("pane-api", tt::LivePane { width: Some(120), ..Default::default() });
        lose_delivery_writes(&app).await;
        let body = uncommitted(prompt(&app, &bot, "Reply with PONG please", "crid-restart").await);
        let turn_id = body["turn_id"].as_str().unwrap().to_string();

        process_restarts(&bot);
        heal_delivery_writes(&app).await;
        let fresh = tt::restart_app(&env).await;
        crate::reconcile::rearm_progress(&fresh).await;

        let t = turn(&fresh, &turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("in_flight", "unknown"), "不是永遠 pending");
        assert_eq!(t.auto_resend, 0, "重啟之後也保住「不自動重送」");
        assert!(t.delivered_at.is_some(), "送達時間不退回建立時間：閒置 watchdog 不會把剛送出的當成老的");
        let out = prompt(&fresh, &bot, "Reply with PONG please", "crid-restart").await.expect("同一個 request id 重問");
        assert_eq!((out.turn_id.as_str(), out.delivery.as_str()), (turn_id.as_str(), "unknown"));
        assert_eq!(typed(&env), 1, "重啟之後也不重送");
    }
    /// herdr 明確回 `agent_blocked`（一個字都沒進去），收成 failed 那一句卻寫不進去：不能回普通的 `delivery=failed`
    /// （DB 還是 in_flight＋pending）。DB 好了就收成 failed＋說明，同一個 request id 重問拿到 `failed`，不會再送。
    #[tokio::test]
    async fn an_agent_blocked_refusal_whose_fail_write_is_lost_is_not_a_plain_failed() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, _conv, run) = idle_bot(&env, "codex").await;
        sqlx::query("UPDATE runs SET pane_typed=0 WHERE id=?").bind(&run).execute(app.db()).await.unwrap();
        env.herdr.set_agent("api-bot", "pane-api", true);
        env.herdr.fail_next("agent.prompt", tt::Fault::RefuseWith("agent_blocked"));
        lose_delivery_writes(&app).await;

        let body = uncommitted(prompt(&app, &bot, "跑一下測試", "crid-blocked").await);
        assert_eq!(body["delivery"], "failed", "{body}");
        assert_eq!(body["sent"], false, "herdr 拒收：一個字都沒進去：{body}");
        let turn_id = body["turn_id"].as_str().unwrap().to_string();
        assert_eq!(turn(&app, &turn_id).await.status, "in_flight", "寫不進去就是還沒收");

        heal_delivery_writes(&app).await;
        let out = prompt(&app, &bot, "跑一下測試", "crid-blocked").await.expect("DB 好了");
        assert_eq!((out.turn_id.as_str(), out.delivery.as_str()), (turn_id.as_str(), "failed"));
        let t = turn(&app, &turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("failed", "failed"));
        let notes: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'")
            .bind(&turn_id)
            .fetch_all(app.db())
            .await
            .unwrap();
        assert!(notes.len() == 1 && notes[0].contains("agent_blocked"), "{notes:?}");
        assert_eq!(env.herdr.calls_to("agent.prompt").len(), 1, "不再送");
    }
    /// #75 重開：開機恢復讀不到時會在背景重試，重試那時這個行程可能正欠著某一筆的送達結果（DB 裡還是 in_flight＋pending）。
    /// 那一筆不是重啟前送到一半的孤兒：先結清這個行程自己的帳，不能被收成 `unknown`、還掛上 poller。
    #[tokio::test]
    async fn the_startup_recovery_settles_this_processs_owed_result_instead_of_taking_it_for_an_orphan() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, _conv, run) = idle_bot(&env, "codex").await;
        sqlx::query("UPDATE runs SET pane_typed=0 WHERE id=?").bind(&run).execute(app.db()).await.unwrap();
        env.herdr.set_agent("api-bot", "pane-api", true);
        env.herdr.fail_next("agent.prompt", tt::Fault::RefuseWith("agent_blocked"));
        lose_delivery_writes(&app).await;
        let body = uncommitted(prompt(&app, &bot, "跑一下測試", "crid-recovery").await);
        let turn_id = body["turn_id"].as_str().unwrap().to_string();

        // 開機恢復（的背景重試）這時走到這個 run：欠著的那一句還寫不進去，就先不碰。
        crate::reconcile::rearm_progress(&app).await;
        let t = turn(&app, &turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("in_flight", "pending"));

        heal_delivery_writes(&app).await;
        let _ = crate::testing::eventually!(turn(&app, &turn_id).await.status != "in_flight");
        let t = turn(&app, &turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("failed", "failed"), "結清的是這個行程記下的「一個字都沒進去」，不是 unknown");
        assert!(!app.progress_pollers.lock().await.contains_key(&run), "收掉的回合不掛 poller");
        assert_eq!(env.herdr.calls_to("agent.prompt").len(), 1, "不再送");
    }
    /// 收成 failed 那一句欠著的時候，那一筆被別的路（run 結束、watchdog）先收掉了，而那條路不動 `delivery`：補的時候
    /// 回合照它收的樣子、不補說明，但送達要補成這一次看到的 `failed`——不能留一筆已經結束、送達卻永遠 `pending` 的回合
    /// （同一個 request id 重問會拿到 `pending`）。
    #[tokio::test]
    async fn an_owed_refusal_on_a_turn_another_path_closed_still_records_that_nothing_was_sent() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, _conv, run) = idle_bot(&env, "codex").await;
        sqlx::query("UPDATE runs SET pane_typed=0 WHERE id=?").bind(&run).execute(app.db()).await.unwrap();
        env.herdr.set_agent("api-bot", "pane-api", true);
        env.herdr.fail_next("agent.prompt", tt::Fault::RefuseWith("agent_blocked"));
        lose_delivery_writes(&app).await;
        let body = uncommitted(prompt(&app, &bot, "跑一下測試", "crid-closed-elsewhere").await);
        let turn_id = body["turn_id"].as_str().unwrap().to_string();
        heal_delivery_writes(&app).await;

        crate::lifecycle::turn_controller::fail(app.db(), &turn_id, crate::lifecycle::turn_controller::DeliveryOnFail::Keep, "run ended").await.unwrap();
        settle(&app, &bot).await.expect("補上");
        let t = turn(&app, &turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("failed", "failed"), "一個字都沒進去：送達不留 pending");
        assert_eq!(owed_answer(&turn_id), None);
        let out = prompt(&app, &bot, "跑一下測試", "crid-closed-elsewhere").await.expect("同一個 request id");
        assert_eq!(out.delivery, "failed");
    }
    /// 一顆閒著的 bot 與一筆排著的 prompt（掛著 AGM 的交辦，delivered/queued）。
    async fn queued(env: &tt::Env, kind: &str, text: &str) -> (String, String, String) {
        let app = &env.app;
        let (bot, conv, _run) = idle_bot(env, kind).await;
        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, origin, status, delivery, client_request_id, prompt_text, queue_class, created_at)
             VALUES (?,?,'web','queued','pending','crid-q',?,'relay',?)",
        )
        .bind(&turn_id)
        .bind(&conv)
        .bind(text)
        .bind(db::now())
        .execute(app.db())
        .await
        .unwrap();
        crate::supervisor::store::get_or_init(app.db()).await.unwrap();
        let a = crate::supervisor::store::insert_assignment(app.db(), None, &bot, "crid-q", text, &[], None, true).await.unwrap();
        crate::supervisor::store::mark_delivered(app.db(), &a.id, &turn_id, "queued").await.unwrap();
        (bot, turn_id, a.id)
    }
    /// #149 驗收四（排隊那條）：flush 打過字、證不明，送達結果寫不進去——那一筆不能停在 in_flight＋pending 沒人管，
    /// 也不能留著可以重送的預設。交辦照舊是 delivered/queued（回合真的在跑），不被當成別的狀態；DB 好了就補上，
    /// 送達時間是 flush 送出的那一刻。
    #[tokio::test]
    async fn a_queued_flush_whose_delivery_write_is_lost_is_settled_not_forgotten() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, turn_id, a) = queued(&env, "grok", "第一行\n第二行").await;
        env.herdr.live_pane("pane-api", tt::LivePane { width: Some(120), boxed: true, ..Default::default() });
        lose_delivery_writes(&app).await;

        let flushed = flush_queued_locked(&app, &bot).await;
        let sent_by = db::now();
        assert!(flushed.is_err(), "送達結果沒寫成：flush 不回普通的成功");
        let t = turn(&app, &turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str(), t.auto_resend), ("in_flight", "pending", 0));
        assert_eq!(typed(&env), 1);
        let row = crate::supervisor::store::assignment(app.db(), &a).await.unwrap().unwrap();
        assert_eq!((row.status.as_str(), row.delivery.as_deref()), ("delivered", Some("queued")), "交辦不因為這一下變成別的狀態");

        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        heal_delivery_writes(&app).await;
        // 下一則 prompt 先把欠著的補上（回合真的還在跑，所以它自己照舊 409）。
        let _ = prompt(&app, &bot, "下一句", "crid-next").await;
        let t = turn(&app, &turn_id).await;
        assert_eq!((t.delivery.as_str(), t.delivery_verified, t.auto_resend), ("ok", 0, 0));
        assert!(t.delivered_at.as_deref().is_some_and(|at| at <= sent_by.as_str()), "送達時間是 flush 送出的那一刻：{:?}", t.delivered_at);
        assert_eq!(typed(&env), 1, "不重送");
    }
    /// 排隊那條的 `agent_blocked`：herdr 拒收、收成 failed 那一句寫不進去——以前那一筆就永遠 in_flight＋pending
    /// （擋住佇列、佔住維護窗口的送達臨界區、交辦停在 delivered）。DB 好了要收成 failed。
    #[tokio::test]
    async fn a_queued_prompt_refused_as_blocked_while_the_db_is_down_does_not_stay_in_flight() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, turn_id, _a) = queued(&env, "claude", "跑一下測試").await;
        let run = db::active_run(app.db(), &bot).await.unwrap().unwrap().id;
        sqlx::query("UPDATE runs SET pane_typed=0 WHERE id=?").bind(&run).execute(app.db()).await.unwrap();
        env.herdr.set_agent("api-bot", "pane-api", true);
        env.herdr.fail_next("agent.prompt", tt::Fault::RefuseWith("agent_blocked"));
        lose_delivery_writes(&app).await;

        assert!(flush_queued_locked(&app, &bot).await.is_err(), "收不成 failed：flush 不回普通的成功");
        assert_eq!(turn(&app, &turn_id).await.status, "in_flight");

        heal_delivery_writes(&app).await;
        let _ = prompt(&app, &bot, "下一句", "crid-next").await;
        let t = turn(&app, &turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("failed", "failed"), "DB 好了就收掉，不留永久 in_flight");
        let resent = env.herdr.calls_to("agent.prompt").iter().filter(|p| p["text"] == "跑一下測試").count();
        assert_eq!(resent, 1, "被拒收的那一則不再送（「下一句」照常送出）");
    }
    /// #149 驗收四（AGM 派工）：送達結果沒寫成的 503 不是「沒送出」——交辦不能被記成送達（DB 還是 pending），
    /// 也不能花掉重試、最後被判 dispatch_failed（工作其實在跑）。留在 queued 等一下再問同一個 crid，拿到寫好的結果才記。
    #[tokio::test]
    async fn an_assignment_whose_delivery_write_was_lost_is_held_not_failed() {
        use crate::supervisor::store;
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, _conv, _run) = idle_bot(&env, "claude").await;
        env.herdr.live_pane("pane-api", tt::LivePane { width: Some(120), ..Default::default() });
        let mgr = tt::claude_bot(&app, &env.project_id, "owed-mgr").await;
        store::get_or_init(app.db()).await.unwrap();
        store::set_env(app.db(), &mgr.id, &env.project_id, "/tmp").await.unwrap();
        let a = store::insert_assignment(app.db(), None, &bot, "crid-assign", "Reply with PONG please", &[], None, true).await.unwrap();
        lose_delivery_writes(&app).await;

        crate::supervisor::controller::dispatch(&app, &a.id).await;
        let row = store::assignment(app.db(), &a.id).await.unwrap().unwrap();
        assert_eq!(row.status, "queued", "DB 還是 pending：不記成送達 {:?}", row.delivery);
        assert_eq!(row.attempts, 0, "不花重試額度");
        assert!(row.error.as_deref().is_some_and(|e| e.contains("delivery_state_uncommitted")), "{:?}", row.error);

        // 重試額度早就用完的那種也一樣：不判 dispatch_failed。
        sqlx::query("UPDATE supervisor_assignments SET attempts=5, next_attempt_at=NULL WHERE id=?").bind(&a.id).execute(app.db()).await.unwrap();
        crate::supervisor::controller::dispatch(&app, &a.id).await;
        assert_eq!(store::assignment(app.db(), &a.id).await.unwrap().unwrap().status, "queued", "工作可能在跑：不判失敗");
        assert_eq!(typed(&env), 1);

        heal_delivery_writes(&app).await;
        sqlx::query("UPDATE supervisor_assignments SET next_attempt_at=NULL WHERE id=?").bind(&a.id).execute(app.db()).await.unwrap();
        crate::supervisor::controller::dispatch(&app, &a.id).await;
        let row = store::assignment(app.db(), &a.id).await.unwrap().unwrap();
        assert_eq!((row.status.as_str(), row.delivery.as_deref()), ("delivered", Some("ok")));
        assert_eq!(turn(&app, row.turn_id.as_deref().unwrap()).await.delivery, "ok", "交辦與 DB 說的是同一件事");
        assert_eq!(typed(&env), 1, "從頭到尾只送一次");
    }
    /// 沒有 hook、也沒有下一則 prompt：定時重試自己把欠著的補上（DB 還壞著的那一輪帳留著），送達時間是送出的那一刻。
    #[tokio::test]
    async fn the_timed_retry_alone_settles_an_owed_delivery_with_its_original_time() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, _conv, _run) = idle_bot(&env, "claude").await;
        env.herdr.live_pane("pane-api", tt::LivePane { width: Some(120), ..Default::default() });
        lose_delivery_writes(&app).await;
        let body = uncommitted(prompt(&app, &bot, "Reply with PONG please", "crid-timer").await);
        let turn_id = body["turn_id"].as_str().unwrap().to_string();
        let sent_by = db::now();

        settle(&app, &bot).await.expect_err("DB 還沒好：這一輪補不上");
        assert_eq!(owed_answer(&turn_id), Some("ok"), "帳留著");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        heal_delivery_writes(&app).await;
        settle(&app, &bot).await.expect("定時重試的下一輪");

        let t = turn(&app, &turn_id).await;
        assert_eq!((t.delivery.as_str(), t.delivery_verified, t.auto_resend), ("ok", 1, 0));
        assert!(t.delivered_at.as_deref().is_some_and(|at| at <= sent_by.as_str()), "{:?} 晚於 {sent_by}", t.delivered_at);
        assert_eq!(owed_answer(&turn_id), None, "帳結清了");
        assert_eq!(typed(&env), 1, "補寫從不再送");
    }
    /// 回合 hook 一進來先補帳（同 #147）：herdr 拒收、還沒收成 failed 的那一筆，不能被下一則回覆（使用者在終端自己打的）
    /// 認領成「答完了」。
    #[tokio::test]
    async fn a_reply_hook_settles_an_owed_refusal_before_matching_turns() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, _conv, run) = idle_bot(&env, "claude").await;
        sqlx::query("UPDATE runs SET pane_typed=0 WHERE id=?").bind(&run).execute(app.db()).await.unwrap();
        env.herdr.set_agent("api-bot", "pane-api", true);
        env.herdr.fail_next("agent.prompt", tt::Fault::RefuseWith("agent_blocked"));
        lose_delivery_writes(&app).await;
        let body = uncommitted(prompt(&app, &bot, "跑一下測試", "crid-hook").await);
        let turn_id = body["turn_id"].as_str().unwrap().to_string();
        heal_delivery_writes(&app).await;

        let stop = crate::hookrecv::HookBody {
            bot_id: bot.clone(),
            provider: "claude".into(),
            payload: json!({"hook_event_name": "Stop", "session_id": "s-typed", "prompt_id": "p-typed", "last_assistant_message": "終端裡那一句的回覆"}),
            received_at: None,
            truncated: false,
            run_id: None,
        };
        crate::hookrecv::process(&app, &stop).await.unwrap();

        let t = turn(&app, &turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("failed", "failed"), "被拒收的就是沒送出去");
        let pinned: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'")
            .bind(&turn_id)
            .fetch_one(app.db())
            .await
            .unwrap();
        assert_eq!(pinned, 0, "別句的回覆沒有掛到它身上");
    }
    /// AGM 收件匣的通知只要「綁在哪一筆、別再送一份」：欠著的當 `unknown`，拒收的照樣 `failed`，其他錯誤原樣回。
    /// #149 重開：第一次寫回失敗（欠著），同一筆接著**直接**寫成功（沒經過結清）——舊帳要跟著清掉。留著的話定時重試
    /// 之後會把舊的那一份再寫一次：這之間別人寫進去的較新結果被蓋回舊的，`delivered_at` 也不是最早送出的那一刻。
    #[tokio::test]
    async fn a_later_write_that_lands_clears_the_older_debt_for_the_same_turn() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, conv, run) = idle_bot(&env, "claude").await;
        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, client_request_id, created_at, auto_resend)
             VALUES (?,?,?,'web','in_flight','pending','crid-twice',?,0)",
        )
        .bind(&turn_id)
        .bind(&conv)
        .bind(&run)
        .bind(db::now())
        .execute(app.db())
        .await
        .unwrap();
        let sent_at = db::now();
        lose_delivery_writes(&app).await;
        delivered_at(&app, &bot, &turn_id, Delivered::Submitted.record().unwrap(), &sent_at).await.expect_err("DB 壞著");
        assert_eq!(owed_answer(&turn_id), Some("ok"), "記成欠著");

        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        heal_delivery_writes(&app).await;
        delivered(&app, &bot, &turn_id, Delivered::Submitted.record().unwrap()).await.expect("同一筆直接寫成了");
        assert_eq!(owed_answer(&turn_id), None, "寫成了就不再欠：舊帳跟著清掉");
        assert_eq!(turn(&app, &turn_id).await.delivered_at.as_deref(), Some(sent_at.as_str()), "送達時間是最早送出的那一刻，不是後來寫的時候");

        // 之後別的路寫進較新的證據（重送證不明，poller 改記 unknown）：定時重試不能把它蓋回舊的。
        sqlx::query("UPDATE turns SET delivery='unknown', delivery_verified=0 WHERE id=?").bind(&turn_id).execute(app.db()).await.unwrap();
        settle(&app, &bot).await.expect("定時重試");
        let t = turn(&app, &turn_id).await;
        assert_eq!((t.delivery.as_str(), t.delivery_verified), ("unknown", 0), "舊帳不會把較新的結果降回去");
    }
    /// #903：認領後放回佇列那一句寫不進去而記成欠著，等補寫的時候，同一段對話同 lane 的佇列名額
    /// 已經被後來的那則佔走——以前每次補寫都撞唯一鍵、帳永遠欠著，T1 永遠 in_flight＋pending 擋住後面所有的回合。
    /// 現在收成 failed＋說明（一個字都沒送出），帳清掉，後來那則照常被認領。
    #[tokio::test]
    async fn a_put_back_whose_queue_slot_was_taken_closes_the_turn_as_failed_instead_of_retrying_forever() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, t1, _a) = queued(&env, "claude", "P1").await;
        // 放回佇列（in_flight → queued）那一句寫不進去：認領照常，放回去失敗。
        sqlx::query(
            "CREATE TRIGGER refuse_requeue BEFORE UPDATE OF status ON turns WHEN OLD.status='in_flight' AND NEW.status='queued' \
             BEGIN SELECT RAISE(ABORT,'injected'); END",
        )
        .execute(app.db())
        .await
        .unwrap();
        // 畫面沒準備好（停在登入選單）：flush 認領後放棄、要放回佇列。
        env.herdr.set_screen("pane-api", "Select login method:\n❯ 1. Claude account with subscription\n  2. Anthropic Console account\n");

        assert!(flush_queued_locked(&app, &bot).await.is_err(), "放不回去：flush 不回普通的成功");
        let t = turn(&app, &t1).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("in_flight", "pending"));
        assert_eq!(owed(&bot).len(), 1, "記成欠著");

        // T1 還欠著的時候，後來的 P2 排進佇列，佔走名額。
        let p2 = prompt_relayed_queueable(&app, &bot, "P2", "crid-p2", Some(crate::agent_relay::DAEMON_SENDER), crate::lc_error::QueueClass::Relay).await.expect("P2 排進佇列");
        assert_eq!(turn(&app, &p2.turn_id).await.status, "queued");

        sqlx::query("DROP TRIGGER refuse_requeue").execute(app.db()).await.unwrap();
        settle_locked(&app, &bot).await.expect("名額被佔走：收成 failed，不是一直補不進去");

        let t = turn(&app, &t1).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("failed", "failed"));
        let notes: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'").bind(&t1).fetch_all(app.db()).await.unwrap();
        assert!(notes.iter().any(|n| n.contains("名額已被後來的那則佔走")), "{notes:?}");
        assert!(owed(&bot).is_empty(), "帳清掉");
        assert_eq!(typed(&env), 0, "從頭到尾沒送過字");

        // 擋住佇列的東西沒了：P2 被認領（要嘛已經離開 queued，要嘛至少算了一次放回的重試）。
        flush_queued_locked(&app, &bot).await.ok();
        let p = turn(&app, &p2.turn_id).await;
        assert!(p.status != "queued" || p.flush_retries > 0, "P2 被認領了：{:?}", (p.status, p.flush_retries));
    }
}
