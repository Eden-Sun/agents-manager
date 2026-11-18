#![allow(unused_imports)]
//! #1862 L2-1：從 am-lifecycle 搬來、需要真 daemon App／DB 的測試。

mod tests {
    use crate::lifecycle_tests::prelude::*;
    // parent-uses
    use crate::lifecycle::*;

    use crate::lifecycle::resume_gate::*;
    use crate::testing as tt;
    fn at(iso: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(iso).unwrap().with_timezone(&chrono::Utc)
    }
    async fn bot_and_run(kind: &str, inject_hooks: i64, resume: Option<&str>, started_at: &str) -> (tt::Env, db::Bot, db::Run) {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "gate").await;
        sqlx::query("UPDATE bots SET kind = ?, inject_hooks = ? WHERE id = ?")
            .bind(kind)
            .bind(inject_hooks)
            .bind(&bot.id)
            .execute(&env.app.db)
            .await
            .unwrap();
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at, resume_session_id)
             VALUES (?,?,'running','idle','ws-1','pane-1','agent','test',?,?)",
        )
        .bind(&run_id)
        .bind(&bot.id)
        .bind(started_at)
        .bind(resume)
        .execute(&env.app.db)
        .await
        .unwrap();
        let bot = db::bot(&env.app.db, &bot.id).await.unwrap().unwrap();
        let run = db::active_run(&env.app.db, &bot.id).await.unwrap().unwrap();
        (env, bot, run)
    }
    /// 誰要等、等多久、什麼時候算到期——整張規則表。
    #[tokio::test]
    async fn only_an_unconfirmed_claude_resume_with_hooks_waits_and_only_for_the_window() {
        let t0 = "2026-09-18T08:00:00.000Z";
        let (_e, bot, run) = bot_and_run("claude", 1, Some("s-1"), t0).await;
        let s = Duration::from_secs;
        assert_eq!(decide(&bot, &run, at("2026-09-18T08:00:30Z")), Gate::Waiting { expected: "s-1".into(), left: s(90) });
        assert_eq!(decide(&bot, &run, at("2026-09-18T08:02:00Z")), Gate::Expired { expected: "s-1".into() }, "剛好到期");
        assert_eq!(decide(&bot, &run, at("2026-09-18T09:00:00Z")), Gate::Expired { expected: "s-1".into() });

        let mut decided = run.clone();
        decided.resume_outcome = Some("verified".into());
        assert_eq!(decide(&bot, &decided, at("2026-09-18T08:00:30Z")), Gate::Open, "已經有結論");
        decided.resume_outcome = Some("mismatch".into());
        assert_eq!(decide(&bot, &decided, at("2026-09-18T08:00:30Z")), Gate::Open, "對不上也是結論：context_lost 已經說過了");

        let mut fresh = run.clone();
        fresh.resume_session_id = None;
        assert_eq!(decide(&bot, &fresh, at("2026-09-18T08:00:30Z")), Gate::Open, "沒要求接回");
        fresh.resume_session_id = Some("  ".into());
        assert_eq!(decide(&bot, &fresh, at("2026-09-18T08:00:30Z")), Gate::Open);

        let mut hookless = bot.clone();
        hookless.inject_hooks = 0;
        assert_eq!(decide(&hookless, &run, at("2026-09-18T08:00:30Z")), Gate::Open, "沒有 hook 就沒有驗證來源");
        for kind in ["codex", "grok"] {
            let mut other = bot.clone();
            other.kind = kind.into();
            assert_eq!(decide(&other, &run, at("2026-09-18T08:00:30Z")), Gate::Open, "{kind} 要等第一個回合才回報，不在這裡等");
        }
        let mut garbled = run.clone();
        garbled.started_at = "not a time".into();
        assert_eq!(decide(&bot, &garbled, at("2026-09-18T08:00:30Z")), Gate::Expired { expected: "s-1".into() }, "讀不懂就走退路，不要永遠卡住");
    }
    /// 到期是刻意的退路：記 `unverified`、留一則看得見的說明、放行——而且只做一次。
    #[tokio::test]
    async fn an_expired_wait_is_recorded_once_with_a_visible_note_and_then_released() {
        let long_ago = db::iso_at(chrono::Utc::now() - chrono::Duration::seconds(600));
        let (e, bot, run) = bot_and_run("claude", 1, Some("s-late"), &long_ago).await;
        let conv = db::conversation_id(&e.app.db, &bot.id).await.unwrap();
        assert_eq!(check(&e.app, &bot, &run, &conv).await, Gate::Open);
        assert_eq!(check(&e.app, &bot, &run, &conv).await, Gate::Open, "拿舊的 run 快照再問一次");
        let (outcome, sid): (Option<String>, Option<String>) =
            sqlx::query_as("SELECT resume_outcome, resume_session_id FROM runs WHERE id=?").bind(&run.id).fetch_one(&e.app.db).await.unwrap();
        assert_eq!(outcome.as_deref(), Some("unverified"));
        assert_eq!(sid.as_deref(), Some("s-late"), "之後才到的 SessionStart 還要能比對：要接的 session 不清掉");
        let notes: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id=? AND role='system'")
            .bind(&conv)
            .fetch_all(&e.app.db)
            .await
            .unwrap();
        assert_eq!(notes.len(), 1, "只說一次：{notes:?}");
        assert!(notes[0].contains("s-late") && notes[0].contains("確認不了"), "{}", notes[0]);
    }
    /// 還在窗口內：只回 `Waiting`，什麼都不寫。
    #[tokio::test]
    async fn a_wait_inside_the_window_writes_nothing() {
        let (e, bot, run) = bot_and_run("claude", 1, Some("s-now"), &db::now()).await;
        let conv = db::conversation_id(&e.app.db, &bot.id).await.unwrap();
        assert!(matches!(check(&e.app, &bot, &run, &conv).await, Gate::Waiting { ref expected, .. } if expected == "s-now"));
        let outcome: Option<String> = sqlx::query_scalar("SELECT resume_outcome FROM runs WHERE id=?").bind(&run.id).fetch_one(&e.app.db).await.unwrap();
        assert_eq!(outcome, None);
        let notes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id=?").bind(&conv).fetch_one(&e.app.db).await.unwrap();
        assert_eq!(notes, 0);
    }
    async fn queue_one(app: &Arc<App>, bot_id: &str, text: &str) -> String {
        let conv = db::conversation_id(&app.db, bot_id).await.unwrap();
        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at)
             VALUES (?,?,'web','queued','pending',?,?)",
        )
        .bind(&turn_id)
        .bind(&conv)
        .bind(text)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        turn_id
    }
    async fn turn(app: &Arc<App>, id: &str) -> db::Turn {
        sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(id).fetch_one(&app.db).await.unwrap()
    }
    /// claude 起來時的 `SessionStart`，由這個 run 自己的行程送來。
    async fn session_start(app: &Arc<App>, bot_id: &str, run_id: &str, session: &str) {
        crate::hookrecv::process(
            app,
            &crate::hookrecv::HookBody {
                bot_id: bot_id.to_string(),
                provider: "claude".into(),
                payload: json!({"hook_event_name": "SessionStart", "session_id": session, "source": "resume"}),
                received_at: None,
                truncated: false,
                run_id: Some(run_id.to_string()),
            },
        )
        .await
        .unwrap();
    }
    /// flush 走到了閘門後面（claim 了，或送不出去被放回佇列並花了一次重試——這裡沒有真的 pane）。
    fn went_past_the_gate(t: &db::Turn) -> bool {
        t.status != "queued" || t.flush_retries > 0
    }
    /// 整條走一遍：還沒驗證時排隊的 prompt 不動（不 claim、不算重試、一個字都不打、掛好 timer），
    /// `SessionStart` 對上之後才往下送。
    #[tokio::test]
    async fn a_queued_prompt_waits_until_the_resumed_session_is_confirmed() {
        let (e, bot, run) = bot_and_run("claude", 1, Some("s-keep"), &db::now()).await;
        let app = e.app.clone();
        let queued = queue_one(&app, &bot.id, "換身份之後的第一句").await;
        forget_queue_retry_timer(&bot.id);

        flush_queued_locked(&app, &bot.id).await.unwrap();
        let t = turn(&app, &queued).await;
        assert_eq!((t.status.as_str(), t.flush_retries, t.run_id.clone()), ("queued", 0, None), "還沒驗證：不 claim、不花重試");
        assert!(!e.herdr.methods().iter().any(|m| m == "pane.send_text" || m == "agent.prompt"), "一個字都沒打");
        assert!(queue_retry_timer_armed(&bot.id), "閒著的 bot 沒有邊會叫醒它：要掛 timer 到期再來");
        assert!(queue_retry_timer_left(&bot.id).unwrap() <= VERIFY_WINDOW);

        session_start(&app, &bot.id, &run.id, "s-keep").await;
        let outcome: Option<String> = sqlx::query_scalar("SELECT resume_outcome FROM runs WHERE id=?").bind(&run.id).fetch_one(&app.db).await.unwrap();
        assert_eq!(outcome.as_deref(), Some("verified"));
        forget_queue_retry_timer(&bot.id);
        flush_queued_locked(&app, &bot.id).await.unwrap();
        assert!(went_past_the_gate(&turn(&app, &queued).await), "驗證過了就照常送");
        forget_queue_retry_timer(&bot.id);
    }
    /// CLI 默默開了新對話（`mismatch`）也是結論：先在聊天室說清楚，再放行——刻意的退路，不是永遠卡住。
    #[tokio::test]
    async fn a_mismatched_resume_says_so_first_and_then_releases_the_queue() {
        let (e, bot, run) = bot_and_run("claude", 1, Some("s-keep"), &db::now()).await;
        let app = e.app.clone();
        let queued = queue_one(&app, &bot.id, "ping").await;
        session_start(&app, &bot.id, &run.id, "s-brand-new").await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let notes: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id=? AND role='system'")
            .bind(&conv)
            .fetch_all(&app.db)
            .await
            .unwrap();
        assert!(notes.iter().any(|n| n.contains("接不回")), "{notes:?}");
        forget_queue_retry_timer(&bot.id);
        flush_queued_locked(&app, &bot.id).await.unwrap();
        assert!(went_past_the_gate(&turn(&app, &queued).await));
        let outcome: Option<String> = sqlx::query_scalar("SELECT resume_outcome FROM runs WHERE id=?").bind(&run.id).fetch_one(&app.db).await.unwrap();
        assert_eq!(outcome.as_deref(), Some("mismatch"));
        forget_queue_retry_timer(&bot.id);
    }
    /// 遠端主機：hook 不走 HTTP，是那台的 `hook.sh` 寫進 spool、daemon drain 回來再走同一支 `process`（SPEC §11.4）。
    /// 用真的 `hook.sh`（本機 `/bin/sh` 冒充遠端）產生 `SessionStart` 那一行：遠端 run 一樣要等驗證、那一行帶的
    /// run id 讓它被認成這一代、驗證之後一樣放行。
    #[tokio::test]
    async fn a_remote_run_waits_the_same_way_and_its_spooled_session_start_releases_it() {
        use std::io::Write as _;
        let (e, bot, run) = bot_and_run("claude", 1, Some("s-far"), &db::now()).await;
        let app = e.app.clone();
        let far = db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, '/home/u/far', 'far', 'far-box', ?)")
            .bind(&far)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("UPDATE bots SET project_id=? WHERE id=?").bind(&far).bind(&bot.id).execute(&app.db).await.unwrap();
        assert_eq!(db::bot_host(&app.db, &bot.id).await.unwrap(), "far-box");
        let queued = queue_one(&app, &bot.id, "遠端那顆的派工").await;
        forget_queue_retry_timer(&bot.id);
        flush_queued_locked(&app, &bot.id).await.unwrap();
        let held = turn(&app, &queued).await;
        assert_eq!((held.status.as_str(), held.flush_retries), ("queued", 0), "遠端 run 一樣要等驗證");

        // 「遠端」那台：真的 hook.sh，pane env 帶這個 run 的 AM_RUN_ID，沒有 herdr（只寫 spool）。
        let home = e.dir.join("far-home");
        std::fs::create_dir_all(&home).unwrap();
        let script = home.join("hook.sh");
        std::fs::write(&script, remote_hook_sh(crate::startup::REMOTE_ROOT)).unwrap();
        let mut ch = std::process::Command::new("/bin/sh")
            .arg(&script)
            .args(["claude", &bot.id, "tok"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("HOME", &home)
            .env("AM_RUN_ID", &run.id)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        ch.stdin.take().unwrap().write_all(br#"{"hook_event_name":"SessionStart","session_id":"s-far","source":"resume"}"#).unwrap();
        assert!(ch.wait().unwrap().success());
        let bot_dir = home.join(crate::startup::REMOTE_ROOT).join("bots").join(&bot.id);
        let mut files: Vec<_> = std::fs::read_dir(bot_dir.join("hook-spool.d"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
            .collect();
        files.sort();
        let line = std::fs::read_to_string(files.first().expect("hook 沒寫進 spool")).unwrap();
        assert!(!line.is_empty(), "hook 沒寫進 {}", bot_dir.display());
        let body: crate::hookrecv::HookBody = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(body.run_id.as_deref(), Some(run.id.as_str()));

        crate::hookrecv::process(&app, &body).await.unwrap();
        let outcome: Option<String> = sqlx::query_scalar("SELECT resume_outcome FROM runs WHERE id=?").bind(&run.id).fetch_one(&app.db).await.unwrap();
        assert_eq!(outcome.as_deref(), Some("verified"));
        forget_queue_retry_timer(&bot.id);
        flush_queued_locked(&app, &bot.id).await.unwrap();
        assert!(went_past_the_gate(&turn(&app, &queued).await), "驗證過了：往下走（這台沒有 herdr，會被放回佇列並花一次重試）");
        forget_queue_retry_timer(&bot.id);
    }
    /// 一直沒回報：等滿就走退路，flush 照常送，聊天室裡有說明。
    #[tokio::test]
    async fn a_flush_after_the_window_takes_the_deliberate_fallback() {
        let long_ago = db::iso_at(chrono::Utc::now() - chrono::Duration::seconds(600));
        let (e, bot, run) = bot_and_run("claude", 1, Some("s-silent"), &long_ago).await;
        let app = e.app.clone();
        let queued = queue_one(&app, &bot.id, "ping").await;
        forget_queue_retry_timer(&bot.id);
        flush_queued_locked(&app, &bot.id).await.unwrap();
        assert!(went_past_the_gate(&turn(&app, &queued).await));
        let outcome: Option<String> = sqlx::query_scalar("SELECT resume_outcome FROM runs WHERE id=?").bind(&run.id).fetch_one(&app.db).await.unwrap();
        assert_eq!(outcome.as_deref(), Some("unverified"));
        forget_queue_retry_timer(&bot.id);
    }
    /// 直接送入也要擋：AGM 派工排進佇列等驗證；使用者的 prompt 回可重試的 409，一筆 turn 都不建。
    #[tokio::test]
    async fn a_direct_prompt_before_verification_is_queued_for_agm_and_refused_for_the_user() {
        let (e, bot, _run) = bot_and_run("claude", 1, Some("s-keep"), &db::now()).await;
        let app = e.app.clone();
        forget_queue_retry_timer(&bot.id);

        match prompt(&app, &bot.id, "使用者直接打的", "req-user").await {
            Err(LcError::Conflict(v)) => {
                assert_eq!(v["reason"], "resume_unverified", "{v}");
                assert_eq!(v["session_id"], "s-keep");
                assert!(v["retry_after_s"].as_u64().unwrap() >= 1);
            }
            other => panic!("expected 409 resume_unverified, got {other:?}"),
        }
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns").fetch_one(&app.db).await.unwrap();
        assert_eq!(turns, 0, "被擋下的那一則不留任何 turn：同一個 request id 之後原樣重送是乾淨的");

        let out = prompt_relayed_queueable(&app, &bot.id, "AGM 派的工作", "req-agm", Some(crate::agent_relay::DAEMON_SENDER), QueueClass::Relay).await.unwrap();
        assert_eq!(out.delivery, "queued");
        assert_eq!(turn(&app, &out.turn_id).await.status, "queued");
        assert!(!e.herdr.methods().iter().any(|m| m == "pane.send_text" || m == "agent.prompt"), "一個字都沒打");
        assert!(queue_retry_timer_armed(&bot.id), "驗證到期時 flush 會回來");
        forget_queue_retry_timer(&bot.id);
    }
    /// #92 live-SSH（2026-09-23）：換身份之後的新設定目錄沒信任過專案目錄，遠端的 claude 停在信任提示
    /// （`agent_status='blocked'`）四分半。窗口是從 `runs.started_at` 算的，這段時間 CLI 根本還沒起來、
    /// 不可能有回報——不能讓它燒掉窗口，人按掉之後也要留一輪遠端 spool 掃描的時間。
    #[tokio::test]
    async fn a_pane_waiting_on_a_human_does_not_burn_the_verify_window() {
        let t0 = "2026-09-23T16:00:00.000Z";
        let (_e, bot, run) = bot_and_run("claude", 1, Some("s-1"), t0).await;
        let long_after = at("2026-09-23T16:10:00Z"); // 窗口（120 秒）早就過了
        assert_eq!(decide(&bot, &run, long_after), Gate::Expired { expected: "s-1".into() }, "沒卡住、狀態也沒變過：照原本的期限");

        let mut blocked = run.clone();
        blocked.agent_status = "blocked".into();
        assert_eq!(
            decide(&bot, &blocked, long_after),
            Gate::Waiting { expected: "s-1".into(), left: UNBLOCK_GRACE },
            "還在等人按信任提示：不可能有回報，不算到期"
        );

        // 人按下去了：`blocked → idle`，trigger 蓋上 `agent_status_since`（issue #93）。
        let mut unblocked = run.clone();
        unblocked.agent_status_since = Some("2026-09-23T16:09:50.000Z".into());
        assert_eq!(
            decide(&bot, &unblocked, long_after),
            Gate::Waiting { expected: "s-1".into(), left: Duration::from_secs(50) },
            "剛按掉：遠端的 SessionStart 還在 spool 裡，再等一輪掃描"
        );
        unblocked.agent_status_since = Some("2026-09-23T16:08:30.000Z".into());
        assert_eq!(decide(&bot, &unblocked, long_after), Gate::Expired { expected: "s-1".into() }, "寬限也過了：照樣走退路");

        let mut garbled = run.clone();
        garbled.agent_status_since = Some("not a time".into());
        assert_eq!(decide(&bot, &garbled, long_after), Gate::Expired { expected: "s-1".into() }, "讀不懂不延");
    }
    /// 同一件事整條走一遍（live 那次因為 #407 的信任提示，`resume_unverified` 這條路在遠端量不到）：
    /// 信任提示卡了十分鐘 → 人按掉 → flush 回來。閘門不能當場記 `unverified`、不能留說明、也不能把排著的
    /// 派工送進去；等 spool 裡那一行 `SessionStart` 回來才 `verified`、才放行。
    #[tokio::test]
    async fn answering_a_trust_prompt_does_not_immediately_give_up_on_the_resume() {
        let long_ago = db::iso_at(chrono::Utc::now() - chrono::Duration::seconds(600));
        let (e, bot, run) = bot_and_run("claude", 1, Some("s-trust"), &long_ago).await;
        let app = e.app.clone();
        let queued = queue_one(&app, &bot.id, "換身份之後排著的派工").await;
        // 十分鐘都卡在信任提示，然後人按下去：`blocked → idle` 讓 trigger 蓋上 `agent_status_since`。
        for st in ["blocked", "idle"] {
            sqlx::query("UPDATE runs SET agent_status = ? WHERE id = ?").bind(st).bind(&run.id).execute(&app.db).await.unwrap();
        }
        let run = db::active_run(&app.db, &bot.id).await.unwrap().unwrap();
        assert!(run.agent_status_since.is_some(), "trigger 有蓋上狀態變更時刻");
        forget_queue_retry_timer(&bot.id);

        flush_queued_locked(&app, &bot.id).await.unwrap();
        let t = turn(&app, &queued).await;
        assert_eq!((t.status.as_str(), t.flush_retries), ("queued", 0), "剛按掉信任提示：還不能算放棄，派工留在佇列");
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let (outcome, notes): (Option<String>, i64) = (
            sqlx::query_scalar("SELECT resume_outcome FROM runs WHERE id=?").bind(&run.id).fetch_one(&app.db).await.unwrap(),
            sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id=? AND role='system'")
                .bind(&conv)
                .fetch_one(&app.db)
                .await
                .unwrap(),
        );
        assert_eq!(outcome, None, "沒有結論就不能寫 unverified");
        assert_eq!(notes, 0, "也不能留那則說錯話的提醒");
        assert!(queue_retry_timer_armed(&bot.id), "掛 timer 等那一行回來");

        // 下一輪 spool 掃描把 `SessionStart` 撈回來。
        session_start(&app, &bot.id, &run.id, "s-trust").await;
        let outcome: Option<String> = sqlx::query_scalar("SELECT resume_outcome FROM runs WHERE id=?").bind(&run.id).fetch_one(&app.db).await.unwrap();
        assert_eq!(outcome.as_deref(), Some("verified"), "接回的就是原本那段");
        forget_queue_retry_timer(&bot.id);
        flush_queued_locked(&app, &bot.id).await.unwrap();
        assert!(went_past_the_gate(&turn(&app, &queued).await), "驗證過了才送");
        forget_queue_retry_timer(&bot.id);
    }
    /// daemon 重啟：行程內的 timer 都沒了。開機的 `rearm_queue_retries` 把排著的那筆叫醒一次，flush 走到閘門
    /// 照 **DB 裡的到期時間**（`started_at` 起算）重新掛 timer——不是從重啟那一刻重新算，也不會就此放行。
    #[tokio::test]
    async fn after_a_daemon_restart_the_wait_resumes_on_the_original_deadline() {
        let started = db::iso_at(chrono::Utc::now() - chrono::Duration::seconds(30));
        let (e, bot, _run) = bot_and_run("claude", 1, Some("s-wait"), &started).await;
        let queued = queue_one(&e.app, &bot.id, "重啟前排進來的").await;
        forget_queue_retry_timer(&bot.id);

        let app = tt::restart_app(&e).await;
        assert!(rearm_queue_retries(&app).await.unwrap() >= 1, "開機接回排著的 prompt");
        assert!(queue_retry_timer_armed(&bot.id));
        forget_queue_retry_timer(&bot.id); // 那個 timer 燒起來做的就是下面這一步
        flush_queued_locked(&app, &bot.id).await.unwrap();
        let t = turn(&app, &queued).await;
        assert_eq!((t.status.as_str(), t.flush_retries), ("queued", 0), "重啟不等於驗證過了");
        let left = queue_retry_timer_left(&bot.id).expect("閘門重新掛了 timer");
        assert!(left <= VERIFY_WINDOW - Duration::from_secs(29), "照原本的到期時間，不是從重啟重新算：{left:?}");
        assert!(left >= VERIFY_WINDOW - Duration::from_secs(60), "{left:?}");
        forget_queue_retry_timer(&bot.id);
    }
}
