#![allow(unused_imports)]
//! #1862 L2-1：從 am-lifecycle 搬來、需要真 daemon App／DB 的測試。

mod tests {
    use crate::lifecycle_tests::prelude::*;
    use crate::lifecycle::start_send::test_seams::*;
    use crate::lifecycle::start_send::test_seams::StartingGuard as Starting;
    // parent-uses
    use crate::lifecycle::*;
    use crate::state::App;
    use crate::lifecycle::send_now::ports::AttachTxPort;
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    use crate::lifecycle::start_send::*;
    use crate::testing as tt;
    const TEXT: &str = "起來後幫我跑一次測試";
    #[tokio::test]
    async fn native_api_start_send_refuses_before_write_and_preserves_waiting_debt() {
        let e = tt::env().await;
        let api = tt::claude_bot(&e.app, &e.project_id, "native-start-send").await;
        sqlx::query("UPDATE bots SET kind=? WHERE id=?").bind(am_config::CLAUDE_API_KIND).bind(&api.id).execute(&e.app.db).await.unwrap();
        let crid = db::ulid();
        let out = prompt_starting(&e.app, &api.id, TEXT, &crid, &[], RelaySrc::default()).await;
        assert!(matches!(out, Err(LcError::Bad(ref message)) if message == "api_native_prompt_unsupported"));
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM turns WHERE client_request_id=?").bind(&crid).fetch_one(&e.app.db).await.unwrap();
        assert_eq!(n, 0);
        let conv = db::conversation_id(&e.app.db, &api.id).await.unwrap();
        let turn = db::ulid();
        sqlx::query("INSERT INTO turns(id,conversation_id,origin,status,delivery,awaits_start,created_at) VALUES(?,?,'web','queued','pending',1,?)")
            .bind(&turn).bind(&conv).bind(db::now()).execute(&e.app.db).await.unwrap();
        start_for_waiting(&e.app, &api.id).await;
        assert_eq!(resume_after_boot(&e.app, LOCAL_HOST).await, 0);
        let row: (String,i64) = sqlx::query_as("SELECT status,awaits_start FROM turns WHERE id=?").bind(&turn).fetch_one(&e.app.db).await.unwrap();
        assert_eq!(row, ("queued".into(),1));
        assert!(db::active_run(&e.app.db, &api.id).await.unwrap().is_none());
        let missing = db::ulid();
        assert!(prompt_starting(&e.app, &missing, TEXT, &db::ulid(), &[], RelaySrc::default()).await.is_err());
        let cli = tt::claude_bot(&e.app, &e.project_id, "native-start-send-cli").await;
        let accepted = prompt_starting(&e.app, &cli.id, TEXT, &db::ulid(), &[], RelaySrc::default()).await.unwrap();
        assert_eq!(accepted.delivery, "queued");
    }
    #[tokio::test]
    async fn native_start_send_bot_read_error_refuses_before_writing() {
        let e = tt::env().await;
        let bot = tt::claude_bot(&e.app, &e.project_id, "native-start-send-fault").await;
        let crid = db::ulid();
        sqlx::query("ALTER TABLE bots RENAME TO synthetic_unreadable_bots").execute(&e.app.db).await.unwrap();
        let result = prompt_starting(&e.app, &bot.id, TEXT, &crid, &[], RelaySrc::default()).await;
        let turns: i64 = sqlx::query_scalar("SELECT count(*) FROM turns WHERE client_request_id=?").bind(&crid).fetch_one(&e.app.db).await.unwrap();
        assert!(result.is_err()); assert_eq!(turns, 0); assert_eq!(started(&e), 0);
    }
    #[tokio::test]
    async fn a_delayed_kick_after_withdrawal_does_not_restart_the_stopped_bot() {
        let e = tt::env().await;
        let bot = tt::claude_bot(&e.app, &e.project_id, "withdraw-before-kick").await;
        let out = prompt_starting(&e.app, &bot.id, TEXT, "withdraw-before-kick", &[], RelaySrc::default()).await.unwrap();
        revoke_queued_turn(&e.app, &out.turn_id, "使用者已撤回").await.unwrap();
        start_for_waiting(&e.app, &bot.id).await;
        assert_eq!(started(&e), 0);
        assert!(db::active_run(&e.app.db, &bot.id).await.unwrap().is_none());
    }
    #[tokio::test]
    async fn a_disabled_schedule_does_not_wake_its_bot_on_a_delayed_kick_or_boot() {
        let e = tt::env().await;
        let bot = tt::claude_bot(&e.app, &e.project_id, "disabled-before-kick").await;
        let out = prompt_starting(&e.app, &bot.id, TEXT, "disabled-before-kick", &[], RelaySrc::default()).await.unwrap();
        let id = db::ulid();
        let now = db::now();
        sqlx::query("INSERT INTO schedules (id,project_id,bot_id,name,prompt,mode,tz,spec_json,enabled,created_at,updated_at) VALUES (?,?,?,'schedule',?,'once','UTC','{}',0,?,?)")
            .bind(&id).bind(&e.project_id).bind(&bot.id).bind(TEXT).bind(&now).bind(&now).execute(&e.app.db).await.unwrap();
        sqlx::query("INSERT INTO schedule_occurrences (id,schedule_id,source,scheduled_for,generation,bot_id,prompt,status,turn_id,created_at,updated_at) VALUES (?,?,'scheduled',?,1,?,?,'queued',?,?,?)")
            .bind(db::ulid()).bind(&id).bind(&now).bind(&bot.id).bind(TEXT).bind(&out.turn_id).bind(&now).bind(&now).execute(&e.app.db).await.unwrap();
        start_for_waiting(&e.app, &bot.id).await;
        assert_eq!(resume_after_boot(&e.app, LOCAL_HOST).await, 0);
        assert_eq!(started(&e), 0);
        assert!(db::active_run(&e.app.db, &bot.id).await.unwrap().is_none());
    }
    async fn turn_of(app: &Arc<App>, crid: &str) -> db::Turn {
        sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE client_request_id=?").bind(crid).fetch_one(&app.db).await.unwrap()
    }
    async fn count(app: &Arc<App>, sql: &str, bind: &str) -> i64 {
        sqlx::query_scalar(sql).bind(bind).fetch_one(&app.db).await.unwrap()
    }
    fn started(e: &tt::Env) -> usize {
        e.herdr.methods().iter().filter(|m| *m == "agent.start").count()
    }
    /// 送出的那一刻就在 DB 裡（turn＋訊息同一個交易），啟動是之後的事——在那之前 daemon 重啟、定時掃描跑過，都還在。
    /// 同一個請求再送一次（重整後重送、網路重試）回同一筆，不會多一則。
    #[tokio::test]
    async fn a_send_to_a_stopped_bot_is_on_disk_before_any_start_and_survives_a_restart() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "stopped").await;
        let out = prompt_starting(&app, &bot.id, TEXT, "crid-1", &[], RelaySrc::default()).await.unwrap();
        assert_eq!(out.delivery, "queued");
        let t = turn_of(&app, "crid-1").await;
        assert_eq!((t.status.as_str(), t.run_id.as_deref(), t.awaits_start, t.prompt_text.as_deref()), ("queued", None, 1, Some(TEXT)));
        assert_eq!(count(&app, "SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='user'", &t.id).await, 1);
        assert_eq!(started(&e), 0, "請求裡不等啟動（啟動是背景的副作用）");

        let again = prompt_starting(&app, &bot.id, TEXT, "crid-1", &[], RelaySrc::default()).await.unwrap();
        assert_eq!((again.turn_id.as_str(), again.delivery.as_str()), (t.id.as_str(), "queued"), "同一個請求回同一筆");
        assert_eq!(count(&app, "SELECT COUNT(*) FROM messages WHERE role='user' AND content=?", TEXT).await, 1, "沒有多一則");

        // daemon 重啟：記憶體全空，定時的撤孤兒掃描照常跑——這一則不是孤兒。
        let fresh = tt::restart_app(&e).await;
        crate::lifecycle::revoke_all_orphaned_queued_turns(&fresh).await;
        let t = turn_of(&fresh, "crid-1").await;
        assert_eq!((t.status.as_str(), t.awaits_start), ("queued", 1), "還在佇列");
        assert_eq!(resume_after_boot(&fresh, LOCAL_HOST).await, 1, "開機對帳完替它再啟動一次");
        assert_eq!(resume_after_boot(&fresh, "elsewhere").await, 0, "別台主機的不歸這一輪");
    }
    /// #198 同類：開機替等著的 bot 起來時讀不到它在哪台主機，不當成本機——本機開機時把還沒對帳的遠端 bot 也起一顆，
    /// 就是同一顆 bot 兩個 agent。讀不到的先不起，那台自己的開機（或稍後重看）再起。
    #[tokio::test]
    async fn a_waiting_bot_whose_host_cannot_be_read_is_not_started_as_local() {
        let e = tt::env().await;
        let app = e.app.clone();
        let far = db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, '/r/p', 'r', 'far', ?)").bind(&far).bind(db::now()).execute(&app.db).await.unwrap();
        let bot = tt::claude_bot(&app, &far, "remote-waiting").await;
        prompt_starting(&app, &bot.id, TEXT, "crid-far", &[], RelaySrc::default()).await.unwrap();

        sqlx::query("ALTER TABLE projects RENAME TO projects_unreadable").execute(&app.db).await.unwrap();
        assert_eq!(resume_after_boot(&app, LOCAL_HOST).await, 0, "讀不到主機：不當成本機的替它起");
        sqlx::query("ALTER TABLE projects_unreadable RENAME TO projects").execute(&app.db).await.unwrap();
        assert_eq!(resume_after_boot(&app, LOCAL_HOST).await, 0, "它不在本機");
        assert_eq!(resume_after_boot(&app, "far").await, 1, "那台自己的開機替它起");
    }
    /// 啟動失敗不丟訊息：留在佇列、原因寫在 turn 上（UI 顯示「沒能啟動」＋重新啟動／取消）。每分鐘的撤孤兒掃描
    /// 不把它撤掉，也不把真正的原因蓋成「run 已經結束」。取消＝撤回、寫明理由；已經撤掉的再取消回 409。
    #[tokio::test]
    async fn a_failed_start_keeps_the_send_with_its_reason_until_the_user_cancels_it() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "no-identity").await;
        // 這台沒有這個身份：`start_bot` 在碰任何 pane 之前就 409。
        sqlx::query("UPDATE bots SET identity='ghost' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        prompt_starting(&app, &bot.id, TEXT, "crid-fail", &[], RelaySrc::default()).await.unwrap();
        {
            // 正在替它起（run 還沒建出來）：不拿 bot 鎖的定時掃描剛好跑過，不能把原因寫成「run 已經結束」。
            let _mark = Starting::begin(&bot.id).expect("沒有別人在起");
            crate::lifecycle::revoke_all_orphaned_queued_turns(&app).await;
            assert_eq!(turn_of(&app, "crid-fail").await.start_error, None, "起到一半不算沒能啟動");
        }
        start_for_waiting(&app, &bot.id).await;
        let t = turn_of(&app, "crid-fail").await;
        assert_eq!(t.status, "queued", "起不來也不丟");
        let why = t.start_error.clone().expect("原因寫在 turn 上");
        assert!(why.contains("ghost"), "{why}");
        assert!(!in_progress(&bot.id));

        crate::lifecycle::revoke_all_orphaned_queued_turns(&app).await;
        let t = turn_of(&app, "crid-fail").await;
        assert_eq!((t.status.as_str(), t.start_error.as_deref()), ("queued", Some(why.as_str())), "掃描不撤、也不蓋掉原因");

        // 取消：撤回、寫明理由，這個對話的排隊名額空出來。
        withdraw_turn(&app, &t.id).await.unwrap();
        let t = turn_of(&app, "crid-fail").await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("failed", "failed"));
        assert_eq!(count(&app, "SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='system' AND content LIKE '%使用者取消%'", &t.id).await, 1);
        assert!(matches!(withdraw_turn(&app, &t.id).await, Err(LcError::Conflict(_))), "撤掉的不能再取消一次");
        assert_eq!(resume_after_boot(&app, LOCAL_HOST).await, 0, "撤掉的不再替它啟動");
    }
    /// 取消只撤「還在等 bot 起來」的那一則：已經被佇列領走（字打進去了）、或 AGM 排的派工，一律 409、原樣不動——
    /// 拿「放棄回合」頂替的話，web 會把已經送出的文字放回輸入框，再按一次就送兩次。
    #[tokio::test]
    async fn withdraw_refuses_anything_but_a_send_still_waiting_for_its_bot() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "withdraw").await;
        prompt_starting(&app, &bot.id, TEXT, "crid-w", &[], RelaySrc::default()).await.unwrap();
        let t = turn_of(&app, "crid-w").await;
        let run = tt::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE turns SET status='in_flight', run_id=? WHERE id=?").bind(&run).bind(&t.id).execute(&app.db).await.unwrap();
        assert!(matches!(withdraw_turn(&app, &t.id).await, Err(LcError::Conflict(_))), "已經送出去的撤不回來");
        assert_eq!(turn_of(&app, "crid-w").await.status, "in_flight", "原樣不動");

        let other = tt::claude_bot(&app, &e.project_id, "agm-queued").await;
        let conv = db::conversation_id(&app.db, &other.id).await.unwrap();
        let agm = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,'web','queued','pending','派工',?)")
            .bind(&agm)
            .bind(&conv)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        assert!(matches!(withdraw_turn(&app, &agm).await, Err(LcError::Conflict(_))), "AGM 的派工不歸這裡撤");
        assert!(matches!(withdraw_turn(&app, "no-such-turn").await, Err(LcError::NotFound(_))));
    }
    /// run 起來之後又結束（pane 不見、agent 退出）不是使用者不要了：一樣留著、記原因。使用者自己按停止才撤。
    #[tokio::test]
    async fn only_the_users_own_stop_withdraws_a_starting_send() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "run-ended").await;
        let run = tt::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE runs SET state='starting' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
        let out = prompt_starting(&app, &bot.id, TEXT, "crid-run", &[], RelaySrc::default()).await.unwrap();
        assert_eq!(out.delivery, "queued", "run 還在 starting：一樣先收下（有人在起它，不必再踢一次）");

        crate::lifecycle::mark_run_exited(&app, &run, "pane 不見了").await;
        let t = turn_of(&app, "crid-run").await;
        assert_eq!(t.status, "queued", "run 結束不撤");
        assert!(t.start_error.as_deref().is_some_and(|w| w.contains("pane 不見了")), "{:?}", t.start_error);

        let run2 = tt::fake_run(&app, &bot.id).await;
        crate::lifecycle::stop_bot(&app, &bot.id).await.unwrap();
        let t = turn_of(&app, "crid-run").await;
        assert_eq!(t.status, "failed", "使用者自己按停止：撤回");
        assert_eq!(count(&app, "SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='system' AND content LIKE 'bot 已被停止%'", &t.id).await, 1);
        let _ = run2;
    }
    /// #159（同一類）→ #199：使用者按停止時，等 bot 起來的那一則撤不掉（DB 一時寫不進去）——以前只記一行 error，之後 bot 重新
    /// 起來、閒下來，flush 就把使用者已經不要的那一則送進去；#159 改成記在記憶體的欠帳，daemon 重啟就沒了。現在撤回跟記
    /// `stopping` 同一個交易：撤不掉就連停都沒停（502，agent 照跑，外面一步都沒動）；再按一次、寫得進去，兩個一起成立，
    /// 之後 bot 重新起來也永遠不送。
    #[tokio::test]
    async fn a_stop_whose_withdrawal_could_not_be_written_never_lets_the_send_go_out() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "stop-lost").await;
        let run = tt::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE runs SET state='starting' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
        prompt_starting(&app, &bot.id, TEXT, "crid-stop-lost", &[], RelaySrc::default()).await.unwrap();
        let t = turn_of(&app, "crid-stop-lost").await;
        sqlx::query("UPDATE runs SET state='running' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
        sqlx::query(&format!(
            "CREATE TRIGGER lost_withdraw BEFORE UPDATE OF status ON turns WHEN OLD.id = '{}' AND NEW.status = 'failed' BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END",
            t.id
        ))
        .execute(&app.db)
        .await
        .unwrap();
        let n = e.herdr.methods().len();
        let err = crate::lifecycle::stop_bot(&app, &bot.id).await.expect_err("撤不掉：連停都不算開始");
        assert!(matches!(err, LcError::Upstream(_)), "{err:?}");
        assert!(!e.herdr.methods().iter().skip(n).any(|m| m == "agent.send_keys" || m == "pane.close"), "外面一步都沒動");
        let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(&run).fetch_one(&app.db).await.unwrap();
        assert_eq!((state.as_str(), turn_of(&app, "crid-stop-lost").await.status.as_str()), ("running", "queued"), "兩個都沒成立");

        sqlx::query("DROP TRIGGER lost_withdraw").execute(&app.db).await.unwrap();
        crate::lifecycle::stop_bot(&app, &bot.id).await.unwrap();
        let t = turn_of(&app, "crid-stop-lost").await;
        assert_eq!(t.status, "failed", "再按一次：撤回跟停一起成立");
        assert_eq!(count(&app, "SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='system' AND content LIKE 'bot 已被停止%'", &t.id).await, 1);

        // 使用者又把它起來了、閒著：flush 不會把那一則送出去。
        let run2 = tt::fake_run(&app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        e.herdr.live_pane(&pane, tt::LivePane { width: Some(120), ..Default::default() });
        db::set_pane_typed(&app.db, &run2).await.unwrap();
        forget_queue_retry_timer(&bot.id);
        flush_queued_locked(&app, &bot.id).await.unwrap();
        assert_eq!(e.herdr.pane(&pane).map_or(0, |p| p.transcript.iter().filter(|l| l.contains(TEXT)).count()), 0, "永遠不送");
    }
    /// 在跑的 bot：旗標沒有作用，走一般的送出（有 run、沒有 `awaits_start`）。附件跟訊息同一個交易綁上；
    /// 認不得的附件 400，什麼都沒寫。
    #[tokio::test]
    async fn a_running_bot_takes_the_normal_path_and_attachments_bind_with_the_send() {
        let e = tt::env().await;
        let app = e.app.clone();
        let running = tt::claude_bot(&app, &e.project_id, "running").await;
        tt::fake_run(&app, &running.id).await;
        // run 的 pane 真 herdr 上一定存在（#1452）：替身要認得它，讀得到 composer。
        e.herdr.know_pane(&format!("pane-{}", running.id));
        // 假的 run 沒有 transcript：一般那條路照它自己的規則回 409 `transcript_not_ready`——重點是沒有被「收下再啟動」。
        let normal = prompt_starting(&app, &running.id, "照常送", "crid-running", &[], RelaySrc::default()).await;
        assert!(
            matches!(&normal, Err(LcError::Conflict(v)) if v["reason"] == "transcript_not_ready"),
            "在跑的 bot 照一般的路：{normal:?}"
        );
        assert_eq!(count(&app, "SELECT COUNT(*) FROM turns WHERE awaits_start=1 AND client_request_id=?", "crid-running").await, 0);

        let stopped = tt::claude_bot(&app, &e.project_id, "stopped-attach").await;
        let att = db::ulid();
        sqlx::query(
            "INSERT INTO attachments (id, bot_id, name, mime, size, local_path, agent_path, host, created_at)
             VALUES (?,?,'shot.png','image/png',1,'/tmp/shot.png','/tmp/shot.png','local',?)",
        )
        .bind(&att)
        .bind(&stopped.id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let bad = prompt_starting(&app, &stopped.id, TEXT, "crid-bad", &["nope".to_string()], RelaySrc::default()).await;
        assert!(matches!(bad, Err(LcError::Bad(_))));
        assert_eq!(count(&app, "SELECT COUNT(*) FROM turns WHERE client_request_id=?", "crid-bad").await, 0, "認不得的附件：什麼都沒寫");

        let out = prompt_starting(&app, &stopped.id, TEXT, "crid-att", std::slice::from_ref(&att), RelaySrc::default()).await.unwrap();
        let bound: Option<String> = sqlx::query_scalar("SELECT message_id FROM attachments WHERE id=?").bind(&att).fetch_one(&app.db).await.unwrap();
        assert_eq!(bound.as_deref(), Some(out.message_id.as_str()), "附件綁在這一則上");
        let t = turn_of(&app, "crid-att").await;
        assert!(t.prompt_text.as_deref().is_some_and(|p| p.contains("/tmp/shot.png")), "起來後送的字帶著附件路徑");
    }
    /// issue #152（start_send 那一半）：替等著送的訊息啟動時 agent 真的起來了，`running` 卻寫不進 DB（`Uncommitted`）——
    /// 那不是「沒能啟動」：不寫 `start_error`。對帳重試把 run 收成 `running` 不會叫 flush，所以這裡要自己等到它收斂再叫；
    /// 收斂之後那一則送出去，只送一次。會真的 `start_bot`，要有 claude（遠端編譯主機沒有）。
    #[tokio::test]
    async fn a_start_that_came_up_but_could_not_record_running_still_sends_once_it_settles() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "uncommitted").await;
        prompt_starting(&app, &bot.id, TEXT, "crid-u", &[], RelaySrc::default()).await.unwrap();
        crate::lifecycle::run_state::refuse_run_state(&app, "running").await;
        start_for_waiting(&app, &bot.id).await;
        let t = turn_of(&app, "crid-u").await;
        assert_eq!((t.status.as_str(), t.start_error.as_deref()), ("queued", None), "agent 起來了：不是沒能啟動");
        let run = db::active_run(&app.db, &bot.id).await.unwrap().expect("run 停在 starting，沒被收掉");
        assert_eq!(run.state, "starting");
        assert!(watching_for_running(&run.id), "排了「收成 running 就叫 flush」");
        assert!(!flush_if_running(&app, &bot.id, &run.id).await, "還在 starting：繼續等");

        // DB 恢復，對帳照 herdr 的證據收成 running。
        crate::lifecycle::run_state::accept_run_state(&app, "running").await;
        let stuck = crate::lifecycle::run_state::Settle::Reconcile { stuck: "starting".into() };
        assert!(crate::lifecycle::run_state::settle_once(&app, &run.id, &stuck).await);
        assert!(flush_if_running(&app, &bot.id, &run.id).await, "收成 running：叫 flush，不再等");
        let pane = db::run(&app.db, &run.id).await.unwrap().unwrap().pane_id.unwrap();
        e.herdr.live_pane(&pane, tt::LivePane { width: Some(120), ..Default::default() });
        db::set_pane_typed(&app.db, &run.id).await.unwrap();
        for _ in 0..3 {
            forget_queue_retry_timer(&bot.id);
            flush_queued_locked(&app, &bot.id).await.unwrap();
        }
        let t = turn_of(&app, "crid-u").await;
        assert_eq!((t.status.as_str(), t.run_id.as_deref(), t.start_error.as_deref()), ("in_flight", Some(run.id.as_str()), None));
        let sent = e.herdr.pane(&pane).map_or(0, |p| p.transcript.iter().filter(|l| l.contains(TEXT)).count());
        assert_eq!(sent, 1, "送出，只送一次");
        forget_queue_retry_timer(&bot.id);
        stop_bot(&app, &bot.id).await.unwrap();
    }
    /// 同上，走睡著的 bot（`idle_sleep::wake` 用 `--resume` 叫醒，回的是字串錯誤）：留下一個 `starting` 的 run 就是
    /// agent 起來了、狀態沒記下，一樣不寫 `start_error`、等收斂再叫 flush。會真的 `start_bot`，要有 claude。
    #[tokio::test]
    async fn a_sleeping_bot_woken_for_a_send_that_could_not_record_running_is_not_a_failed_start() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "asleep-uncommitted").await;
        sqlx::query("INSERT INTO bot_sleeps (bot_id, idle_minutes, reason, slept_at) VALUES (?, 30, 'idle', ?)")
            .bind(&bot.id)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        prompt_starting(&app, &bot.id, TEXT, "crid-sleep", &[], RelaySrc::default()).await.unwrap();
        crate::lifecycle::run_state::refuse_run_state(&app, "running").await;
        start_for_waiting(&app, &bot.id).await;
        assert_eq!(turn_of(&app, "crid-sleep").await.start_error, None, "叫醒時 agent 起來了：不是沒能啟動");
        let run = db::active_run(&app.db, &bot.id).await.unwrap().expect("run 停在 starting");
        assert!(watching_for_running(&run.id));
        crate::lifecycle::run_state::accept_run_state(&app, "running").await;
        forget_queue_retry_timer(&bot.id);
        stop_bot(&app, &bot.id).await.unwrap();
    }
    /// 整條：沒在跑 → 收下 → daemon 重啟（啟動還沒發生）→ 開機替它啟動 → 起來、閒下來 → 佇列送出，只送一次；
    /// 多叫醒幾次、同一個請求重送、再踢一次啟動，都不會再送。會真的 `start_bot`，要有 claude（遠端編譯主機沒有）。
    #[tokio::test]
    async fn a_send_to_a_stopped_bot_goes_out_exactly_once_after_the_daemon_starts_it() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "boot-start").await;
        prompt_starting(&app, &bot.id, TEXT, "crid-e2e", &[], RelaySrc::default()).await.unwrap();

        let app = tt::restart_app(&e).await;
        assert_eq!(resume_after_boot(&app, LOCAL_HOST).await, 1);
        start_for_waiting(&app, &bot.id).await;
        let t = turn_of(&app, "crid-e2e").await;
        assert_eq!((t.status.as_str(), t.start_error.as_deref()), ("queued", None), "起來了，還沒送（等閒下來）");
        let run = db::active_run(&app.db, &bot.id).await.unwrap().expect("run 起來了");
        let pane = run.pane_id.clone().unwrap();
        e.herdr.live_pane(&pane, tt::LivePane { width: Some(120), ..Default::default() });
        db::set_pane_typed(&app.db, &run.id).await.unwrap();
        let sent = || e.herdr.pane(&pane).map_or(0, |p| p.transcript.iter().filter(|l| l.contains(TEXT)).count());

        for _ in 0..3 {
            forget_queue_retry_timer(&bot.id);
            flush_queued_locked(&app, &bot.id).await.unwrap();
        }
        let t = turn_of(&app, "crid-e2e").await;
        assert_eq!((t.status.as_str(), t.run_id.as_deref()), ("in_flight", Some(run.id.as_str())));
        assert_eq!(sent(), 1, "送出，只送一次");

        let again = prompt_starting(&app, &bot.id, TEXT, "crid-e2e", &[], RelaySrc::default()).await.unwrap();
        assert_eq!(again.turn_id, t.id, "同一個請求重送：回同一筆，不再開一則");
        start_for_waiting(&app, &bot.id).await;
        flush_queued_locked(&app, &bot.id).await.unwrap();
        assert_eq!(sent(), 1, "再踢一次啟動、再叫醒 flush：不會再送");
        forget_queue_retry_timer(&bot.id);
        stop_bot(&app, &bot.id).await.unwrap();
    }
    /// #708：移交出去的專案不收 prompt、也不排隊（同 `prompt_inner`）。`start_if_stopped` 這條入口也要擋：
    /// 被拒的那次不留列，收回移交之後同一個請求照常收。
    #[tokio::test]
    async fn a_prompt_for_a_stopped_bot_in_a_handed_off_project_is_refused_not_queued() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "handed").await;
        sqlx::query("UPDATE projects SET handed_off_to = 'agm-host' WHERE id = ?").bind(&e.project_id).execute(&app.db).await.unwrap();

        let err = prompt_starting(&app, &bot.id, "還在嗎", "crid-handoff-1", &[], RelaySrc::trusted(None)).await.unwrap_err();
        match err {
            LcError::Conflict(v) => assert_eq!(v["reason"], "handed_off"),
            other => panic!("預期 409 handed_off，實際 {other:?}"),
        }
        assert_eq!(count(&app, "SELECT COUNT(*) FROM turns t JOIN conversations c ON c.id = t.conversation_id WHERE c.bot_id = ?", &bot.id).await, 0);
        assert_eq!(count(&app, "SELECT COUNT(*) FROM messages m JOIN conversations c ON c.id = m.conversation_id WHERE c.bot_id = ?", &bot.id).await, 0);
        assert_eq!(started(&e), 0, "移交中不替它啟動");

        sqlx::query("UPDATE projects SET handed_off_to = NULL WHERE id = ?").bind(&e.project_id).execute(&app.db).await.unwrap();
        let out = prompt_starting(&app, &bot.id, "還在嗎", "crid-handoff-1", &[], RelaySrc::trusted(None)).await.unwrap();
        assert_eq!(out.delivery, "queued", "收回之後同一個請求照常收，被拒的那次沒有撞冪等");
        forget_queue_retry_timer(&bot.id);
        stop_bot(&app, &bot.id).await.unwrap();
    }
}
