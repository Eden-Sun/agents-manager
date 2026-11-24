#![allow(unused_imports)]
//! quota_hold 的測試（#1862 L2-3e 自 crate 搬來）。
mod tests {
    use crate::lifecycle::quota_hold::test_seams::*;
    use std::time::{Duration, Instant};
    use std::collections::{HashMap, HashSet};
    use serde_json::{json, Value};
    use crate::lifecycle::quota_hold::*;
    use crate::lifecycle::*;
    use crate::lifecycle_tests::prelude::*;
        use crate::testing as tt;

    /// 一顆 claude bot（身份 `identity`）、一個 run、一則排著的 prompt。
    async fn queued_on(identity: Option<&str>, name: &str) -> Queued {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, name).await;
        sqlx::query("UPDATE bots SET identity=? WHERE id=?").bind(identity).bind(&bot.id).execute(app.db()).await.unwrap();
        let bot = db::bot(app.db(), &bot.id).await.unwrap().unwrap();
        tt::fake_run(&app, &bot.id).await;
        let conv = db::conversation_id(app.db(), &bot.id).await.unwrap();
        let turn = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,'web','queued','pending','派工',?)")
            .bind(&turn)
            .bind(&conv)
            .bind(db::now())
            .execute(app.db())
            .await
            .unwrap();
        forget_queue_retry_timer(&bot.id);
        forget_held(&bot.id);
        Queued { env, bot, turn }
    }

    async fn flush(app: &Arc<App>, bot_id: &str) {
        forget_queue_retry_timer(bot_id);
        flush_queued_locked(app, bot_id).await.unwrap();
    }

    /// `(還排著而且沒花重試＝被額度擋, 身上的憑據)`。過了閘門的在測試裡沒有真的 pane，會被放回並花一次重試。
    async fn state(app: &Arc<App>, turn: &str) -> (bool, Option<Held>) {
        let (status, retries, raw): (String, i64, Option<String>) =
            sqlx::query_as("SELECT status, flush_retries, quota_hold FROM turns WHERE id=?").bind(turn).fetch_one(app.db()).await.unwrap();
        (status == "queued" && retries == 0, raw.map(|r| serde_json::from_str(&r).unwrap()))
    }

    fn later(hours: i64) -> String {
        db::iso_at(chrono::Utc::now() + chrono::Duration::hours(hours))
    }

    async fn put_hold(app: &Arc<App>, turn: &str, held: &Held) {
        sqlx::query("UPDATE turns SET quota_hold=? WHERE id=?").bind(serde_json::to_string(held).unwrap()).bind(turn).execute(app.db()).await.unwrap();
    }

    fn old_hold(identity: Option<&str>, until: Option<String>, bucket: Option<&str>) -> Held {
        Held {
            identity: identity.map(String::from),
            message: "You've hit your limit".into(),
            until,
            at: db::iso_at(chrono::Utc::now() - chrono::Duration::minutes(10)),
            bucket: bucket.map(String::from),
            held_at: db::iso_at(chrono::Utc::now() - chrono::Duration::minutes(5)),
            boot: "previous-boot".into(),
            target: None,
        }
    }

    async fn rename(app: &Arc<App>, from: &str, to: &str) {
        sqlx::query(&format!("ALTER TABLE {from} RENAME TO {to}")).execute(app.db()).await.unwrap();
    }

    fn at(iso: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(iso).unwrap().with_timezone(&chrono::Utc)
    }

    fn hit(until: Option<&str>) -> crate::quota::LimitHit {
        crate::quota::LimitHit { message: "You've hit your session limit".into(), until: until.map(String::from), at: db::now(), bucket: Some("five_hour".into()) }
    }

    struct Queued {
        env: tt::Env,
        bot: db::Bot,
        turn: String,
    }

    /// shell 的 cc0（沒有自己的 `CLAUDE_CONFIG_DIR`＝預設帳號）：撞限的 key 是裸 `claude`，身份偵測之前卻算成 `claude:cc0`。
    fn shell_cc0() -> crate::tools::HostTools {
        crate::tools::HostTools {
            tools: Default::default(),
            identities: Default::default(),
            shell_identities: vec![crate::config::IdentityCfg { name: "cc0".into(), kind: "claude".into(), host: None, env: Default::default(), args: vec![] }],
            utc_offset_secs: None, herdr_cli: None, checked_at: db::now(),
        }
    }

    /// 真的走 flush：目標身份撞限還在 → 不 claim、不花重試、一個字都不打、掛 timer；撞限解除 → 照常送。
    #[tokio::test]
    async fn a_queued_prompt_waits_while_its_identity_has_no_quota() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "no-quota").await;
        let run = tt::fake_run(&app, &bot.id).await;
        let conv = db::conversation_id(app.db(), &bot.id).await.unwrap();
        let turn = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,'web','queued','pending','派工',?)")
            .bind(&turn)
            .bind(&conv)
            .bind(db::now())
            .execute(app.db())
            .await
            .unwrap();
        let until = db::iso_at(chrono::Utc::now() + chrono::Duration::hours(2));
        assert!(crate::quota::seed_limit_hit(&app, LOCAL_HOST, "claude", &until, "You've hit your session limit", Some("five_hour".into())).await);
        forget_queue_retry_timer(&bot.id);
        forget_held(&bot.id);

        flush_queued_locked(&app, &bot.id).await.unwrap();
        let t: (String, i64, Option<String>) = sqlx::query_as("SELECT status, flush_retries, run_id FROM turns WHERE id=?").bind(&turn).fetch_one(app.db()).await.unwrap();
        assert_eq!(t, ("queued".to_string(), 0, None), "沒額度：不 claim、不花重試");
        assert!(!env.herdr.methods().iter().any(|m| m == "pane.send_text" || m == "agent.prompt"), "一個字都沒打");
        assert_eq!(queue_retry_timer_left(&bot.id).map(|d| d <= RECHECK_MAX), Some(true), "掛了 timer，最多五分鐘再看");
        assert!(recently_held(&bot.id));

        crate::quota::clear_limit_hit(&app, LOCAL_HOST, "claude").await;
        forget_queue_retry_timer(&bot.id);
        flush_queued_locked(&app, &bot.id).await.unwrap();
        let t: (String, i64) = sqlx::query_as("SELECT status, flush_retries FROM turns WHERE id=?").bind(&turn).fetch_one(app.db()).await.unwrap();
        assert!(t.0 != "queued" || t.1 > 0, "額度回來：往下送（這裡沒有真的 pane，會被放回並花一次重試）：{t:?}");
        let _ = run;
        forget_queue_retry_timer(&bot.id);
        forget_held(&bot.id);
    }

    /// #238：run 以 cc1 起來、設定已改成 cc2（還沒重啟）：排著的那一則會送進 cc1 的行程，照 cc1 的撞限擋，憑據也記 cc1——
    /// 記成 cc2 的話，重啟後回填拿它跟 run 的 cc1 比對不上，重啟前記下的撞限就被放掉。cc2 撞限不擋它（它還不在 cc2 上）。
    #[tokio::test]
    async fn a_queued_prompt_is_held_on_the_identity_its_run_started_with() {
        for (hit_on, held) in [("claude:cc1", true), ("claude:cc2", false)] {
            let q = queued_on(Some("cc2"), "switched").await;
            let app = q.env.app.clone();
            for name in ["cc1", "cc2"] {
                let dir = q.env.dir.join(format!("claude-{name}")).to_string_lossy().into_owned();
                app.cfg
                    .update(move |c| {
                        c.identities.push(crate::config::IdentityCfg {
                            name: name.into(),
                            kind: "claude".into(),
                            host: None,
                            env: [("CLAUDE_CONFIG_DIR".to_string(), dir)].into(),
                            args: vec![],
                        });
                        Ok(())
                    })
                    .await
                    .unwrap();
            }
            sqlx::query("UPDATE runs SET runtime_identity='cc1' WHERE bot_id=?").bind(&q.bot.id).execute(app.db()).await.unwrap();
            assert!(crate::quota::seed_limit_hit(&app, crate::config::LOCAL_HOST, hit_on, &later(2), "You've hit your session limit", Some("five_hour".into())).await);
            flush(&app, &q.bot.id).await;
            let (was_held, hold) = state(&app, &q.turn).await;
            assert_eq!(was_held, held, "{hit_on} 撞限：擋不擋");
            if held {
                assert_eq!(hold.and_then(|h| h.identity).as_deref(), Some("cc1"), "憑據記的是 run 的身份");
            }
        }
    }

    /// 重開留言那條序列，而且 key 在偵測前後不一樣：cc0 撞限（裸 `claude`）→ 排著的被擋、憑據寫在那一列 → 重啟
    /// → 偵測前開機叫醒的 flush 照樣擋 → 偵測完回填到裸 `claude`（不是沒人讀的 `claude:cc0`），帶著原本的撞限時刻與桶名
    /// → 記憶體接手：撞限被成功回合清掉就放行，憑據也清掉。
    #[tokio::test]
    async fn a_hold_from_the_previous_boot_holds_until_the_backfill_then_memory_takes_over() {
        let q = queued_on(Some("cc0"), "cc0-restart").await;
        let old = q.env.app.clone();
        crate::tools::install_host_tools(&old, LOCAL_HOST, shell_cc0()).await;
        let until = later(2);
        assert!(crate::quota::seed_limit_hit(&old, LOCAL_HOST, "claude", &until, "You've hit your session limit", Some("five_hour".into())).await);
        let hit_at = crate::runners::quota::limit_hit_for_bot(&old, &q.bot).await.unwrap().at;
        flush(&old, &q.bot.id).await;
        let (blocked, held) = state(&old, &q.turn).await;
        let held = held.expect("擋下的當下寫進那一列");
        assert!(blocked);
        assert_eq!((held.identity.as_deref(), held.until.as_deref(), held.at.as_str(), held.bucket.as_deref()), (Some("cc0"), Some(until.as_str()), hit_at.as_str(), Some("five_hour")));

        forget_held(&q.bot.id);
        let app = tt::restart_app(&q.env).await;
        assert!(crate::runners::quota::limit_hit_for_bot(&app, &q.bot).await.is_none(), "新行程：記憶體是空的");
        flush(&app, &q.bot.id).await;
        assert!(state(&app, &q.turn).await.0, "身份偵測之前：看那一列的憑據，照樣擋");
        assert!(recently_held(&q.bot.id), "保險絲看得到正在擋");
        assert!(!q.env.herdr.methods().iter().any(|m| m == "pane.send_text" || m == "agent.prompt"), "一個字都沒打");

        crate::tools::install_host_tools(&app, LOCAL_HOST, shell_cc0()).await;
        {
            let quotas = app.quotas.lock().await;
            let restored = quotas.get("claude").and_then(|x| x.limit_hit.clone()).expect("回填到裸 claude");
            assert_eq!((restored.until.as_deref(), restored.at.as_str(), restored.bucket.as_deref()), (Some(until.as_str()), hit_at.as_str(), Some("five_hour")), "原樣種回");
            assert!(quotas.get("claude:cc0").is_none(), "不能生出一格沒人讀的 claude:cc0");
        }
        assert!(crate::runners::quota::limit_hit_for_bot(&app, &q.bot).await.is_some(), "派送前也看得到");
        flush(&app, &q.bot.id).await;
        assert!(state(&app, &q.turn).await.0, "回填之後照樣擋");

        crate::quota::clear_limit_hit(&app, LOCAL_HOST, "claude").await;
        flush(&app, &q.bot.id).await;
        let (blocked, held) = state(&app, &q.turn).await;
        assert!(!blocked && held.is_none(), "記憶體說不擋：放行，憑據清掉");
        forget_queue_retry_timer(&q.bot.id);
        forget_held(&q.bot.id);
    }

    /// 上一輪的憑據在回填之前也照「現在」判：換了身份、換了模型、到期、之後同一把 key 有成功回合，都放行。
    /// 這一輪自己寫的憑據記憶體本來就有，記憶體說不擋就不擋。沒寫時間的（codex credits）照樣擋。
    #[tokio::test]
    async fn a_previous_boot_hold_does_not_outlive_a_switch_an_expiry_or_a_successful_turn() {
        let q = queued_on(Some("cc-a"), "old-hold").await;
        let app = q.env.app.clone();
        let cases: Vec<(&str, Held, Option<&str>, bool)> = vec![
            ("同一個身份、還沒到期", old_hold(Some("cc-a"), Some(later(2)), Some("five_hour")), None, true),
            ("沒寫時間（codex credits）", old_hold(Some("cc-a"), None, None), None, true),
            ("身份換掉了", old_hold(Some("cc-b"), Some(later(2)), None), None, false),
            ("到期了", old_hold(Some("cc-a"), Some(db::iso_at(chrono::Utc::now() - chrono::Duration::seconds(1))), None), None, false),
            ("撞的是 Fable 桶、現在跑 opus", old_hold(Some("cc-a"), Some(later(2)), Some("fable")), Some("opus"), false),
            ("撞的是 Fable 桶、還在跑 fable", old_hold(Some("cc-a"), Some(later(2)), Some("fable")), Some("fable"), true),
            ("這一輪自己寫的：記憶體說了算", Held { boot: app.boot_id.clone(), ..old_hold(Some("cc-a"), Some(later(2)), None) }, None, false),
        ];
        for (why, held, model, want) in cases {
            sqlx::query("UPDATE turns SET status='queued', run_id=NULL, flush_retries=0, next_flush_at=NULL WHERE id=?").bind(&q.turn).execute(app.db()).await.unwrap();
            sqlx::query("UPDATE bots SET model=? WHERE id=?").bind(model).bind(&q.bot.id).execute(app.db()).await.unwrap();
            put_hold(&app, &q.turn, &held).await;
            flush(&app, &q.bot.id).await;
            let (blocked, left) = state(&app, &q.turn).await;
            assert_eq!(blocked, want, "{why}");
            assert_eq!(left.is_some(), want, "{why}：放行就清掉憑據");
        }

        sqlx::query("UPDATE turns SET status='queued', run_id=NULL, flush_retries=0, next_flush_at=NULL WHERE id=?").bind(&q.turn).execute(app.db()).await.unwrap();
        sqlx::query("UPDATE bots SET model=NULL WHERE id=?").bind(&q.bot.id).execute(app.db()).await.unwrap();
        put_hold(&app, &q.turn, &old_hold(Some("cc-a"), Some(later(2)), None)).await;
        flush(&app, &q.bot.id).await;
        assert!(state(&app, &q.turn).await.0);
        let bot = db::bot(app.db(), &q.bot.id).await.unwrap().unwrap();
        crate::runners::quota::clear_limit_hit_for_bot(&app, &bot).await;
        flush(&app, &q.bot.id).await;
        assert!(!state(&app, &q.turn).await.0, "成功回合清過那把 key：放行");
        forget_queue_retry_timer(&q.bot.id);
        forget_held(&q.bot.id);
    }

    /// 回填每台主機每一輪開機只跑一次，跑完之後記憶體說了算；換了身份、之後有成功回合清過的不種；
    /// 沒寫時間的原樣黏著種回去。
    #[tokio::test]
    async fn the_backfill_runs_once_per_boot_and_skips_what_no_longer_holds() {
        let a = queued_on(Some("cc-a"), "bf-a").await;
        let app = a.env.app.clone();
        let add = |name: &'static str, identity: &'static str| {
            let app = app.clone();
            let pid = a.env.project_id.clone();
            async move {
                let bot = tt::claude_bot(&app, &pid, name).await;
                sqlx::query("UPDATE bots SET identity=? WHERE id=?").bind(identity).bind(&bot.id).execute(app.db()).await.unwrap();
                let conv = db::conversation_id(app.db(), &bot.id).await.unwrap();
                let turn = db::ulid();
                sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,'web','queued','pending','派工',?)")
                    .bind(&turn).bind(&conv).bind(db::now()).execute(app.db()).await.unwrap();
                (db::bot(app.db(), &bot.id).await.unwrap().unwrap(), turn)
            }
        };
        let (switched, switched_turn) = add("bf-switched", "cc-c").await;
        let (cleared, cleared_turn) = add("bf-cleared", "cc-d").await;
        put_hold(&app, &a.turn, &old_hold(Some("cc-a"), None, None)).await;
        put_hold(&app, &switched_turn, &old_hold(Some("cc-b"), Some(later(2)), None)).await;
        put_hold(&app, &cleared_turn, &old_hold(Some("cc-d"), Some(later(2)), None)).await;
        crate::runners::quota::clear_limit_hit_for_bot(&app, &cleared).await;

        backfill_once(&app, LOCAL_HOST).await;
        let hit = crate::runners::quota::limit_hit_for_bot(&app, &a.bot).await.expect("種回來了");
        assert_eq!(hit.until, None, "沒寫時間的照樣黏著");
        assert!(crate::runners::quota::limit_hit_for_bot(&app, &switched).await.is_none(), "身份換掉了：不種");
        assert!(crate::runners::quota::limit_hit_for_bot(&app, &cleared).await.is_none(), "之後有成功回合清過：不種");

        crate::runners::quota::clear_limit_hit_for_bot(&app, &a.bot).await;
        backfill_once(&app, LOCAL_HOST).await;
        assert!(crate::runners::quota::limit_hit_for_bot(&app, &a.bot).await.is_none(), "同一輪開機只回填一次，不把清掉的種回去");
        flush(&app, &a.bot.id).await;
        assert!(!state(&app, &a.turn).await.0, "回填跑完之後記憶體說了算");
        forget_queue_retry_timer(&a.bot.id);
        forget_held(&a.bot.id);
    }

    /// #347：撞限記錄帶著寫下時主機名指到的機器。重啟後回填（以及回填之前的 flush）只在那個名字**還指到同一台**時才算數：
    /// 單純重連照樣種回、照樣擋；改指到另一台（ssh 目標換了）就不種、不擋——那是舊機器上的帳號。
    #[tokio::test]
    async fn a_previous_boot_hold_only_counts_while_the_host_still_points_at_the_same_machine() {
        let env = tt::env().await;
        let app = env.app.clone();
        let cfg = |name: &str, ssh: &str| crate::config::HostCfg { daemon_url: None,
            shared_session: false,
            name: name.into(),
            ssh: ssh.into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        };
        let (same, moved) = ("qh-same-347", "qh-moved-347");
        let same_conn = app.hosts.insert_remote_for_test(cfg(same, "target-a")).await;
        app.hosts.insert_remote_for_test(cfg(moved, "target-a")).await;
        let queue = |host: &'static str, name: &'static str| {
            let app = app.clone();
            async move {
                let pid = db::ulid();
                sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, ?, ?, ?, ?)")
                    .bind(&pid).bind(format!("/x/{name}")).bind(name).bind(host).bind(db::now()).execute(app.db()).await.unwrap();
                let bot = tt::claude_bot(&app, &pid, name).await;
                sqlx::query("UPDATE bots SET identity='cc-a' WHERE id=?").bind(&bot.id).execute(app.db()).await.unwrap();
                let bot = db::bot(app.db(), &bot.id).await.unwrap().unwrap();
                tt::fake_run(&app, &bot.id).await;
                let conv = db::conversation_id(app.db(), &bot.id).await.unwrap();
                let turn = db::ulid();
                sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,'web','queued','pending','派工',?)")
                    .bind(&turn).bind(&conv).bind(db::now()).execute(app.db()).await.unwrap();
                stamp_queued(&app, &bot.id, Some("cc-a"), &hit(Some(&later(2)))).await.unwrap();
                let (_, held) = state(&app, &turn).await;
                put_hold(&app, &turn, &Held { boot: "previous-boot".into(), ..held.expect("蓋上了") }).await;
                forget_queue_retry_timer(&bot.id);
                forget_held(&bot.id);
                (bot, turn)
            }
        };
        let (same_bot, same_turn) = queue(same, "qh-same").await;
        let (flushed_bot, flushed_turn) = queue(moved, "qh-moved-flush").await;
        let (moved_bot, moved_turn) = queue(moved, "qh-moved").await;
        assert_eq!(state(&app, &same_turn).await.1.and_then(|h| h.target).as_deref(), Some("target-a:22/agents-manager"), "記下當時指到的機器");

        same_conn.bump_generation_for_test();
        app.hosts.replace_remote_for_test(&app, cfg(moved, "target-b")).await;

        assert!(blocking_hit(&app, &same_bot, &same_turn).await.is_some(), "重連：同一台的撞限照擋");
        assert!(blocking_hit(&app, &flushed_bot, &flushed_turn).await.is_none(), "改指到另一台：舊機器的撞限不擋");
        assert!(state(&app, &flushed_turn).await.1.is_none(), "放行就清掉那一列的憑據");
        assert!(state(&app, &moved_turn).await.1.is_some(), "另一顆的憑據還在，下面的回填才有東西可判");

        backfill_once(&app, same).await;
        backfill_once(&app, moved).await;
        assert!(crate::runners::quota::limit_hit_for_bot(&app, &same_bot).await.is_some(), "重連：種回來");
        assert!(crate::runners::quota::limit_hit_for_bot(&app, &moved_bot).await.is_none(), "改指到另一台：不種回新機器");
        for b in [&same_bot, &flushed_bot, &moved_bot] {
            forget_queue_retry_timer(&b.id);
            forget_held(&b.id);
        }
    }

    /// #108 重開：判斷擋不擋要的狀態讀不到，照擋、短時間後再看——不是「沒撞限」。
    /// (a) 讀不到主機（以前退回 `local`：查錯 key、回沒撞限、放行）；(b) 回填之前讀不到那一列自己帶的上一輪憑據
    /// （以前 `.ok()?` 當作沒有憑據、放行）。擋的時候有盡頭，保險絲不會把它當成「看不到盡頭的額度」撤掉。
    #[tokio::test]
    async fn a_queued_prompt_is_held_when_its_quota_state_cannot_be_read() {
        let q = queued_on(Some("cc-a"), "unreadable").await;
        let app = q.env.app.clone();
        put_hold(&app, &q.turn, &old_hold(Some("cc-a"), Some(later(2)), Some("five_hour"))).await;

        rename(&app, "projects", "projects_unreadable").await;
        let hit = blocking_hit(&app, &q.bot, &q.turn).await.expect("讀不到主機：照擋");
        assert!(bounded(&hit, chrono::Utc::now(), chrono::Duration::seconds(60)), "有盡頭：{hit:?}");
        flush(&app, &q.bot.id).await;
        assert!(state(&app, &q.turn).await.0, "不 claim、不花重試");
        assert!(queue_retry_timer_left(&q.bot.id).is_some_and(|d| d <= UNVERIFIED_RETRY), "很快再看一次");
        assert!(!q.env.herdr.methods().iter().any(|m| m == "pane.send_text" || m == "agent.prompt"), "一個字都沒打");
        rename(&app, "projects_unreadable", "projects").await;

        rename(&app, "turns", "turns_unreadable").await;
        assert!(blocking_hit(&app, &q.bot, &q.turn).await.is_some(), "讀不到那一列的憑據：照擋");
        rename(&app, "turns_unreadable", "turns").await;
        assert!(state(&app, &q.turn).await.1.is_some(), "憑據沒被當成放行清掉");

        sqlx::query("UPDATE turns SET quota_hold='not json' WHERE id=?").bind(&q.turn).execute(app.db()).await.unwrap();
        assert!(blocking_hit(&app, &q.bot, &q.turn).await.is_some(), "解不開：回填之前照擋");
        backfill_once(&app, LOCAL_HOST).await;
        assert!(blocking_hit(&app, &q.bot, &q.turn).await.is_some(), "解不開：回填之後照樣擋（#522）");
        let raw: Option<String> = sqlx::query_scalar("SELECT quota_hold FROM turns WHERE id=?").bind(&q.turn).fetch_one(app.db()).await.unwrap();
        assert_eq!(raw.as_deref(), Some("not json"), "唯一的證據不准被 forget 掉");
        forget_queue_retry_timer(&q.bot.id);
        forget_held(&q.bot.id);
    }

    /// 查撞限那一下讀不到主機、下一步又讀得到了（DB 剛恢復）：前一步的判斷根本沒做成，不能因為那一列上沒有憑據就放行——
    /// 記憶體裡明明有這個身份的撞限。
    #[tokio::test]
    async fn a_lookup_that_failed_is_not_mistaken_for_no_limit_when_the_db_recovers_mid_way() {
        let q = queued_on(Some("cc-a"), "recovered").await;
        let app = q.env.app.clone();
        assert!(crate::quota::seed_limit_hit(&app, LOCAL_HOST, "claude:cc-a", &later(2), "You've hit your session limit", Some("five_hour".into())).await);
        rename(&app, "projects", "projects_unreadable").await;
        let app2 = app.clone();
        crate::race_point::arm("quota_hold_after_lookup", &q.bot.id, move || async move {
            rename(&app2, "projects_unreadable", "projects").await;
        });
        assert!(blocking_hit(&app, &q.bot, &q.turn).await.is_some(), "照擋");
        assert!(blocking_hit(&app, &q.bot, &q.turn).await.is_some(), "恢復之後：記憶體的撞限擋");
        forget_queue_retry_timer(&q.bot.id);
        forget_held(&q.bot.id);
    }

    /// 憑據寫不進排著的那一列：這一輪記憶體照擋，但不算圍籬做完了——很快再寫一次（不是等五分鐘的重看），寫成為止。
    #[tokio::test]
    async fn a_hold_that_cannot_be_persisted_is_written_again_soon() {
        let q = queued_on(None, "unpersisted").await;
        let app = q.env.app.clone();
        assert!(crate::quota::seed_limit_hit(&app, LOCAL_HOST, "claude", &later(2), "You've hit your session limit", Some("five_hour".into())).await);
        sqlx::query("CREATE TRIGGER refuse_quota_hold BEFORE UPDATE OF quota_hold ON turns BEGIN SELECT RAISE(ABORT, 'injected'); END")
            .execute(app.db())
            .await
            .unwrap();
        flush(&app, &q.bot.id).await;
        let (blocked, held) = state(&app, &q.turn).await;
        assert!(blocked && held.is_none(), "照擋，憑據還沒落地");
        assert!(queue_retry_timer_left(&q.bot.id).is_some_and(|d| d <= PERSIST_RETRY), "很快再寫一次：{:?}", queue_retry_timer_left(&q.bot.id));

        sqlx::query("DROP TRIGGER refuse_quota_hold").execute(app.db()).await.unwrap();
        flush(&app, &q.bot.id).await;
        let (blocked, held) = state(&app, &q.turn).await;
        assert!(blocked && held.is_some(), "寫成了");
        assert!(queue_retry_timer_left(&q.bot.id).is_some_and(|d| d > PERSIST_RETRY), "寫成之後回到一般的重看節奏");
        forget_queue_retry_timer(&q.bot.id);
        forget_held(&q.bot.id);
    }

    /// 回填讀不到就不算回填過：以前讀不到那幾列當作沒有、照樣標成跑完，flush 從此只看空的記憶體——上一輪記下的撞限放行。
    #[tokio::test]
    async fn a_backfill_that_cannot_read_is_not_counted_as_done() {
        let q = queued_on(Some("cc-a"), "bf-unreadable").await;
        let app = q.env.app.clone();
        put_hold(&app, &q.turn, &old_hold(Some("cc-a"), Some(later(2)), None)).await;

        rename(&app, "conversations", "conversations_unreadable").await;
        backfill_once(&app, LOCAL_HOST).await;
        rename(&app, "conversations_unreadable", "conversations").await;
        assert!(!backfilled(&app, LOCAL_HOST), "沒讀到：不算回填過");
        assert!(crate::runners::quota::limit_hit_for_bot(&app, &q.bot).await.is_none());
        flush(&app, &q.bot.id).await;
        assert!(state(&app, &q.turn).await.0, "照那一列自己的憑據擋");

        backfill_once(&app, LOCAL_HOST).await;
        assert!(backfilled(&app, LOCAL_HOST), "讀得到了：再跑一次就跑完");
        assert!(crate::runners::quota::limit_hit_for_bot(&app, &q.bot).await.is_some(), "種回記憶體");
        forget_queue_retry_timer(&q.bot.id);
        forget_held(&q.bot.id);
    }
}
