#![allow(unused_imports)]
//! stuck_turns 的測試（#1862 L2-3e 自 crate 搬來）。
#[cfg(test)]
mod tests {
    use crate::lifecycle::stuck_turns::test_seams::*;
    use std::time::{Duration, Instant};
    use std::collections::{HashMap, HashSet};
    use serde_json::{json, Value};
    use crate::lifecycle::stuck_turns::*;
    use crate::lifecycle::*;
    use crate::lifecycle_tests::prelude::*;
        use crate::testing as tt;

    /// 本機 claude bot、一顆 idle 的 run、一筆 in_flight turn（我們送的是 `prompt`）。
    async fn stuck(prompt: &str) -> Fixture {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "k8").await;
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle','ws-1','pane-1','agent','test',?)",
        )
        .bind(&run_id)
        .bind(&bot.id)
        .bind(db::now())
        .execute(app.db())
        .await
        .unwrap();
        let conv = db::conversation_id(app.db(), &bot.id).await.unwrap();
        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at)
             VALUES (?,?,?,'web','in_flight','unknown',?)",
        )
        .bind(&turn_id)
        .bind(&conv)
        .bind(&run_id)
        .bind(db::now())
        .execute(app.db())
        .await
        .unwrap();
        insert_message(&app, &conv, Some(&turn_id), "user", prompt, "web", false, None).await.unwrap();
        Fixture { env, bot_id: bot.id, run_id, turn_id }
    }

    async fn turn(app: &Arc<App>, id: &str) -> db::Turn {
        sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id = ?").bind(id).fetch_one(app.db()).await.unwrap()
    }

    const MIN: Duration = Duration::from_secs(60);

    struct Fixture {
        env: tt::Env,
        bot_id: String,
        run_id: String,
        turn_id: String,
    }

    fn claude_user(text: &str) -> String {
        json!({"type": "user", "message": {"role": "user", "content": text}}).to_string()
    }

    fn claude_assistant(stop: &str, parts: Value) -> String {
        json!({"type": "assistant", "message": {"role": "assistant", "stop_reason": stop, "content": parts}}).to_string()
    }

    async fn own_transcript(f: &Fixture) -> std::path::PathBuf {
        let config_dir = f.env.dir.join("claude-config");
        sqlx::query("UPDATE bots SET env_json=? WHERE id=?")
            .bind(serde_json::json!({"CLAUDE_CONFIG_DIR": config_dir.to_string_lossy()}).to_string())
            .bind(&f.bot_id)
            .execute(f.env.app.db())
            .await
            .unwrap();
        let dir = config_dir.join("projects/-test");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("transcript.jsonl")
    }

    async fn messages(app: &Arc<App>, turn_id: &str) -> Vec<(String, String, String)> {
        sqlx::query_as("SELECT role, source, content FROM messages WHERE turn_id = ? ORDER BY created_at, rowid")
            .bind(turn_id)
            .fetch_all(app.db())
            .await
            .unwrap()
    }

    async fn stale_candidate(app: &std::sync::Arc<crate::state::App>, project_id: &str, name: &str) -> (String, String, String) {
        let bot = tt::claude_bot(app, project_id, name).await;
        let run_id = tt::fake_run(app, &bot.id).await;
        let conversation_id = db::conversation_id(app.db(), &bot.id).await.unwrap();
        let turn_id = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,?,'web','in_flight','ok','x',?)")
            .bind(&turn_id)
            .bind(&conversation_id)
            .bind(&run_id)
            .bind(db::iso_in(-3600))
            .execute(app.db())
            .await
            .unwrap();
        (bot.id, run_id, turn_id)
    }

    /// Old or poisoned run rows must not let a local Claude completion be inferred from an
    /// arbitrary daemon-local JSONL file.
    #[tokio::test]
    async fn a_stuck_turn_does_not_import_reply_from_outside_the_bots_projects_dir() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "stuck-path-guard").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        let scratch = tt::track(std::env::temp_dir().join(format!("am-stuck-path-{}", db::ulid())));
        std::fs::create_dir_all(&scratch).unwrap();
        let outside = scratch.join("foreign.jsonl");
        let log = [
            claude_user("requested prompt"),
            claude_assistant("end_turn", json!([{"type":"text","text":"FOREIGN-REPLY-5592"}])),
        ].join("\n");
        std::fs::write(&outside, log).unwrap();
        sqlx::query("UPDATE runs SET transcript_path=? WHERE id=?")
            .bind(outside.to_string_lossy())
            .bind(&run_id)
            .execute(app.db())
            .await
            .unwrap();
        let run = db::run(app.db(), &run_id).await.unwrap().unwrap();

        let reply = logged_reply(&app, &bot, &run, &["requested prompt".into()]).await;
        assert_eq!(reply, None, "任意本機 JSONL 不能冒充 bot 的回覆證據");
    }

    /// 一顆 bot 的鎖被握著不放：巡邏跳過它，別的 bot 卡住的回合照收，整輪不會停在那裡（issue #1201）。
    #[tokio::test]
    async fn a_bot_whose_lock_is_held_does_not_stall_the_sweep_for_other_bots() {
        let f = stuck("被握著鎖的那顆").await;
        let app = f.env.app.clone();
        let other = tt::claude_bot(&app, &f.env.project_id, "other-stuck").await;
        let other_run = tt::fake_run(&app, &other.id).await;
        sqlx::query("UPDATE runs SET agent_status = 'idle' WHERE id = ?").bind(&other_run).execute(app.db()).await.unwrap();
        let conv = db::conversation_id(app.db(), &other.id).await.unwrap();
        let other_turn = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,?,'web','in_flight','ok','x',?)")
            .bind(&other_turn)
            .bind(&conv)
            .bind(&other_run)
            .bind(db::iso_in(-3600))
            .execute(app.db())
            .await
            .unwrap();
        let t0 = Instant::now();
        observe_at(&f.run_id, "idle", t0);
        observe_at(&other_run, "idle", t0);

        let lock = app.bot_lock(&f.bot_id).await;
        let _held = lock.lock().await;
        let done = tokio::time::timeout(Duration::from_secs(30), sweep_with(&app, None, t0 + 6 * MIN, 5 * MIN)).await;
        assert_eq!(done.ok(), Some(vec![other_turn.clone()]), "握著鎖的那顆跳過，另一顆照收，整輪有回來");
        assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight", "被跳過的那顆這一輪不動");
        assert!(lock.try_lock().is_err(), "前提：鎖整段都還握著");
    }

    /// A row near the end of a full sweep must not wait behind per-bot lock timeouts (issue #1264).
    #[tokio::test]
    async fn a_sweep_skips_many_locked_bots_and_closes_the_unlocked_candidate() {
        let env = tt::env().await;
        let app = env.app.clone();
        let t0 = Instant::now();
        let mut locked_turns = Vec::new();
        let mut locked_bots = Vec::new();

        for i in 0..20 {
            let name = format!("locked-stuck-{i}");
            let (bot_id, run_id, turn_id) = stale_candidate(&app, &env.project_id, &name).await;
            observe_at(&run_id, "idle", t0);
            locked_turns.push(turn_id);
            locked_bots.push(bot_id);
        }

        let (target_bot, target_run, target_turn) = stale_candidate(&app, &env.project_id, "unlocked-stuck-late").await;
        observe_at(&target_run, "idle", t0);

        let locks = futures::future::join_all(locked_bots.iter().map(|id| app.bot_lock(id))).await;
        let _guards = locks.into_iter().map(|lock| lock.try_lock_owned().unwrap()).collect::<Vec<_>>();
        let sweep = tokio::time::timeout(Duration::from_secs(2), sweep_at(&app, None, t0 + 6 * MIN, 5 * MIN)).await;

        assert_eq!(sweep.ok(), Some(vec![target_turn.clone()]), "blocked bots must be skipped promptly so the later eligible turn is closed");
        for turn_id in locked_turns {
            assert_eq!(turn(&app, &turn_id).await.status, "in_flight", "busy bot turns remain for a later sweep");
        }
        assert_eq!(turn(&app, &target_turn).await.status, "completed_fallback");
        let _ = target_bot;
    }

    /// #708：移交出去的專案，in-flight 回合是對方 daemon 的：巡邏不收；收回之後照常收。
    #[tokio::test]
    async fn the_sweep_leaves_a_handed_off_projects_turn_alone() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
        let run = tt::fake_run(&app, &bot.id).await;
        let conv = crate::db::conversation_id(app.db(), &bot.id).await.unwrap();
        let turn = crate::db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,?,'web','in_flight','ok','x',?)")
            .bind(&turn)
            .bind(&conv)
            .bind(&run)
            .bind(crate::db::iso_in(-3600))
            .execute(app.db())
            .await
            .unwrap();
        let status = |app: Arc<App>, turn: String| async move {
            sqlx::query_scalar::<_, String>("SELECT status FROM turns WHERE id = ?").bind(turn).fetch_one(app.db()).await.unwrap()
        };
        sqlx::query("UPDATE projects SET handed_off_to = 'agm-host' WHERE id = ?").bind(&env.project_id).execute(app.db()).await.unwrap();
        sweep_at(&app, None, Instant::now(), Duration::ZERO).await;
        assert_eq!(status(app.clone(), turn.clone()).await, "in_flight");

        sqlx::query("UPDATE projects SET handed_off_to = NULL WHERE id = ?").bind(&env.project_id).execute(app.db()).await.unwrap();
        sweep_at(&app, None, Instant::now(), Duration::ZERO).await;
        assert_ne!(status(app.clone(), turn.clone()).await, "in_flight", "收回之後照常收");
    }

    #[test]
    fn a_codex_turn_is_proven_by_task_complete_with_a_message() {
        let user = |t: &str| json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": t}]}}).to_string();
        let complete = |m: Value| json!({"type": "event_msg", "payload": {"type": "task_complete", "last_agent_message": m}}).to_string();
        let sent = vec!["跑測試".to_string()];
        let log = [user("跑測試"), json!({"type": "event_msg", "payload": {"type": "token_count"}}).to_string(), complete(json!("測試全過"))].join("\n");
        assert_eq!(codex_reply_after(&log, &sent).as_deref(), Some("測試全過"));
        let limit = [user("跑測試"), complete(Value::Null)].join("\n");
        assert_eq!(codex_reply_after(&limit, &sent), None, "撞額度只有 error，不是回覆");
        assert_eq!(codex_reply_after(&user("跑測試"), &sent), None, "還沒 task_complete");
    }

    /// 2026-09-26 cf-ox-fork-fork：Session paused 選單關掉、run 回到 idle、沒有回覆——不等 5 分鐘，馬上收，
    /// 對話裡寫明是被暫停，網頁不再顯示等待中。
    #[tokio::test]
    async fn a_turn_left_open_by_a_closed_session_paused_menu_is_closed_right_away() {
        let f = stuck("被暫停的那則").await;
        let app = f.env.app.clone();
        assert_eq!(close_after_session_paused(&app, &f.run_id, &f.turn_id).await.as_deref(), Some(f.turn_id.as_str()));
        assert_eq!(turn(&app, &f.turn_id).await.status, "completed_fallback");
        let msgs = messages(&app, &f.turn_id).await;
        assert!(msgs.iter().any(|(role, _, c)| role == "system" && c.contains("Session paused")), "{msgs:?}");
        assert_eq!(close_after_session_paused(&app, &f.run_id, &f.turn_id).await, None, "已經收掉就不再動");
    }

    /// 選了「換模型重試」：claude 接著跑同一回合（herdr 報 working），不能收。
    #[tokio::test]
    async fn a_session_paused_turn_that_resumed_is_left_open() {
        let f = stuck("換模型重試那則").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&f.run_id).execute(app.db()).await.unwrap();
        assert_eq!(close_after_session_paused(&app, &f.run_id, &f.turn_id).await, None);
        assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight");
        let _ = &f.bot_id;
    }

    /// #575：選單關掉時的 T1 已正常收尾，下一個 T2 已 in-flight，但 herdr 還沒把 run 從 idle 改成 working。
    /// T1 的舊計時器必須只看 T1，不能把同一 run 上現在的 T2 當成它要收的回合。
    #[tokio::test]
    async fn a_stale_session_paused_close_does_not_close_the_next_turn() {
        let f = stuck("被暫停的舊回合").await;
        let app = f.env.app.clone();

        assert_eq!(
            crate::lifecycle::turn_controller::set_status(app.db(), &f.turn_id, "in_flight", "completed", "Stop hook").await.unwrap(),
            crate::lifecycle::turn_controller::Outcome::Applied,
            "T1 由正常 Stop hook 路徑先收尾"
        );

        let conversation_id = db::conversation_id(app.db(), &f.bot_id).await.unwrap();
        let next_turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at)
             VALUES (?,?,?,'web','in_flight','ok',?)",
        )
        .bind(&next_turn_id)
        .bind(&conversation_id)
        .bind(&f.run_id)
        .bind(db::now())
        .execute(app.db())
        .await
        .unwrap();
        let run_status: String = sqlx::query_scalar("SELECT agent_status FROM runs WHERE id=?")
            .bind(&f.run_id)
            .fetch_one(app.db())
            .await
            .unwrap();
        assert_eq!(run_status, "idle", "T2 已在 DB 開始，herdr working 事件尚未到");

        assert_eq!(close_after_session_paused(&app, &f.run_id, &f.turn_id).await, None, "T1 的舊計時器只能對 T1 做 no-op");
        assert_eq!(turn(&app, &f.turn_id).await.status, "completed", "正常收尾的 T1 保持終態");
        assert_eq!(turn(&app, &next_turn_id).await.status, "in_flight", "舊計時器不得誤關 T2");
    }

    /// 門檻之前不收；過了門檻、transcript 證不出來 → completed_fallback＋寫明理由，不是 failed；
    /// 交辦照一般流程走到 awaiting_review。
    #[tokio::test]
    async fn an_idle_run_past_the_threshold_closes_its_turn_as_fallback_with_a_reason() {
        let f = stuck("部署 6b84fa5").await;
        let app = f.env.app.clone();
        let a = crate::supervisor::store::insert_assignment(app.db(), None, &f.bot_id, "crid-stuck", "部署 6b84fa5", &[], None, true)
            .await
            .unwrap()
            .id;
        crate::supervisor::store::mark_delivered(app.db(), &a, &f.turn_id, "ok").await.unwrap();
        let mut events = app.subscribe_turns();

        let t0 = Instant::now();
        observe_at(&f.run_id, "idle", t0);
        assert!(sweep_at(&app, None, t0 + 4 * MIN, 5 * MIN).await.is_empty(), "四分鐘還不收");
        assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight");

        assert_eq!(sweep_at(&app, None, t0 + 6 * MIN, 5 * MIN).await, vec![f.turn_id.clone()]);
        let t = turn(&app, &f.turn_id).await;
        assert_eq!(t.status, "completed_fallback", "證不出來是 fallback，不是 failed");
        let msgs = messages(&app, &f.turn_id).await;
        let why = msgs.iter().find(|m| m.0 == "system").expect("理由寫進對話");
        assert!(why.2.contains("閒置 6 分鐘") && why.2.contains("reconcile"), "{why:?}");

        let ev = events.try_recv().expect("收尾有推 turn 事件");
        assert_eq!((ev.turn_id.as_str(), ev.status.as_str()), (f.turn_id.as_str(), "completed_fallback"));
        crate::supervisor::controller::reconcile(&app).await;
        let row = crate::supervisor::store::assignment(app.db(), &a).await.unwrap().unwrap();
        assert_eq!(row.status, "awaiting_review", "交辦照一般回合結束流程，不留在 delivered");

        assert!(sweep_at(&app, None, t0 + 20 * MIN, 5 * MIN).await.is_empty());
    }

    /// #1758：閒置自動壓縮開出來的 external 空回合，回合開始後收過 compact 的 SessionStart → 照常收，不寫說明；
    /// 沒收過 compact 的同樣空回合照舊寫說明。
    #[tokio::test]
    async fn an_empty_external_turn_opened_by_compaction_is_closed_without_a_note() {
        for compacted in [true, false] {
            let f = stuck("x").await;
            let app = f.env.app.clone();
            sqlx::query("DELETE FROM messages WHERE turn_id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
            sqlx::query("UPDATE turns SET origin='external', delivery='ok', created_at=? WHERE id=?")
                .bind(db::iso_in(-600))
                .bind(&f.turn_id)
                .execute(app.db())
                .await
                .unwrap();
            let _ = am_base::hook_inbox::migrate(app.db()).await;
            let source = if compacted { "compact" } else { "resume" };
            let body = serde_json::json!({"bot_id": f.bot_id, "provider": "claude",
                "payload": {"hook_event_name": "SessionStart", "source": source}});
            sqlx::query("INSERT INTO hook_events (id, bot_id, provider, source, body_json, received_at) VALUES (?, ?, 'claude', 'http', ?, ?)")
                .bind(db::ulid())
                .bind(&f.bot_id)
                .bind(body.to_string())
                .bind(db::iso_in(-300))
                .execute(app.db())
                .await
                .unwrap();

            let t0 = Instant::now();
            observe_at(&f.run_id, "idle", t0);
            assert_eq!(sweep_at(&app, None, t0 + 6 * MIN, 5 * MIN).await, vec![f.turn_id.clone()]);
            assert_eq!(turn(&app, &f.turn_id).await.status, "completed_fallback");
            let notes = messages(&app, &f.turn_id).await.into_iter().filter(|m| m.0 == "system").count();
            assert_eq!(notes, usize::from(!compacted), "compacted={compacted}");
        }
    }

    #[tokio::test]
    async fn observation_hooks_do_not_extend_idle_activity() {
        for event in ["SessionStart", "StatusLine", "UserPromptSubmit", "Notification"] {
            let f = stuck("fixture").await;
            let app = f.env.app.clone();
            let _ = am_base::hook_inbox::migrate(app.db()).await;
            let body = serde_json::json!({"payload": {"hook_event_name": event}});
            sqlx::query("INSERT INTO hook_events (id, bot_id, provider, source, body_json, received_at, processed_at) VALUES (?, ?, 'claude', 'http', ?, ?, ?)")
                .bind(db::ulid()).bind(&f.bot_id).bind(body.to_string()).bind(db::now()).bind(db::now()).execute(app.db()).await.unwrap();
            sqlx::query("UPDATE turns SET created_at=? WHERE id=?").bind(db::iso_in(-600)).bind(&f.turn_id).execute(app.db()).await.unwrap();
            let t0 = Instant::now();
            observe_at(&f.run_id, "idle", t0);
            assert_eq!(sweep_at(&app, None, t0 + 6 * MIN, 5 * MIN).await, vec![f.turn_id.clone()], "{event}");
        }
    }

    /// #1763：run 報 idle 已過門檻，但門檻內還有 PostToolUse 進來（遠端長跑的 Bash）：不收，重新計時；hook 停了之後才照常收。
    #[tokio::test]
    async fn recent_hook_activity_keeps_an_idle_reported_turn_open() {
        let f = stuck("部署 demo").await;
        let app = f.env.app.clone();
        let _ = am_base::hook_inbox::migrate(app.db()).await;
        let body = serde_json::json!({"bot_id": f.bot_id, "provider": "claude", "payload": {"hook_event_name": "PostToolUse", "tool_name": "Bash"}});
        sqlx::query("INSERT INTO hook_events (id, bot_id, provider, source, body_json, received_at, processed_at) VALUES (?, ?, 'claude', 'http', ?, ?, ?)")
            .bind(db::ulid())
            .bind(&f.bot_id)
            .bind(body.to_string())
            .bind(db::iso_in(-60))
            .bind(db::iso_in(-60))
            .execute(app.db())
            .await
            .unwrap();
        sqlx::query("UPDATE turns SET created_at=? WHERE id=?").bind(db::iso_in(-600)).bind(&f.turn_id).execute(app.db()).await.unwrap();

        let t0 = Instant::now();
        observe_at(&f.run_id, "idle", t0);
        assert!(sweep_at(&app, None, t0 + 6 * MIN, 5 * MIN).await.is_empty(), "一分鐘前還有工具 hook：不收");
        assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight");

        sqlx::query("UPDATE hook_events SET processed_at=? WHERE bot_id=?").bind(db::iso_in(-900)).bind(&f.bot_id).execute(app.db()).await.unwrap();
        assert!(sweep_at(&app, None, t0 + 8 * MIN, 5 * MIN).await.is_empty(), "從上次看到活動重新計時，還沒滿門檻");
        assert_eq!(sweep_at(&app, None, t0 + 14 * MIN, 5 * MIN).await, vec![f.turn_id.clone()], "hook 停了、再閒置滿門檻才收");
    }

    #[tokio::test]
    async fn unconfirmed_idle_turn_is_not_completed_without_a_proven_reply() {
        let f = stuck("unconfirmed prompt").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE turns SET delivery='unknown', delivery_verified=0 WHERE id=?").bind(&f.turn_id).execute(app.db()).await.unwrap();
        let run = db::run(app.db(), &f.run_id).await.unwrap().unwrap();
        let t = turn(&app, &f.turn_id).await;
        assert!(close_locked(&app, &run, &t, CloseWhy::Idle(5 * MIN)).await.unwrap());
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("failed", "unknown"));
        assert!(messages(&app, &f.turn_id).await.iter().any(|m| m.2.contains("送出未確認")));
    }

    /// #903：`delivery='pending'` ＝ 從來沒送出去（送達結果沒寫、也沒有字進過 pane）的 in_flight 回合，閒置 watchdog
    /// 不能收成 `completed_fallback`（等於說 agent 做完了）：收成 failed＋`delivery='failed'`＋說明，請使用者重送。
    #[tokio::test]
    async fn a_never_sent_pending_turn_is_failed_not_completed_by_the_idle_sweep() {
        let f = stuck("從未送出的那則").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE turns SET delivery='pending' WHERE id=?").bind(&f.turn_id).execute(app.db()).await.unwrap();
        let a = crate::supervisor::store::insert_assignment(app.db(), None, &f.bot_id, "crid-never-sent", "從未送出的那則", &[], None, true)
            .await
            .unwrap()
            .id;
        crate::supervisor::store::mark_delivered(app.db(), &a, &f.turn_id, "queued").await.unwrap();

        let t0 = Instant::now();
        observe_at(&f.run_id, "idle", t0);
        assert_eq!(sweep_at(&app, None, t0 + 6 * MIN, 5 * MIN).await, vec![f.turn_id.clone()]);
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("failed", "failed"));
        let msgs = messages(&app, &f.turn_id).await;
        assert!(!msgs.iter().any(|m| m.0 == "assistant"), "沒有 assistant 訊息：{msgs:?}");
        assert!(msgs.iter().any(|m| m.0 == "system" && m.2.contains("送出前就卡住")), "{msgs:?}");

        crate::supervisor::controller::reconcile(&app).await;
        let row = crate::supervisor::store::assignment(app.db(), &a).await.unwrap().unwrap();
        assert_eq!(row.turn_status.as_deref(), Some("failed"), "{:?}", row.turn_status);
    }

    /// 2026-09-16 13:17 實況：AM-2-M 從 13:01 就閒置，13:17:01 收到新 prompt（貼上的圖片轉成 `[Image #33]` 時 Enter 被吃掉，
    /// agent 一直沒轉成 working），13:17:10 的掃描看到「run 閒置 15 分鐘」就把剛送出 9 秒的回合收成 completed_fallback。
    /// 閒置要從**這筆送達之後**才算。
    #[tokio::test]
    async fn a_prompt_just_sent_to_a_long_idle_bot_is_not_mistaken_for_a_stuck_turn() {
        let f = stuck("ui issue?").await;
        let app = f.env.app.clone();
        let t0 = Instant::now();
        observe_at(&f.run_id, "idle", t0);
        let sent = (chrono::Utc::now() + chrono::Duration::minutes(15) - chrono::Duration::seconds(9))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        sqlx::query("UPDATE turns SET created_at = ?, delivered_at = ? WHERE id = ?")
            .bind(&sent)
            .bind(&sent)
            .bind(&f.turn_id)
            .execute(app.db())
            .await
            .unwrap();

        assert!(sweep_at(&app, None, t0 + 15 * MIN, 5 * MIN).await.is_empty(), "剛送出 9 秒，不是卡住");
        assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight");
        assert_eq!(sweep_at(&app, None, t0 + 21 * MIN, 5 * MIN).await, vec![f.turn_id.clone()]);
        let why = messages(&app, &f.turn_id).await.into_iter().find(|m| m.0 == "system").expect("理由寫進對話");
        assert!(why.2.contains("閒置 6 分鐘"), "{why:?}");
    }

    /// transcript 證得出完整回覆 → completed，回覆補進對話。
    #[tokio::test]
    async fn a_reply_proven_by_the_transcript_completes_the_turn_and_is_backfilled() {
        let f = stuck("部署 6b84fa5").await;
        let app = f.env.app.clone();
        let path = own_transcript(&f).await;
        let log = [
            claude_user("部署 6b84fa5"),
            claude_assistant("end_turn", json!([{"type": "text", "text": "部署完成：pid 42894"}])),
        ]
        .join("\n");
        std::fs::write(&path, log + "\n").unwrap();
        sqlx::query("UPDATE runs SET transcript_path = ? WHERE id = ?").bind(path.to_string_lossy()).bind(&f.run_id).execute(app.db()).await.unwrap();

        let t0 = Instant::now();
        observe_at(&f.run_id, "idle", t0);
        assert_eq!(sweep_at(&app, None, t0 + 6 * MIN, 5 * MIN).await.len(), 1);
        assert_eq!(turn(&app, &f.turn_id).await.status, "completed");
        let msgs = messages(&app, &f.turn_id).await;
        assert!(msgs.contains(&("assistant".into(), "transcript".into(), "部署完成：pid 42894".into())), "{msgs:?}");
        assert!(!msgs.iter().any(|m| m.0 == "system"), "證得出來就不用解釋");
    }

    /// #218：herdr `agent.prompt` 送的回合，claude 2.1.278 在 transcript 裡把 prompt 包成 `<pasted_content id=…>`
    /// （真 transcript，2026-09-19 實測）。Stop 漏掉時照樣找得到我們那一則、存下它之後的回覆；以前逐字比對找不到起點，
    /// 收成 `completed_fallback`，transcript 裡那份完整的回覆就丟了。
    #[tokio::test]
    async fn a_prompt_the_cli_wrapped_as_pasted_content_still_proves_the_reply() {
        let f = stuck("請只回覆 OK 兩個字母，不要多說任何其他的話，也不要使用任何工具。").await;
        let app = f.env.app.clone();
        let path = own_transcript(&f).await;
        let log: Vec<&str> = include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/claude_2.1.278_pasted_content.jsonl").lines().take(2).collect();
        assert!(log[0].contains("<pasted_content id=\\\"c4ab\\\">"), "fixture 第一列就是包起來的那一則");
        std::fs::write(&path, log.join("\n") + "\n").unwrap();
        sqlx::query("UPDATE runs SET transcript_path = ? WHERE id = ?").bind(path.to_string_lossy()).bind(&f.run_id).execute(app.db()).await.unwrap();

        let t0 = Instant::now();
        observe_at(&f.run_id, "idle", t0);
        assert_eq!(sweep_at(&app, None, t0 + 6 * MIN, 5 * MIN).await.len(), 1);
        assert_eq!(turn(&app, &f.turn_id).await.status, "completed");
        let msgs = messages(&app, &f.turn_id).await;
        assert!(msgs.contains(&("assistant".into(), "transcript".into(), "OK".into())), "{msgs:?}");
    }

    /// #193：收之前讀不到主機（證據要看本機 transcript）：這一輪不收、回錯，下一輪再看。以前當成「證不出回覆」，
    /// 照樣收成 `completed_fallback`，transcript 裡那份完整的回覆就沒存。
    #[tokio::test]
    async fn a_stuck_turn_whose_proof_cannot_be_read_is_left_for_the_next_sweep() {
        let f = stuck("部署 6b84fa5").await;
        let app = f.env.app.clone();
        let path = own_transcript(&f).await;
        let log = [claude_user("部署 6b84fa5"), claude_assistant("end_turn", json!([{"type": "text", "text": "部署完成：pid 42894"}]))].join("\n");
        std::fs::write(&path, log + "\n").unwrap();
        sqlx::query("UPDATE runs SET transcript_path = ? WHERE id = ?").bind(path.to_string_lossy()).bind(&f.run_id).execute(app.db()).await.unwrap();
        let run = db::run(app.db(), &f.run_id).await.unwrap().unwrap();
        let t = turn(&app, &f.turn_id).await;

        sqlx::query("ALTER TABLE projects RENAME TO projects_unreadable").execute(app.db()).await.unwrap();
        assert!(close_locked(&app, &run, &t, CloseWhy::Idle(6 * MIN)).await.is_err(), "讀不到主機是錯，不是「證不出來」");
        sqlx::query("ALTER TABLE projects_unreadable RENAME TO projects").execute(app.db()).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight", "這一輪不收");

        assert!(close_locked(&app, &run, &t, CloseWhy::Idle(6 * MIN)).await.unwrap(), "讀得到了：照常收");
        assert_eq!(turn(&app, &f.turn_id).await.status, "completed", "transcript 證得出來");
        let msgs = messages(&app, &f.turn_id).await;
        assert!(msgs.contains(&("assistant".into(), "transcript".into(), "部署完成：pid 42894".into())), "{msgs:?}");
    }

    /// working 或 blocked（等人回答）一律不收，就算計時早就超過；閃一下 working 就重算。
    #[tokio::test]
    async fn working_or_blocked_is_never_collected_and_a_blip_of_work_restarts_the_clock() {
        let f = stuck("部署").await;
        let app = f.env.app.clone();
        let t0 = Instant::now();
        observe_at(&f.run_id, "idle", t0);

        for status in ["working", "blocked"] {
            sqlx::query("UPDATE runs SET agent_status = ? WHERE id = ?").bind(status).bind(&f.run_id).execute(app.db()).await.unwrap();
            assert!(sweep_at(&app, None, t0 + 30 * MIN, 5 * MIN).await.is_empty(), "{status} 不收");
            assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight");
        }

        sqlx::query("UPDATE runs SET agent_status = 'idle' WHERE id = ?").bind(&f.run_id).execute(app.db()).await.unwrap();
        let back = t0 + 30 * MIN;
        observe_at(&f.run_id, "idle", back);
        assert!(sweep_at(&app, None, back + 4 * MIN, 5 * MIN).await.is_empty(), "重算後才四分鐘");
        observe_at(&f.run_id, "working", back + 4 * MIN);
        observe_at(&f.run_id, "idle", back + 4 * MIN + Duration::from_secs(10));
        assert!(sweep_at(&app, None, back + 6 * MIN, 5 * MIN).await.is_empty(), "閃過 working，從 4 分 10 秒重算");
        assert_eq!(sweep_at(&app, None, back + 10 * MIN, 5 * MIN).await.len(), 1);
    }

    /// 事件漏掉、只有 DB 的狀態變了（reconcile 寫的）：每輪取樣也要重算，不能接著舊的計時。
    #[tokio::test]
    async fn a_status_change_seen_only_in_the_database_restarts_the_clock_too() {
        let f = stuck("部署").await;
        let app = f.env.app.clone();
        let t0 = Instant::now();
        observe_at(&f.run_id, "idle", t0);
        let set = |status: &'static str| {
            let app = app.clone();
            let run = f.run_id.clone();
            async move {
                sqlx::query("UPDATE runs SET agent_status = ? WHERE id = ?").bind(status).bind(&run).execute(app.db()).await.unwrap();
            }
        };
        set("working").await;
        assert!(sweep_at(&app, None, t0 + 30 * MIN, 5 * MIN).await.is_empty());
        set("idle").await;
        assert!(sweep_at(&app, None, t0 + 31 * MIN, 5 * MIN).await.is_empty(), "這一輪看到 idle，從這裡起算");
        assert!(sweep_at(&app, None, t0 + 34 * MIN, 5 * MIN).await.is_empty(), "才三分鐘");
        assert_eq!(sweep_at(&app, None, t0 + 37 * MIN, 5 * MIN).await.len(), 1, "沒有事件也收得到");
    }

    /// 等 bot 鎖的時候那顆 bot 開始等人回答（blocked）：拿到鎖要重看一次，不收。
    #[tokio::test]
    async fn a_bot_that_starts_waiting_for_the_user_while_we_wait_for_its_lock_is_left_alone() {
        let f = stuck("部署").await;
        let app = f.env.app.clone();
        let t0 = Instant::now();
        observe_at(&f.run_id, "idle", t0);
        let lock = app.bot_lock(&f.bot_id).await;
        let held = lock.lock().await;
        let app2 = app.clone();
        let sweeping = tokio::spawn(async move { sweep_at(&app2, None, t0 + 6 * MIN, 5 * MIN).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        sqlx::query("UPDATE runs SET agent_status = 'blocked' WHERE id = ?").bind(&f.run_id).execute(app.db()).await.unwrap();
        drop(held);
        assert!(sweeping.await.unwrap().is_empty());
        assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight");
    }

    /// 還沒到門檻的 bot 不去排它的鎖：一顆正在忙著送 prompt 的 bot 不能把整輪掃描卡住。
    #[tokio::test]
    async fn a_bot_below_the_threshold_is_not_waited_on() {
        let f = stuck("部署").await;
        let app = f.env.app.clone();
        let t0 = Instant::now();
        observe_at(&f.run_id, "idle", t0);
        let lock = app.bot_lock(&f.bot_id).await;
        let _held = lock.lock().await;
        let done = tokio::time::timeout(Duration::from_secs(30), sweep_at(&app, None, t0 + MIN, 5 * MIN)).await;
        assert_eq!(done.ok(), Some(vec![]), "沒到門檻就直接略過，不等鎖");
        assert!(lock.try_lock().is_err(), "前提：鎖整段都還握著，sweep 不是因為拿到鎖才回來的");
    }

    /// 收尾後，被它擋住的 queued 馬上送出去。
    #[tokio::test]
    async fn closing_a_stuck_turn_flushes_the_prompt_queued_behind_it() {
        let f = stuck("部署").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", tt::LivePane { width: Some(120), ..Default::default() });
        let conv = db::conversation_id(app.db(), &f.bot_id).await.unwrap();
        let queued = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at)
             VALUES (?,?,'web','queued','pending','排在後面的那一則',?)",
        )
        .bind(&queued)
        .bind(&conv)
        .bind(db::now())
        .execute(app.db())
        .await
        .unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &queued).await.status, "queued", "卡住的 in_flight 擋著它");

        let t0 = Instant::now();
        observe_at(&f.run_id, "idle", t0);
        assert_eq!(sweep_at(&app, None, t0 + 6 * MIN, 5 * MIN).await.len(), 1);
        let q = turn(&app, &queued).await;
        assert_eq!(q.status, "in_flight", "收尾後同一把鎖裡就送出去了");
        assert_eq!(f.env.herdr.pane("pane-1").unwrap().transcript.iter().filter(|l| l.contains("排在後面的那一則")).count(), 1);
    }

    /// 只看指定的主機；遠端的 run 就算卡住，本機那一輪也不碰。
    #[tokio::test]
    async fn a_host_scoped_sweep_leaves_other_hosts_alone() {
        let f = stuck("部署").await;
        let app = f.env.app.clone();
        let t0 = Instant::now();
        observe_at(&f.run_id, "idle", t0);
        assert!(sweep_at(&app, Some("box"), t0 + 6 * MIN, 5 * MIN).await.is_empty());
        assert_eq!(sweep_at(&app, Some(LOCAL_HOST), t0 + 6 * MIN, 5 * MIN).await.len(), 1);
    }

    /// claude-api bot 沒有 pane／hook，其回合由 api_bot 自己的 turn_timeout／sweeper 管；
    /// stuck_turn 巡邏即便看到 run 閒置超過門檻，也絕不可將其 in_flight turn 關閉。
    #[tokio::test]
    async fn a_claude_api_bot_turn_is_never_closed_by_stuck_turn_sweep() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = db::ulid();
        let now = db::now();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, effort, fast, hook_token, created_at)
             VALUES (?, ?, 'api-stuck-test', 'claude-api', 'high', 0, 'tok', ?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(&now)
        .execute(app.db())
        .await
        .unwrap();

        let conv_id = db::ulid();
        sqlx::query("INSERT INTO conversations (id, bot_id, created_at) VALUES (?, ?, ?)")
            .bind(&conv_id)
            .bind(&bot_id)
            .bind(&now)
            .execute(app.db())
            .await
            .unwrap();

        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, started_at)
             VALUES (?, ?, 'running', 'idle', ?)",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .bind(&now)
        .execute(app.db())
        .await
        .unwrap();

        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at)
             VALUES (?, ?, ?, 'web', 'in_flight', 'ok', 'API bot prompt', ?)",
        )
        .bind(&turn_id)
        .bind(&conv_id)
        .bind(&run_id)
        .bind(db::iso_in(-600))
        .execute(app.db())
        .await
        .unwrap();

        sqlx::query("UPDATE runs SET agent_status = 'working' WHERE id = ?").bind(&run_id).execute(app.db()).await.unwrap();
        let t0 = Instant::now();
        let closed_working = sweep_at(&app, None, t0 + 10 * MIN, 5 * MIN).await;
        assert!(closed_working.is_empty(), "working 狀態的 claude-api bot 絕不應被 stuck_turn sweep 收尾");
        assert_eq!(turn(&app, &turn_id).await.status, "in_flight");

        sqlx::query("UPDATE runs SET agent_status = 'idle' WHERE id = ?").bind(&run_id).execute(app.db()).await.unwrap();
        observe_at(&run_id, "idle", t0);

        let closed = sweep_at(&app, None, t0 + 10 * MIN, 5 * MIN).await;
        assert!(closed.is_empty(), "claude-api bot 不應被 stuck_turn sweep 收尾");
        assert_eq!(turn(&app, &turn_id).await.status, "in_flight", "turn 必須保持 in_flight");
    }
}
