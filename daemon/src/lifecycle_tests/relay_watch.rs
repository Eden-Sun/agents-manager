#![allow(unused_imports)]
//! relay_watch 的測試（#1862 L2-3e 自 crate 搬來）。
mod tests {
    use crate::lifecycle::relay_watch::test_seams::*;
    use std::time::{Duration, Instant};
    use std::collections::{HashMap, HashSet};
    use serde_json::{json, Value};
    use crate::lifecycle::relay_watch::*;
    use crate::lifecycle::*;
    use crate::lifecycle_tests::prelude::*;
        use crate::testing as tt;

    const TEXT: &str = "請再打一次臺產，看 errorCode 的值是多少";

    async fn fixture(name: &str) -> F {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, name).await;
        let run_id = tt::fake_run(&env.app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        env.herdr.tabs.lock().unwrap().push(tt::MockTab { tab_id: format!("tab-{}", bot.id), workspace_id: "ws-1".into(), label: "t".into(), panes: vec![pane.clone()] });
        F { env, bot_id: bot.id, run_id, pane }
    }

    fn close_pane(f: &F) {
        f.env.herdr.tabs.lock().unwrap().clear();
    }

    async fn run_state(f: &F) -> String {
        sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(&f.run_id).fetch_one(f.env.app.db()).await.unwrap()
    }

    struct F {
        env: tt::Env,
        bot_id: String,
        run_id: String,
        pane: String,
    }

    fn watch(f: &F, turn_id: Option<String>, at: Instant) -> Watch {
        Watch { run_id: f.run_id.clone(), bot_id: f.bot_id.clone(), text: TEXT.into(), turn_id, started: at, may_fail: true, held_since: None, nudges: 0, snapshot_at_nudge: None, _registered: None }
    }

    fn keys_sent(f: &F) -> usize {
        f.env.herdr.calls.lock().unwrap().iter().filter(|(m, _)| m == "pane.send_keys").count()
    }

    async fn status_of(f: &F, turn: &str) -> String {
        sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(turn).fetch_one(f.env.app.db()).await.unwrap()
    }

    #[tokio::test]
    async fn nr_relay_watch_uses_the_run_after_rename_and_name_reuse() {
        let f = fixture("nr-relay-stable").await;
        let app = f.env.app.clone();
        let sender = tt::claude_bot(&app, &f.env.project_id, "nr-relay-sender").await;
        sqlx::query("UPDATE runs SET agent_name='renamed' WHERE id=?").bind(&f.run_id).execute(app.db()).await.unwrap();
        let other = tt::claude_bot(&app, &f.env.project_id, "nr-relay-other").await;
        tt::fake_run(&app, &other.id).await;
        assert_eq!(resolve(&app, &sender.id, &f.run_id).await.unwrap().map(|r| r.id), Some(f.run_id.clone()));
        assert!(resolve(&app, &sender.id, "agent").await.unwrap().is_none());
    }

