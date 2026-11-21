#![allow(unused_imports)]
//! #1862 L2-1：從 am-lifecycle 搬來、需要真 daemon App／DB 的測試。

mod tests {
    use crate::lifecycle_tests::prelude::*;
    use crate::lifecycle::resume_nudge::test_seams::*;
    use crate::lifecycle::*;
    use crate::capabilities::Db;
    use crate::lifecycle::s6_ports::ResumeNudgeContext;
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use std::time::Instant;
    use crate::lifecycle::resume_nudge::*;
    use crate::testing::{claude_bot, env, Env};
    use serde_json::json;
    /// herdr 砍 pane：模型叫了工具，結果還沒寫進去。
    fn tool_use() -> String {
        json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "cargo test"}}]}}).to_string()
    }
    /// daemon／SIGTERM：工具被收掉（exit 137），模型還沒回。
    fn tool_result() -> String {
        json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "Exit code 137", "is_error": true}]}}).to_string()
    }
    fn reply(text: &str) -> String {
        json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}}).to_string()
    }
    fn user(text: &str) -> String {
        json!({"type": "user", "message": {"role": "user", "content": text}}).to_string()
    }
    fn log(lines: &[String]) -> String {
        lines.iter().map(|l| format!("{l}\n")).collect()
    }
    /// 被砍在工具中途的 transcript（預設尾巴）。
    fn killed() -> String {
        log(&[user("跑測試"), tool_use()])
    }
    /// 裝 cwd 目錄名；真的 transcript 在 `<CLAUDE_CONFIG_DIR>/projects/<這個>/<session>.jsonl`。
    const CWD_KEY: &str = "-Users-me-project-x";
    fn write_transcript(config_dir: &std::path::Path, bot_id: &str, body: &str) -> std::path::PathBuf {
        let dir = config_dir.join("projects").join(CWD_KEY);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join(format!("sid-{bot_id}.jsonl"));
        std::fs::write(&f, body).unwrap();
        f
    }
    /// 這顆 bot 用的帳號（`CLAUDE_CONFIG_DIR`）；測試不碰真的 `~/.claude`。
    async fn use_account(e: &Env, bot_id: &str, name: &str) -> std::path::PathBuf {
        let dir = e.dir.join(name);
        let (n, d) = (name.to_string(), dir.to_str().unwrap().to_string());
        e.app
            .cfg
            .update(move |c| {
                if !c.identities.iter().any(|i| i.name == n) {
                    c.identities.push(crate::config::IdentityCfg {
                        name: n.clone(),
                        kind: "claude".into(),
                        host: None,
                        env: [("CLAUDE_CONFIG_DIR".to_string(), d.clone())].into(),
                        args: vec![],
                    });
                }
                Ok(())
            })
            .await
            .unwrap();
        sqlx::query("UPDATE bots SET identity=? WHERE id=?").bind(name).bind(bot_id).execute(e.app.db()).await.unwrap();
        dir
    }
    async fn queued(e: &Env, bot_id: &str) -> Vec<(String, Option<String>)> {
        let conv = db::conversation_id(e.app.db(), bot_id).await.unwrap();
        sqlx::query_as(
            "SELECT m.content, m.relay_from FROM turns t JOIN messages m ON m.turn_id = t.id
             WHERE t.conversation_id = ? AND t.status = 'queued'",
        )
        .bind(&conv)
        .fetch_all(e.app.db())
        .await
        .unwrap()
    }
    /// 上一段對話有 transcript，`--resume` 接得回；尾巴是被砍的工具。
    async fn resumable(e: &Env, bot_id: &str, state: &str, status: &str) {
        resumable_at(e, bot_id, state, status, "2026-09-24T00:00:00Z", None).await;
    }
    async fn resumable_at(e: &Env, bot_id: &str, state: &str, status: &str, started_at: &str, turn_error: Option<&str>) {
        resumable_with(e, bot_id, state, status, started_at, turn_error, &killed()).await;
    }
    async fn resumable_with(e: &Env, bot_id: &str, state: &str, status: &str, started_at: &str, turn_error: Option<&str>, body: &str) {
        let account = use_account(e, bot_id, "cc").await;
        let transcript = write_transcript(&account, bot_id, body);
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at, turn_error)
             VALUES (?,?,?,?,?,?,?,'2026-09-24T00:01:00Z',?)",
        )
        .bind(db::ulid())
        .bind(bot_id)
        .bind(state)
        .bind(status)
        .bind(format!("sid-{bot_id}"))
        .bind(transcript.to_str().unwrap())
        .bind(started_at)
        .bind(turn_error)
        .execute(e.app.db())
        .await
        .unwrap();
    }
    fn strict() -> StartOpts {
        StartOpts { resume_native: true, resume_required: true, ..Default::default() }
    }
    /// SessionStart 回報了：接回的結論寫上去、畫面回到 `idle`。
    async fn settle(e: &Env, bot_id: &str, run: &str, outcome: Option<&str>) {
        sqlx::query(
            "UPDATE runs SET state='running', agent_status='idle', resume_session_id=NULL, resume_outcome=?, native_session_id=? WHERE id=?",
        )
        .bind(outcome)
        .bind(format!("sid-{bot_id}"))
        .bind(run)
        .execute(e.app.db())
        .await
        .unwrap();
    }
    /// 看一次開窗、再看一次滿 10 秒：回第二次有沒有排進去。
    async fn wait_out(e: &Env, bot_id: &str) -> bool {
        let t0 = Instant::now();
        let first = check_at(&e.app, bot_id, t0).await;
        assert!(!first, "第一次看到閒置只開始計時，不送");
        check_at(&e.app, bot_id, t0 + IDLE_WAIT).await
    }
    /// 重啟：做事中被 `?resume=native` 重啟的 claude 記成候選、**不佔佇列**；閒置中、停在等人畫面上（`blocked`）重啟的不記。
    #[tokio::test]
    async fn a_claude_restarted_while_busy_is_armed_without_taking_the_queue() {
        let e = env().await;
        let strict = strict();
        for (status, want) in [("working", true), ("blocked", false), ("idle", false)] {
            let bot = claude_bot(&e.app, &e.project_id, &format!("r-{status}")).await;
            resumable(&e, &bot.id, "stopped", "idle").await;
            let run = crate::lifecycle::start_bot_with(&e.app, &bot.id, strict.clone()).await.unwrap();
            sqlx::query("UPDATE runs SET agent_status=? WHERE id=?").bind(status).bind(&run).execute(e.app.db()).await.unwrap();
            let run = crate::lifecycle::restart_bot_with(&e.app, &bot.id, strict.clone()).await.unwrap();
            assert!(queued(&e, &bot.id).await.is_empty(), "{status}：啟動當下不佔 queued 槽");
            assert_eq!(armed_run(&bot.id), want.then_some(run), "{status}");
            crate::lifecycle::stop_bot(&e.app, &bot.id).await.unwrap();
        }
    }
    /// 閒著但還有一回合沒收（hook 還沒到）也算忙。
    #[tokio::test]
    async fn an_open_turn_counts_as_busy() {
        let e = env().await;
        let strict = strict();
        let bot = claude_bot(&e.app, &e.project_id, "open-turn").await;
        resumable(&e, &bot.id, "stopped", "idle").await;
        let run = crate::lifecycle::start_bot_with(&e.app, &bot.id, strict.clone()).await.unwrap();
        sqlx::query("UPDATE runs SET agent_status='idle' WHERE id=?").bind(&run).execute(e.app.db()).await.unwrap();
        let conv = db::conversation_id(e.app.db(), &bot.id).await.unwrap();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
            .bind(db::ulid())
            .bind(&conv)
            .bind(&run)
            .bind(db::now())
            .execute(e.app.db())
            .await
            .unwrap();
        let run = crate::lifecycle::restart_bot_with(&e.app, &bot.id, strict).await.unwrap();
        assert_eq!(armed_run(&bot.id), Some(run));
    }
    /// herdr 整個重啟：舊 run 被 pane 消失收成 `exited`、最後記著 working，`start?resume=native` 接回之後記成候選；
    /// 最後是 idle 的、停在等人畫面上的（`blocked`）、使用者自己停的（`stopped`）不記。
    #[tokio::test]
    async fn a_start_after_the_pane_died_mid_work_is_armed() {
        let e = env().await;
        let strict = strict();
        for (state, status, want) in [("exited", "working", true), ("exited", "blocked", false), ("exited", "idle", false), ("stopped", "working", false)] {
            let bot = claude_bot(&e.app, &e.project_id, &format!("s-{state}-{status}")).await;
            resumable(&e, &bot.id, state, status).await;
            let run = crate::lifecycle::start_bot_with(&e.app, &bot.id, strict.clone()).await.unwrap();
            assert_eq!(armed_run(&bot.id), want.then_some(run), "{state}/{status}");
            assert!(queued(&e, &bot.id).await.is_empty(), "{state}/{status}：啟動當下不佔 queued 槽");
            crate::lifecycle::stop_bot(&e.app, &bot.id).await.unwrap();
        }
    }
    /// 上一回合是被 API 錯誤收掉的（撞額度等，`runs.turn_error`）：不是外力砍在半路，不補——重啟與啟動兩條都一樣。
    #[tokio::test]
    async fn a_run_cut_short_by_an_api_error_is_not_nudged() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "quota-start").await;
        resumable_at(&e, &bot.id, "exited", "working", "2026-09-24T00:00:00Z", Some("API Error: 429 rate limit")).await;
        crate::lifecycle::start_bot_with(&e.app, &bot.id, strict()).await.unwrap();
        assert_eq!(armed_run(&bot.id), None, "啟動：上一個 run 撞額度收掉的不補");
        crate::lifecycle::stop_bot(&e.app, &bot.id).await.unwrap();

        let bot = claude_bot(&e.app, &e.project_id, "quota-restart").await;
        resumable(&e, &bot.id, "stopped", "idle").await;
        let run = crate::lifecycle::start_bot_with(&e.app, &bot.id, strict()).await.unwrap();
        sqlx::query("UPDATE runs SET agent_status='working', turn_error='API Error: 429 rate limit' WHERE id=?")
            .bind(&run)
            .execute(e.app.db())
            .await
            .unwrap();
        crate::lifecycle::restart_bot_with(&e.app, &bot.id, strict()).await.unwrap();
        assert_eq!(armed_run(&bot.id), None, "重啟：這一回合撞額度收掉的不補");
    }
    /// 使用者剛從網頁中斷過這個 run：是人叫它停的，重啟接回之後不替他續做。
    #[tokio::test]
    async fn a_run_the_user_interrupted_is_not_nudged() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "interrupted").await;
        resumable(&e, &bot.id, "stopped", "idle").await;
        let run = crate::lifecycle::start_bot_with(&e.app, &bot.id, strict()).await.unwrap();
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run).execute(e.app.db()).await.unwrap();
        crate::lifecycle::interrupt_grace::note_user_interrupt(&bot.id);
        crate::lifecycle::restart_bot_with(&e.app, &bot.id, strict()).await.unwrap();
        assert_eq!(armed_run(&bot.id), None);
    }
    /// 「上一個 run」是最後寫進去的那一列，不是 `started_at` 字串排最大的那列：秒精度的 `…00Z` 字串上排在
    /// 毫秒精度的 `…00.500Z` 後面（`Z` > `.`，#101），用字串排會拿舊的那個 run 的 `exited`／working 來補。
    #[tokio::test]
    async fn the_previous_run_is_the_last_one_written_not_the_largest_timestamp_string() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "mixed-precision").await;
        resumable_at(&e, &bot.id, "exited", "working", "2026-09-24T00:00:00Z", None).await;
        resumable_at(&e, &bot.id, "stopped", "idle", "2026-09-24T00:00:00.500Z", None).await;
        crate::lifecycle::start_bot_with(&e.app, &bot.id, strict()).await.unwrap();
        assert_eq!(armed_run(&bot.id), None, "真正最後那個 run 是使用者停的，不補");
    }
    /// #424 裁示丙的整張表：transcript 尾巴（沒結果的 tool_use／tool_result 無回覆／正常回覆／空檔）×
    /// 接回 verified 與否 × 10 秒內佇列有沒有東西。只有「被砍的工具＋verified＋佇列一直空」送一次。
    #[tokio::test]
    async fn the_nudge_needs_a_killed_tool_a_verified_resume_and_an_empty_queue() {
        let e = env().await;
        let tails = [
            ("tool_use", killed(), true),
            ("tool_result", log(&[user("跑"), tool_use(), tool_result()]), true),
            ("reply", log(&[user("跑"), tool_use(), tool_result(), reply("做完了")]), false),
            ("empty", String::new(), false),
        ];
        for (tail, body, killed_tool) in &tails {
            for verified in [true, false] {
                for queue_in_window in [false, true] {
                    let case = format!("{tail}/verified={verified}/queue={queue_in_window}");
                    let bot = claude_bot(&e.app, &e.project_id, &format!("m-{tail}-{verified}-{queue_in_window}")).await;
                    resumable_with(&e, &bot.id, "exited", "working", "2026-09-24T00:00:00Z", None, body).await;
                    let run = crate::lifecycle::start_bot_with(&e.app, &bot.id, strict()).await.unwrap();
                    assert_eq!(armed_run(&bot.id).as_deref(), Some(run.as_str()), "{case}：先記成候選");
                    settle(&e, &bot.id, &run, Some(if verified { "verified" } else { "mismatch" })).await;
                    let t0 = Instant::now();
                    assert!(!check_at(&e.app, &bot.id, t0).await, "{case}：不會馬上送");
                    if queue_in_window {
                        let conv = db::conversation_id(e.app.db(), &bot.id).await.unwrap();
                        let relay = crate::lifecycle::prompt::RelaySrc::trusted(Some("agm"));
                        crate::lifecycle::prompt::queue_for_next_turn(&e.app, &conv, &bot.id, "派工", "派工", "agm-1", relay).await.unwrap();
                    }
                    let sent = check_at(&e.app, &bot.id, t0 + IDLE_WAIT).await;
                    let want = *killed_tool && verified && !queue_in_window;
                    assert_eq!(sent, want, "{case}");
                    let q = queued(&e, &bot.id).await;
                    let mut expect = Vec::new();
                    if queue_in_window {
                        expect.push(("派工".to_string(), Some("agm".to_string())));
                    }
                    if want {
                        expect.push((NUDGE_TEXT.to_string(), Some("daemon".to_string())));
                    }
                    assert_eq!(q, expect, "{case}");
                    assert_eq!(armed_run(&bot.id), None, "{case}：不論送不送，這個 run 都不再看");
                    assert!(!check_at(&e.app, &bot.id, t0 + IDLE_WAIT * 3).await, "{case}：同一個 run 只補一次");
                    crate::lifecycle::stop_bot(&e.app, &bot.id).await.unwrap();
                }
            }
        }
    }
    /// 10 秒從「接回驗過且 idle」起算：還沒驗過的時候怎麼等都不算，驗過後不滿 10 秒不送。
    #[tokio::test]
    async fn the_ten_seconds_start_once_the_resume_is_verified_and_idle() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "window").await;
        resumable(&e, &bot.id, "exited", "working").await;
        let run = crate::lifecycle::start_bot_with(&e.app, &bot.id, strict()).await.unwrap();
        let t0 = Instant::now();
        sqlx::query("UPDATE runs SET state='running', agent_status='idle' WHERE id=?").bind(&run).execute(e.app.db()).await.unwrap();
        assert!(!check_at(&e.app, &bot.id, t0).await, "接回還沒結論：等");
        assert!(!check_at(&e.app, &bot.id, t0 + IDLE_WAIT * 2).await, "還沒結論就不開始算");
        settle(&e, &bot.id, &run, Some("verified")).await;
        let t1 = t0 + IDLE_WAIT * 3;
        assert!(!check_at(&e.app, &bot.id, t1).await, "驗過、閒著：從這裡開始算");
        assert!(!check_at(&e.app, &bot.id, t1 + IDLE_WAIT - Duration::from_secs(1)).await, "9 秒：還不送");
        assert!(queued(&e, &bot.id).await.is_empty());
        assert!(check_at(&e.app, &bot.id, t1 + IDLE_WAIT).await, "滿 10 秒：送");
        assert_eq!(queued(&e, &bot.id).await, vec![(NUDGE_TEXT.to_string(), Some("daemon".to_string()))]);
    }
    /// 驗過之後在等的 10 秒裡變成 `working`／`blocked`、或這個 run 上已經送過別的回合：這一輪取消，回到 idle 也不補。
    #[tokio::test]
    async fn activity_during_the_window_cancels_the_nudge() {
        let e = env().await;
        for case in ["working", "blocked", "turn"] {
            let bot = claude_bot(&e.app, &e.project_id, &format!("w-{case}")).await;
            resumable(&e, &bot.id, "exited", "working").await;
            let run = crate::lifecycle::start_bot_with(&e.app, &bot.id, strict()).await.unwrap();
            settle(&e, &bot.id, &run, Some("verified")).await;
            let t0 = Instant::now();
            assert!(!check_at(&e.app, &bot.id, t0).await);
            if case == "turn" {
                let conv = db::conversation_id(e.app.db(), &bot.id).await.unwrap();
                sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','completed','ok',?)")
                    .bind(db::ulid())
                    .bind(&conv)
                    .bind(&run)
                    .bind(db::now())
                    .execute(e.app.db())
                    .await
                    .unwrap();
            } else {
                sqlx::query("UPDATE runs SET agent_status=? WHERE id=?").bind(case).bind(&run).execute(e.app.db()).await.unwrap();
                assert!(!check_at(&e.app, &bot.id, t0 + Duration::from_secs(3)).await);
                sqlx::query("UPDATE runs SET agent_status='idle' WHERE id=?").bind(&run).execute(e.app.db()).await.unwrap();
            }
            assert!(!check_at(&e.app, &bot.id, t0 + IDLE_WAIT).await, "{case}");
            assert!(queued(&e, &bot.id).await.is_empty(), "{case}");
            crate::lifecycle::stop_bot(&e.app, &bot.id).await.unwrap();
        }
    }
    /// transcript 照這顆 bot 實際帳號的 `CLAUDE_CONFIG_DIR` 找：換身份後 CLI 接著寫的是複製進新帳號那份，
    /// 舊帳號那份（run 上記的路徑）停在接回之前，不看它。
    #[tokio::test]
    async fn the_transcript_is_read_from_the_accounts_config_dir() {
        let e = env().await;
        for (case, old_body, account_body, want) in [
            ("帳號那份是被砍的工具", log(&[user("跑"), reply("好了")]), killed(), true),
            ("帳號那份已經回覆了", killed(), log(&[user("跑"), tool_use(), tool_result(), reply("好了")]), false),
        ] {
            let bot = claude_bot(&e.app, &e.project_id, &format!("acct-{want}")).await;
            let old = write_transcript(&e.dir.join(format!("old-{want}")), &bot.id, &old_body);
            sqlx::query(
                "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at)
                 VALUES (?,?,'exited','working',?,?,'2026-09-24T00:00:00Z','2026-09-24T00:01:00Z')",
            )
            .bind(db::ulid())
            .bind(&bot.id)
            .bind(format!("sid-{}", bot.id))
            .bind(old.to_str().unwrap())
            .execute(e.app.db())
            .await
            .unwrap();
            let account = use_account(&e, &bot.id, &format!("cc-new-{want}")).await;
            let run = crate::lifecycle::start_bot_with(&e.app, &bot.id, strict()).await.unwrap();
            write_transcript(&account, &bot.id, &account_body);
            settle(&e, &bot.id, &run, Some("verified")).await;
            assert_eq!(wait_out(&e, &bot.id).await, want, "{case}");
            crate::lifecycle::stop_bot(&e.app, &bot.id).await.unwrap();
        }
    }
    /// flush 放行前的最後一關：排進去之後 run 換掉、或接回結論翻成不是 `verified`，撤掉並在聊天室說明（#430）。
    #[tokio::test]
    async fn a_queued_nudge_is_withdrawn_unless_the_resume_is_still_verified() {
        let e = env().await;
        for (case, outcome, withdrawn) in [("verified", "verified", false), ("mismatch", "mismatch", true), ("unverified", "unverified", true)] {
            let bot = claude_bot(&e.app, &e.project_id, &format!("v-{case}")).await;
            resumable(&e, &bot.id, "exited", "working").await;
            let run = crate::lifecycle::start_bot_with(&e.app, &bot.id, strict()).await.unwrap();
            settle(&e, &bot.id, &run, Some("verified")).await;
            assert!(wait_out(&e, &bot.id).await, "{case}：先排進去");
            sqlx::query("UPDATE runs SET resume_outcome=? WHERE id=?").bind(outcome).bind(&run).execute(e.app.db()).await.unwrap();
            let crid = format!("{CRID_PREFIX}{run}");
            forget_queue_retry_timer(&bot.id);
            flush_queued_locked(&e.app, &bot.id).await.unwrap();
            forget_queue_retry_timer(&bot.id);
            let conv = db::conversation_id(e.app.db(), &bot.id).await.unwrap();
            let t: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE conversation_id=? AND client_request_id=?")
                .bind(&conv)
                .bind(&crid)
                .fetch_one(e.app.db())
                .await
                .unwrap();
            let notes: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'")
                .bind(&t.id)
                .fetch_all(e.app.db())
                .await
                .unwrap();
            let said = notes.iter().any(|n| n.contains("續行提示沒有送出"));
            if withdrawn {
                assert_eq!((t.status.as_str(), t.flush_retries, said), ("failed", 0, true), "{case}：撤掉、一個字都沒打、有說明：{notes:?}");
            } else {
                assert!(!said && (t.status != "queued" || t.flush_retries > 0), "{case}：過了這一關往下送：{t:?} {notes:?}");
            }
            crate::lifecycle::stop_bot(&e.app, &bot.id).await.unwrap();
        }
    }
    /// 接不回、退回開新對話：沒有原本的工作可續，不補。
    #[tokio::test]
    async fn a_fallback_to_a_new_conversation_is_not_nudged() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "fresh").await;
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, started_at, ended_at)
             VALUES (?,?,'exited','working','2026-09-24T00:00:00Z','2026-09-24T00:01:00Z')",
        )
        .bind(db::ulid())
        .bind(&bot.id)
        .execute(e.app.db())
        .await
        .unwrap();
        crate::lifecycle::start_bot_with(&e.app, &bot.id, StartOpts { resume_native: true, ..Default::default() }).await.unwrap();
        assert_eq!(armed_run(&bot.id), None);
        assert!(queued(&e, &bot.id).await.is_empty());
    }
    /// 啟動前佇列裡已經有一則（AGM 的派工）：不擠掉它、也不疊第二則，這一輪直接取消。
    #[tokio::test]
    async fn an_already_queued_prompt_is_left_alone() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "has-queue").await;
        resumable(&e, &bot.id, "exited", "working").await;
        let conv = db::conversation_id(e.app.db(), &bot.id).await.unwrap();
        let relay = crate::lifecycle::prompt::RelaySrc::trusted(Some("agm"));
        crate::lifecycle::prompt::queue_for_next_turn(&e.app, &conv, &bot.id, "派工", "派工", "agm-1", relay).await.unwrap();
        let run = crate::lifecycle::start_bot_with(&e.app, &bot.id, strict()).await.unwrap();
        settle(&e, &bot.id, &run, Some("verified")).await;
        assert!(!check_at(&e.app, &bot.id, Instant::now()).await);
        assert_eq!(armed_run(&bot.id), None);
        assert_eq!(queued(&e, &bot.id).await, vec![("派工".to_string(), Some("agm".to_string()))]);
    }
}