    /// #927：送出時間落在回合存續區間內，選當時最新的回合。
    #[tokio::test]
    async fn the_sender_turn_is_the_one_running_when_it_sent() {
        let f = fixture("sender-turn").await;
        let app = f.env.app.clone();
        let sender = tt::claude_bot(&app, &f.env.project_id, "sender-turn-child").await;
        let conv = db::conversation_id(app.db(), &sender.id).await.unwrap();
        for (id, status, created, completed) in [
            ("turn-old", "completed", "2026-10-09T10:00:00.000Z", Some("2026-10-09T10:05:00.000Z")),
            ("turn-open", "in_flight", "2026-10-09T10:10:00.000Z", None),
        ] {
            sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at, completed_at) VALUES (?,?,'web',?,'ok',?,?)")
                .bind(id)
                .bind(&conv)
                .bind(status)
                .bind(created)
                .bind(completed)
                .execute(app.db())
                .await
                .unwrap();
        }
        assert_eq!(sender_turn_at(&app, &sender.id, "2026-10-09T10:02:00.000Z").await.unwrap().as_deref(), Some("turn-old"));
        assert_eq!(sender_turn_at(&app, &sender.id, "2026-10-09T10:20:00.000Z").await.unwrap().as_deref(), Some("turn-open"));
        assert_eq!(sender_turn_at(&app, &sender.id, "2026-10-09T10:07:00.000Z").await.unwrap(), None, "兩回合之間沒有在跑的回合");
    }

    /// #1365：child 忙著時又被派一則，排成 queued（`created_at` 比正在跑的那一筆新）。它還沒開始跑，不是寄件當下正在跑的回合；
    /// 遠端報備晚到、排隊的那一則那時已經在跑，才以它開跑的時間為準。
    #[tokio::test]
    async fn a_queued_turn_is_never_the_senders_running_turn() {
        let f = fixture("sender-queued").await;
        let app = f.env.app.clone();
        let sender = tt::claude_bot(&app, &f.env.project_id, "sender-queued-child").await;
        let conv = db::conversation_id(app.db(), &sender.id).await.unwrap();
        for (id, status, created) in [("turn-running", "in_flight", "2026-10-09T10:00:00.000Z"), ("turn-queued", "queued", "2026-10-09T10:02:00.000Z")] {
            sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at) VALUES (?,?,'web',?,'ok',?)")
                .bind(id)
                .bind(&conv)
                .bind(status)
                .bind(created)
                .execute(app.db())
                .await
                .unwrap();
        }
        assert_eq!(sender_turn_at(&app, &sender.id, "2026-10-09T10:04:00.000Z").await.unwrap().as_deref(), Some("turn-running"), "排隊中的回合還沒開始跑");

        sqlx::query("UPDATE turns SET status = 'completed', completed_at = '2026-10-09T10:05:00.000Z' WHERE id = 'turn-running'").execute(app.db()).await.unwrap();
        sqlx::query("UPDATE turns SET status = 'in_flight', delivered_at = '2026-10-09T10:05:01.000Z' WHERE id = 'turn-queued'").execute(app.db()).await.unwrap();
        assert_eq!(sender_turn_at(&app, &sender.id, "2026-10-09T10:04:00.000Z").await.unwrap().as_deref(), Some("turn-running"), "報備當下還是 turn-running");
        assert_eq!(sender_turn_at(&app, &sender.id, "2026-10-09T10:06:00.000Z").await.unwrap().as_deref(), Some("turn-queued"), "之後才開跑的那一則");
    }

    #[tokio::test]
    async fn an_announce_opens_an_in_flight_turn_marked_with_its_sender() {
        let f = fixture("insurer-mt").await;
        let app = f.env.app.clone();
        let sender = tt::claude_bot(&app, &f.env.project_id, "console-fetures").await;
        let run = db::run(app.db(), &f.run_id).await.unwrap().unwrap();

        let tid = open_turn(&app, &run, &sender.id, None, TEXT).await.expect("開了回合");
        assert_eq!(status_of(&f, &tid).await, "in_flight");
        let (content, from): (String, Option<String>) =
            sqlx::query_as("SELECT content, relay_from FROM messages WHERE turn_id=? AND role='user'").bind(&tid).fetch_one(app.db()).await.unwrap();
        assert_eq!((content.as_str(), from.as_deref()), (TEXT, Some(sender.id.as_str())));
        assert!(open_turn(&app, &run, &sender.id, None, "第二句").await.is_none(), "已經有回合在飛：不重開");

        assert_eq!(resolve(&app, &sender.id, &f.run_id).await.unwrap().map(|r| r.id), Some(f.run_id.clone()), "run id 對得到");
        assert!(resolve(&app, &sender.id, &f.pane).await.unwrap().is_none(), "pane 地址本身不是 run 身份");
        assert!(resolve(&app, &sender.id, "沒有這個人").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_relay_watch_is_tracked_and_stops_at_graceful_shutdown() {
        let f = fixture("tracked-relay-watch").await;
        let app = f.env.app.clone();
        let sender = tt::claude_bot(&app, &f.env.project_id, "tracked-relay-sender").await;
        let run = db::run(app.db(), &f.run_id).await.unwrap().unwrap();

        on_resolved_announce(&app, &sender.id, None, TEXT, Some(run)).await;
        assert_eq!(app.background_tasks.len(), 1, "the delivery watcher must belong to the daemon shutdown barrier");

        app.shutdown.cancel();
        app.background_tasks.close();
        assert!(GIVE_UP_AFTER > Duration::from_secs(30), "上限要比 watcher 自己放棄的時間短，才分得出『有看 shutdown』");
        tokio::time::timeout(Duration::from_secs(30), app.background_tasks.wait())
            .await
            .expect("a relay watcher must leave promptly at shutdown");
        assert_eq!(app.background_tasks.len(), 0);
    }

    #[tokio::test]
    async fn an_open_external_turn_rearms_its_composer_watch_after_restart() {
        let f = fixture("rearmed-relay-watch").await;
        let app = f.env.app.clone();
        let sender = tt::claude_bot(&app, &f.env.project_id, "rearmed-relay-sender").await;
        let run = db::run(app.db(), &f.run_id).await.unwrap().unwrap();
        let turn_id = open_turn(&app, &run, &sender.id, None, TEXT).await.expect("persist the external turn before shutdown");
        f.env.herdr.live_pane(&f.pane, tt::LivePane { no_spinner: true, zero_revision: true, composer: vec![TEXT.into()], width: Some(120), ..Default::default() });

        let mut restored = restore_open_watches(&app, "local").await.unwrap();
        assert_eq!(restored.len(), 1, "startup must reconstruct the open external turn from SQLite");
        let mut watch = restored.pop().unwrap();
        assert_eq!(watch.turn_id.as_deref(), Some(turn_id.as_str()));
        let t0 = Instant::now();
        assert_eq!(step(&app, &mut watch, t0).await, Step::Waiting, "first observe the restored composer");
        assert_eq!(step(&app, &mut watch, t0 + NUDGE_AFTER).await, Step::Nudged, "a prompt left in the composer must still receive Enter");
        assert_eq!(keys_sent(&f), 1);
    }

    /// #1202：重啟前早就開的交辦回合（超過 `GIVE_UP_AFTER`）：接回的盯梢期限到了只收工，不把它判成「沒送達」。
    #[tokio::test]
    async fn a_restored_watch_for_an_old_turn_never_fails_it_as_undelivered() {
        let f = fixture("old-relay-turn").await;
        let app = f.env.app.clone();
        let sender = tt::claude_bot(&app, &f.env.project_id, "old-relay-sender").await;
        let run = db::run(app.db(), &f.run_id).await.unwrap().unwrap();
        let tid = open_turn(&app, &run, &sender.id, None, TEXT).await.expect("開了回合");
        sqlx::query("UPDATE turns SET created_at = ? WHERE id = ?").bind(db::iso_in(-600)).bind(&tid).execute(app.db()).await.unwrap();
        f.env.herdr.live_pane(&f.pane, tt::LivePane { no_spinner: true, width: Some(120), ..Default::default() });

        let mut w = restore_open_watches(&app, "local").await.unwrap().pop().expect("還是接回來（字若還在輸入列要補 Enter）");
        let t0 = Instant::now();
        assert_eq!(step(&app, &mut w, t0 + GIVE_UP_AFTER + Duration::from_secs(1)).await, Step::Done, "期限到了只收工");
        assert_eq!(status_of(&f, &tid).await, "in_flight", "不判沒送達：留給 hook／stuck_turns 收");
        let notes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='system'").bind(&tid).fetch_one(app.db()).await.unwrap();
        assert_eq!(notes, 0);
    }

    /// #1202：剛報備完就重啟：期限從開回合那一刻接著算，不重新給 90 秒。
    #[tokio::test]
    async fn a_restored_watch_for_a_fresh_turn_keeps_its_original_deadline() {
        let f = fixture("fresh-relay-turn").await;
        let app = f.env.app.clone();
        let sender = tt::claude_bot(&app, &f.env.project_id, "fresh-relay-sender").await;
        let run = db::run(app.db(), &f.run_id).await.unwrap().unwrap();
        let tid = open_turn(&app, &run, &sender.id, None, TEXT).await.expect("開了回合");
        sqlx::query("UPDATE turns SET created_at = ? WHERE id = ?").bind(db::iso_in(-60)).bind(&tid).execute(app.db()).await.unwrap();
        f.env.herdr.live_pane(&f.pane, tt::LivePane { no_spinner: true, width: Some(120), ..Default::default() });

        let mut w = restore_open_watches(&app, "local").await.unwrap().pop().unwrap();
        let t0 = Instant::now();
        assert_eq!(step(&app, &mut w, t0 + Duration::from_secs(15)).await, Step::Waiting, "60+15 秒：還沒到 90");
        assert_eq!(step(&app, &mut w, t0 + Duration::from_secs(40)).await, Step::GaveUp, "60+40 秒：照原本的期限判沒送達");
        assert_eq!(status_of(&f, &tid).await, "failed");
    }

    /// #1366：主機每次重連都會 `rearm_host`；同一筆還在飛的回合不能再開第二條盯梢（補 Enter 的上限與停手都是每條各算）。
    /// 盯梢結束（關機收工）之後登記放掉，同一筆回合還在飛時下一次對帳才接得回來。
    #[tokio::test]
    async fn rearming_a_host_twice_does_not_double_watch_a_turn() {
        let f = fixture("rearm-twice").await;
        let app = f.env.app.clone();
        let sender = tt::claude_bot(&app, &f.env.project_id, "rearm-twice-sender").await;
        let run = db::run(app.db(), &f.run_id).await.unwrap().unwrap();
        open_turn(&app, &run, &sender.id, None, TEXT).await.expect("開了回合");

        rearm_host(&app, "local").await;
        rearm_host(&app, "local").await;
        assert_eq!(app.background_tasks.len(), 1, "同一筆回合只盯一條");

        app.shutdown.cancel();
        app.background_tasks.close();
        tokio::time::timeout(Duration::from_secs(30), app.background_tasks.wait()).await.expect("watchers leave promptly at shutdown");
        rearm_host(&app, "local").await;
        assert_eq!(app.background_tasks.len(), 1, "盯梢結束後登記放掉，下一次對帳重新接上");
        app.shutdown.cancel();
        app.background_tasks.close();
        tokio::time::timeout(Duration::from_secs(30), app.background_tasks.wait()).await.expect("watchers leave promptly at shutdown");
    }

    #[tokio::test]
    async fn resolve_uses_the_sender_host_and_session_and_rejects_bot_names() {
        let f = fixture("local-target").await;
        let app = f.env.app.clone();
        let sender = tt::claude_bot(&app, &f.env.project_id, "relay-sender").await;
        tt::fake_run(&app, &sender.id).await;

        let remote_project = db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, ?, 'remote', 'mac2', ?)")
            .bind(&remote_project)
            .bind(format!("{}/remote", f.env.dir.display()))
            .bind(db::now())
            .execute(app.db())
            .await
            .unwrap();
        let remote = tt::claude_bot(&app, &remote_project, "remote-collision").await;
        let remote_run = tt::fake_run(&app, &remote.id).await;
        sqlx::query("UPDATE runs SET pane_id = ?, started_at = '9999' WHERE id = ?")
            .bind(&f.pane)
            .bind(&remote_run)
            .execute(app.db())
            .await
            .unwrap();

        let other_session = tt::claude_bot(&app, &f.env.project_id, "session-collision").await;
        let other_session_run = tt::fake_run(&app, &other_session.id).await;
        sqlx::query("UPDATE runs SET pane_id = ?, herdr_session = 'default', started_at = '9998' WHERE id = ?")
            .bind(&f.pane)
            .bind(&other_session_run)
            .execute(app.db())
            .await
            .unwrap();

        assert_eq!(resolve(&app, &sender.id, &f.run_id).await.unwrap().map(|r| r.id), Some(f.run_id.clone()));
        assert!(resolve(&app, &sender.id, "local-target").await.unwrap().is_none(), "herdr does not resolve a bot's DB name");
    }

    /// #1363：收件方的專案已經移交給別台主機：這顆 daemon 不認領它（字由 shim 直接送進 pane，歸接手的 daemon 記），
    /// 不替它開回合、不盯 pane；移交清掉之後又認得到。
    #[tokio::test]
    async fn resolve_ignores_a_target_whose_project_was_handed_off() {
        let f = fixture("handoff-target").await;
        let app = f.env.app.clone();
        let sender = tt::claude_bot(&app, &f.env.project_id, "handoff-sender").await;
        assert_eq!(resolve(&app, &sender.id, &f.run_id).await.unwrap().map(|r| r.id), Some(f.run_id.clone()), "移交前：收件方在這顆 daemon 底下");

        sqlx::query("UPDATE projects SET handed_off_to = 'agm-host' WHERE id = ?").bind(&f.env.project_id).execute(app.db()).await.unwrap();
        assert!(resolve(&app, &sender.id, &f.run_id).await.unwrap().is_none(), "已移交：不是這顆 daemon 管的目標");
        let run = resolve(&app, &sender.id, &f.run_id).await.unwrap();
        on_resolved_announce(&app, &sender.id, None, TEXT, run).await;
        let external: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE origin='external'").fetch_one(app.db()).await.unwrap();
        assert_eq!(external, 0, "沒有替已移交的 bot 開回合");

        sqlx::query("UPDATE projects SET handed_off_to = NULL WHERE id = ?").bind(&f.env.project_id).execute(app.db()).await.unwrap();
        assert_eq!(resolve(&app, &sender.id, &f.run_id).await.unwrap().map(|r| r.id), Some(f.run_id.clone()), "移交清掉：又認得到");
    }

    /// #1460：`resolve` 因為移交而回 `None` 時，log 要認得出「這句是送給移交出去的 agent」（只給 log 用，不影響送出）。
    /// 沒移交、或名字不是它的 agent，都不算。
    #[tokio::test]
    async fn a_handed_off_target_is_recognised_for_the_handoff_log() {
        let f = fixture("handoff-log").await;
        let app = f.env.app.clone();
        let sender = tt::claude_bot(&app, &f.env.project_id, "handoff-log-sender").await;
        assert!(!handed_off_target(&app, &sender.id, &f.run_id).await.unwrap(), "移交前：本機的收件方不算移交");

        sqlx::query("UPDATE projects SET handed_off_to = 'agm-host' WHERE id = ?").bind(&f.env.project_id).execute(app.db()).await.unwrap();
        assert!(handed_off_target(&app, &sender.id, &f.run_id).await.unwrap(), "移交後：認得出是移交出去的 agent");
        assert!(!handed_off_target(&app, &sender.id, "no-such-agent").await.unwrap(), "不是它的 agent 不算");
    }

    #[tokio::test]
    async fn resolve_returns_none_when_multiple_runs_match_in_the_sender_scope() {
        let f = fixture("ambiguous-target").await;
        let app = f.env.app.clone();
        let sender = tt::claude_bot(&app, &f.env.project_id, "ambiguity-sender").await;
        tt::fake_run(&app, &sender.id).await;
        let second = tt::claude_bot(&app, &f.env.project_id, "ambiguous-second").await;
        let second_run = tt::fake_run(&app, &second.id).await;
        sqlx::query("UPDATE runs SET pane_id = ?, started_at = '9999' WHERE id = ?")
            .bind(&f.pane)
            .bind(&second_run)
            .execute(app.db())
            .await
            .unwrap();

        assert!(resolve(&app, &sender.id, &f.pane).await.unwrap().is_none(), "pane addresses alone are not recipient identity");
    }

    /// 2026-09-21 實機：字卡在輸入列、agent idle。超過 N 秒補一次 Enter，字進 transcript；沒到 N 秒不動。
    #[tokio::test]
    async fn a_prompt_left_in_the_composer_gets_its_enter_after_the_delay() {
        let f = fixture("stuck-enter").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane(&f.pane, tt::LivePane { no_spinner: true, zero_revision: true, composer: vec![TEXT.into()], width: Some(120), ..Default::default() });
        let t0 = Instant::now();
        let mut w = watch(&f, None, t0);

        assert_eq!(step(&app, &mut w, t0).await, Step::Waiting, "剛看到，先給 TUI 一點時間");
        assert_eq!(step(&app, &mut w, t0 + NUDGE_AFTER - Duration::from_millis(1)).await, Step::Waiting);
        assert_eq!(keys_sent(&f), 0);
        assert_eq!(step(&app, &mut w, t0 + NUDGE_AFTER).await, Step::Nudged);
        assert_eq!(keys_sent(&f), 1);
        assert_eq!(f.env.herdr.pane(&f.pane).unwrap().transcript.len(), 1, "字送出去了");
    }

    #[tokio::test]
    async fn nr1936_relay_never_nudges_a_working_agent_with_a_live_timer() {
        let f = fixture("working-timer").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane(&f.pane, tt::LivePane { zero_revision: true, composer: vec![TEXT.into()], width: Some(120), ..Default::default() });
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&f.run_id).execute(app.db()).await.unwrap();
        let t0 = Instant::now();
        let mut w = watch(&f, None, t0);
        assert_eq!(step(&app, &mut w, t0 + NUDGE_AFTER).await, Step::Done);
        assert_eq!(keys_sent(&f), 0);
    }

    #[tokio::test]
    async fn nr1913_zero_revision_nudge_rechecks_screen_and_run_before_enter() {
        for replace_run in [false, true] {
            let f = fixture("zero-race").await;
            let app = f.env.app.clone();
            f.env.herdr.live_pane(&f.pane, tt::LivePane { no_spinner: true, zero_revision: true, composer: vec![TEXT.into()], width: Some(120), ..Default::default() });
            let t0 = Instant::now();
            let mut w = watch(&f, None, t0);
            assert_eq!(step(&app, &mut w, t0).await, Step::Waiting);
            let live = f.env.herdr.live.clone();
            let pane = f.pane.clone();
            let pool = app.db().clone();
            let run = f.run_id.clone();
            crate::lifecycle::race_point::arm("relay_before_nudge", &f.bot_id, move || async move {
                if replace_run {
                    sqlx::query("UPDATE runs SET state='exited' WHERE id=?").bind(run).execute(&pool).await.unwrap();
                } else {
                    live.lock().unwrap().get_mut(&pane).unwrap().composer = vec!["user draft".into()];
                }
            });
            let result = step(&app, &mut w, t0 + NUDGE_AFTER).await;
            assert_eq!(result, if replace_run { Step::Done } else { Step::Waiting });
            assert_eq!(keys_sent(&f), 0);
        }
    }

    /// 使用者自己在 child 輸入列打的字不是宣告的那句：一個鍵都不按。
    #[tokio::test]
    async fn what_the_user_typed_is_never_submitted() {
        let f = fixture("user-typing").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane(&f.pane, tt::LivePane { no_spinner: true, composer: vec!["我自己正在打的另一段草稿文字".into()], width: Some(120), ..Default::default() });
        let t0 = Instant::now();
        let mut w = watch(&f, None, t0);
        for s in [0, 5, 30] {
            assert_eq!(step(&app, &mut w, t0 + Duration::from_secs(s)).await, Step::Waiting);
        }
        assert_eq!(keys_sent(&f), 0);
    }

    /// Enter 後完整畫面未變：保守地只按一次，不把 revision 相等當成送達證據；
    /// agent 開始 working 就收工。
    #[tokio::test]
    async fn an_enter_that_never_redraws_the_pane_is_not_retried_and_the_watch_ends_when_the_agent_works() {
        let f = fixture("swallowed").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane(&f.pane, tt::LivePane { no_spinner: true, zero_revision: true, composer: vec![TEXT.into()], swallow_enter: true, width: Some(120), ..Default::default() });
        let t0 = Instant::now();
        let mut w = watch(&f, None, t0);
        let mut at = t0;
        assert_eq!(step(&app, &mut w, at).await, Step::Waiting);
        at += NUDGE_AFTER;
        assert_eq!(step(&app, &mut w, at).await, Step::Nudged);
        for _ in 0..3 {
            at += NUDGE_AFTER;
            assert_eq!(step(&app, &mut w, at).await, Step::Waiting, "畫面沒變：不再按");
        }
        assert_eq!(keys_sent(&f), 1);

        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&f.run_id).execute(app.db()).await.unwrap();
        assert_eq!(step(&app, &mut w, at).await, Step::Done);
    }

    /// 字既沒進輸入列、agent 也沒接手：超過期限把回合標失敗並說明；agent 接手（回合被收）則收工。
    #[tokio::test]
    async fn an_undelivered_prompt_fails_its_turn_and_a_closed_turn_ends_the_watch() {
        let f = fixture("never-arrived").await;
        let app = f.env.app.clone();
        let sender = tt::claude_bot(&app, &f.env.project_id, "sender").await;
        let run = db::run(app.db(), &f.run_id).await.unwrap().unwrap();
        f.env.herdr.live_pane(&f.pane, tt::LivePane { no_spinner: true, width: Some(120), ..Default::default() });
        let tid = open_turn(&app, &run, &sender.id, None, TEXT).await.unwrap();
        let t0 = Instant::now();
        let mut w = watch(&f, Some(tid.clone()), t0);

        assert_eq!(step(&app, &mut w, t0 + GIVE_UP_AFTER - Duration::from_secs(1)).await, Step::Waiting);
        assert_eq!(status_of(&f, &tid).await, "in_flight");
        assert_eq!(step(&app, &mut w, t0 + GIVE_UP_AFTER).await, Step::GaveUp);
        assert_eq!(status_of(&f, &tid).await, "failed");
        let notes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='system'").bind(&tid).fetch_one(app.db()).await.unwrap();
        assert_eq!(notes, 1);
        assert_eq!(step(&app, &mut w, t0 + GIVE_UP_AFTER).await, Step::Done, "回合已經收了");
    }

    #[tokio::test]
    async fn give_up_needs_only_one_available_pool_connection() {
        let f = fixture("one-connection-give-up").await;
        let app = f.env.app.clone();
        let sender = tt::claude_bot(&app, &f.env.project_id, "sender").await;
        let run = db::run(app.db(), &f.run_id).await.unwrap().unwrap();
        let tid = open_turn(&app, &run, &sender.id, None, TEXT).await.unwrap();
        let w = watch(&f, Some(tid.clone()), Instant::now());
        let mut held = Vec::new();
        for _ in 1..app.db().options().get_max_connections() {
            held.push(app.db().acquire().await.unwrap());
        }
        assert_eq!(tokio::time::timeout(Duration::from_secs(2), give_up(&app, &w)).await
            .expect("give_up must not acquire a second connection inside its transaction"), Step::GaveUp);
        drop(held);
        assert_eq!(status_of(&f, &tid).await, "failed");
    }

    /// #380 實測：pane 早就 `pane_not_found`，run 卻還是 running。盯梢看到就收掉 run，一個鍵都不按。
    #[tokio::test]
    async fn a_watch_never_retries_against_a_pane_that_no_longer_exists() {
        let f = fixture("dead-child").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane(&f.pane, tt::LivePane { no_spinner: true, zero_revision: true, composer: vec![TEXT.into()], width: Some(120), ..Default::default() });
        close_pane(&f);
        let t0 = Instant::now();
        let mut w = watch(&f, None, t0);
        assert_eq!(step(&app, &mut w, t0 + NUDGE_AFTER * 2).await, Step::Done);
        assert_eq!(keys_sent(&f), 0, "死掉的 pane 不重試");
        assert_eq!(run_state(&f).await, "exited", "run 收掉，側欄不再畫成活的");
    }

    /// 一次暫時 DB read error 代表不知道，不是交辦已結束；DB 恢復後同一個 watcher 還要補 Enter。
    #[tokio::test]
    async fn a_transient_run_read_error_keeps_the_watch_alive() {
        let f = fixture("run-read-retry").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane(&f.pane, tt::LivePane { no_spinner: true, zero_revision: true, composer: vec![TEXT.into()], width: Some(120), ..Default::default() });
        let t0 = Instant::now();
        let mut w = watch(&f, None, t0);

        tt::make_table_unreadable(&app, "runs").await;
        assert_eq!(step(&app, &mut w, t0 + NUDGE_AFTER).await, Step::Waiting, "讀取錯誤不能結束 watcher");
        assert_eq!(keys_sent(&f), 0, "未知狀態時不可按鍵");
        tt::make_table_readable(&app, "runs").await;

        assert_eq!(step(&app, &mut w, t0 + NUDGE_AFTER).await, Step::Waiting, "恢復後先確認 composer");
        assert_eq!(step(&app, &mut w, t0 + NUDGE_AFTER * 2).await, Step::Nudged, "持續看到 composer 超過 NUDGE_AFTER 後補 Enter");
        assert_eq!(keys_sent(&f), 1);
    }

    /// in_flight_turn 讀取失敗也不能被當成「原回合已收掉」。
    #[tokio::test]
    async fn a_transient_in_flight_turn_read_error_keeps_the_watch_alive() {
        let f = fixture("turn-read-retry").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane(&f.pane, tt::LivePane { no_spinner: true, zero_revision: true, composer: vec![TEXT.into()], width: Some(120), ..Default::default() });
        let sender = tt::claude_bot(&app, &f.env.project_id, "turn-read-sender").await;
        let run = db::run(app.db(), &f.run_id).await.unwrap().unwrap();
        let turn_id = open_turn(&app, &run, &sender.id, None, TEXT).await.unwrap();
        let t0 = Instant::now();
        let mut w = watch(&f, Some(turn_id), t0);

        tt::make_table_unreadable(&app, "turns").await;
        assert_eq!(step(&app, &mut w, t0 + NUDGE_AFTER).await, Step::Waiting, "讀取錯誤不能被當成回合已收掉");
        assert_eq!(keys_sent(&f), 0, "未知狀態時不可按鍵");
        tt::make_table_readable(&app, "turns").await;

        assert_eq!(step(&app, &mut w, t0 + NUDGE_AFTER).await, Step::Waiting, "恢復後先確認原回合仍在飛");
        assert_eq!(step(&app, &mut w, t0 + NUDGE_AFTER * 2).await, Step::Nudged, "持續看到 composer 超過 NUDGE_AFTER 後補 Enter");
        assert_eq!(keys_sent(&f), 1);
    }

    /// bot row 讀不到不是 bot 已刪除；恢復後要繼續看同一個 composer。
    #[tokio::test]
    async fn a_transient_bot_read_error_keeps_the_watch_alive() {
        let f = fixture("bot-read-retry").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane(&f.pane, tt::LivePane { no_spinner: true, zero_revision: true, composer: vec![TEXT.into()], width: Some(120), ..Default::default() });
        let t0 = Instant::now();
        let mut w = watch(&f, None, t0);

        tt::make_table_unreadable(&app, "bots").await;
        assert_eq!(step(&app, &mut w, t0 + NUDGE_AFTER).await, Step::Waiting, "讀取錯誤不能結束 watcher");
        assert_eq!(keys_sent(&f), 0, "未知狀態時不可按鍵");
        tt::make_table_readable(&app, "bots").await;

        assert_eq!(step(&app, &mut w, t0 + NUDGE_AFTER).await, Step::Waiting, "恢復後先確認 composer");
        assert_eq!(step(&app, &mut w, t0 + NUDGE_AFTER * 2).await, Step::Nudged, "持續看到 composer 超過 NUDGE_AFTER 後補 Enter");
        assert_eq!(keys_sent(&f), 1);
    }

    /// 定時掃描：pane 明確不在的 running run 收成 exited；pane 還在的、herdr 維護中的都不動。
    #[tokio::test]
    async fn the_sweep_ends_only_runs_whose_pane_herdr_says_is_gone() {
        let alive = fixture("alive-child").await;
        assert!(crate::lifecycle::dead_panes::sweep(&alive.env.app).await.is_empty(), "pane 還在：不動");
        assert_eq!(run_state(&alive).await, "running");

        close_pane(&alive);
        assert_eq!(crate::lifecycle::dead_panes::sweep(&alive.env.app).await, vec![alive.run_id.clone()]);
        assert_eq!(run_state(&alive).await, "exited");
    }

    /// 舊 pane 消失後 herdr 把同一個 id 交給別的 agent：不能再對那個 id 打字，run 要收成 exited。
    #[tokio::test]
    async fn a_reused_pane_id_held_by_another_agent_is_not_the_old_run() {
        let f = fixture("reused-pane").await;
        sqlx::query("UPDATE runs SET native_session_id='original-session' WHERE id=?")
            .bind(&f.run_id).execute(&f.env.app.db).await.unwrap();
        f.env.herdr.set_agent("someone-else", &f.pane, true);
        let typed = crate::lifecycle::slash::send_text(&f.env.app, &f.bot_id, "hello", false, None).await;
        assert!(typed.is_err(), "打進被別人佔走的 pane id：{typed:?}");
        assert!(f.env.herdr.calls_to("pane.send_text").is_empty(), "一個字都不能進新 pane");
        assert_eq!(crate::lifecycle::dead_panes::sweep(&f.env.app).await, vec![f.run_id.clone()]);
        assert_eq!(run_state(&f).await, "exited");
    }

    /// #878：herdr 的 `agent start` 逾時後把名字從 pane 上拿掉，pane 與 CLI 還活著。沒有名字的 agent 不是「別人」，
    /// 定時掃描不能把這顆 run 收成 `pane gone`，打字也不能被當成打進別人的 pane。
    #[tokio::test]
    async fn a_pane_whose_agent_lost_its_herdr_name_is_still_the_run() {
        let f = fixture("nameless-pane").await;
        f.env.herdr.set_unnamed_agent(&f.pane, None);
        assert!(crate::lifecycle::dead_panes::sweep(&f.env.app).await.is_empty(), "只是沒有名字：pane 還在、不收");
        assert_eq!(run_state(&f).await, "running");
        let run = db::run(&f.env.app.db, &f.run_id).await.unwrap().unwrap();
        let client = f.env.app.herdr_for_run(&run).await.unwrap();
        assert!(!crate::lifecycle::dead_panes::pane_target_stale(&client, &run).await, "沒有名字的 agent 不算佔走 pane id");
    }

    /// 沒有名字時兩邊的 session 都知道、而且對不上，才是別的 agent 佔走了這個 id。
    #[tokio::test]
    async fn a_nameless_agent_with_another_session_is_not_the_run() {
        let f = fixture("nameless-other-session").await;
        sqlx::query("UPDATE runs SET native_session_id = 'sess-ours' WHERE id = ?").bind(&f.run_id).execute(&f.env.app.db).await.unwrap();
        f.env.herdr.set_unnamed_agent(&f.pane, Some("sess-theirs"));
        assert_eq!(crate::lifecycle::dead_panes::sweep(&f.env.app).await, vec![f.run_id.clone()]);

        let same = fixture("nameless-same-session").await;
        sqlx::query("UPDATE runs SET native_session_id = 'sess-ours' WHERE id = ?").bind(&same.run_id).execute(&same.env.app.db).await.unwrap();
        same.env.herdr.set_unnamed_agent(&same.pane, Some("sess-ours"));
        assert!(crate::lifecycle::dead_panes::sweep(&same.env.app).await.is_empty(), "同一個 session：還是我");
    }

    /// announce 給一顆 pane 已經不在的收件方：收掉 run、不開回合。
    #[tokio::test]
    async fn an_announce_to_a_dead_pane_opens_no_turn_and_ends_the_run() {
        let f = fixture("dead-target").await;
        let app = f.env.app.clone();
        let sender = tt::claude_bot(&app, &f.env.project_id, "dead-sender").await;
        close_pane(&f);
        let run = resolve(&app, &sender.id, &f.run_id).await.unwrap();
        on_resolved_announce(&app, &sender.id, None, TEXT, run).await;
        assert_eq!(run_state(&f).await, "exited");
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE run_id=?").bind(&f.run_id).fetch_one(app.db()).await.unwrap();
        assert_eq!(turns, 0);
    }
}
