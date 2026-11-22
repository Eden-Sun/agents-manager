#![allow(unused_imports)]

mod tests {
    use crate::lifecycle_tests::prelude::*;
    use crate::turn_error::test_seams::*;
    use am_ports::DbContext;

    /// 同一毫秒的兩個回合：「這個 run 的最後一回合」要看寫入順序，不是隨便一個（`created_at` 只到毫秒，id 的隨機段不遞增）。
    #[tokio::test]
    async fn the_last_turn_of_a_run_is_the_one_written_last_when_two_share_a_millisecond() {
        let e = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&e.app, &e.project_id, "last-turn-tie").await;
        let run = crate::testing::fake_run(&e.app, &bot.id).await;
        let conv = crate::db::conversation_id(e.app.db(), &bot.id).await.unwrap();
        for id in ["t-zzz-first", "t-aaa-second"] {
            sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','completed','ok','2026-10-01T00:00:00.000Z')")
                .bind(id)
                .bind(&conv)
                .bind(&run)
                .execute(e.app.db())
                .await
                .unwrap();
        }
        let db = DbContext::new(e.app.db().clone());
        let last = last_turn(&db, &run).await.unwrap().map(|t| t.id);
        assert_eq!(last.as_deref(), Some("t-aaa-second"));
    }
}

mod quota_limit_tests {
    use crate::lifecycle_tests::prelude::*;
    use crate::turn_error::*;

    const LIMIT: &str = "You've hit your session limit · resets 5pm";

    #[tokio::test]
    async fn an_agy_individual_quota_hit_marks_its_five_hour_window_full() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (mut bot, _) = bot_with_a_queued_prompt(&env, crate::config::LOCAL_HOST, None).await;
        bot.kind = "agy".into();
        let line = "Individual quota reached for Gemini Pro. Resets in 1h 30m";
        mark_agy_limit_hit(&app, &bot, line).await.unwrap();
        let q = app.quotas.lock().await.get("agy").cloned().expect("agy quota row");
        assert_eq!(q.five_hour.as_ref().map(|w| w.used_pct), Some(100.0));
        assert!(q.seven_day.is_none());
        let hit = q.limit_hit.expect("saved limit hit");
        assert_eq!(hit.bucket.as_deref(), Some("five_hour"));
        let at = chrono::DateTime::parse_from_rfc3339(&hit.at).unwrap();
        let until = chrono::DateTime::parse_from_rfc3339(hit.until.as_deref().expect("relative reset captured")).unwrap();
        assert_eq!((until - at).num_seconds(), 90 * 60);
    }

    /// 真畫面（折行）→ 擷取到的那一行 → 寫進 limit_hit：到期要用折行後的 `13m11s`，不是保底的 5h。
    #[tokio::test]
    async fn a_wrapped_agy_banner_on_screen_writes_its_real_reset_into_the_limit_hit() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (mut bot, _) = bot_with_a_queued_prompt(&env, crate::config::LOCAL_HOST, None).await;
        bot.kind = "agy".into();
        let screen = "❯ prompt\n⚠ Individual quota reached. Please upgrade your subscription to increase your limits. Resets in\n13m11s.\n─────\n❯\n";
        let line = api_error_line(screen).expect("折行的橫幅要認得出來");
        assert!(is_quota_limit(&line));
        mark_agy_limit_hit(&app, &bot, &line).await.unwrap();
        let q = app.quotas.lock().await.get("agy").cloned().expect("agy quota row");
        assert_eq!(q.five_hour.as_ref().map(|w| w.used_pct), Some(100.0));
        let hit = q.limit_hit.expect("saved limit hit");
        let at = chrono::DateTime::parse_from_rfc3339(&hit.at).unwrap();
        let until = chrono::DateTime::parse_from_rfc3339(hit.until.as_deref().expect("relative reset captured")).unwrap();
        assert_eq!((until - at).num_seconds(), 13 * 60 + 11);
    }

    /// 一顆 bot，對話裡排著一則。`host` 不是本機時建一個那台主機的專案。
    async fn bot_with_a_queued_prompt(env: &crate::testing::Env, host: &str, identity: Option<&str>) -> (db::Bot, String) {
        let app = env.app.clone();
        let pid = if host == crate::config::LOCAL_HOST {
            env.project_id.clone()
        } else {
            let pid = db::ulid();
            sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, '/r/p', 'r', ?, ?)")
                .bind(&pid)
                .bind(host)
                .bind(db::now())
                .execute(app.db())
                .await
                .unwrap();
            pid
        };
        let bot = crate::testing::claude_bot(&app, &pid, "limited").await;
        sqlx::query("UPDATE bots SET identity=? WHERE id=?").bind(identity).bind(&bot.id).execute(app.db()).await.unwrap();
        let bot = db::bot(app.db(), &bot.id).await.unwrap().unwrap();
        crate::testing::fake_run(&app, &bot.id).await;
        let conv = db::conversation_id(app.db(), &bot.id).await.unwrap();
        let turn = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,'web','queued','pending','派工',?)")
            .bind(&turn)
            .bind(&conv)
            .bind(db::now())
            .execute(app.db())
            .await
            .unwrap();
        (bot, turn)
    }

    async fn hits(app: &Arc<App>) -> Vec<(String, String)> {
        app.quotas.lock().await.iter().filter_map(|(k, q)| q.limit_hit.as_ref().map(|h| (k.clone(), h.at.clone()))).collect()
    }

    async fn hold_on(app: &Arc<App>, turn: &str) -> Option<serde_json::Value> {
        let raw: Option<String> = sqlx::query_scalar("SELECT quota_hold FROM turns WHERE id=?").bind(turn).fetch_one(app.db()).await.unwrap();
        raw.map(|r| serde_json::from_str(&r).unwrap())
    }

    /// #108 重開：遠端 bot 撞限，那一刻讀不到它在哪台主機。以前退回 `local`——撞限寫進本機帳號那一格，遠端那個用盡的
    /// 身份照樣被派工。現在回錯、哪一格都不寫、記成欠著：派送前與 flush 照欠著的那一筆擋；讀得到之後補進 `remote1/claude`，
    /// 撞限時刻是當初那一刻，排著的那一則也蓋上憑據。
    #[tokio::test]
    async fn a_limit_hit_whose_host_cannot_be_read_is_owed_and_never_lands_on_the_local_key() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (bot, queued) = bot_with_a_queued_prompt(&env, "remote1", None).await;

        sqlx::query("ALTER TABLE projects RENAME TO projects_unreadable").execute(app.db()).await.unwrap();
        assert!(mark_claude_limit_hit(&app, &bot, LIMIT).await.is_err(), "記不進去就是錯");
        assert_eq!(hits(&app).await, vec![], "哪一格都沒寫，尤其不是本機的 `claude`");
        assert!(owes_limit_hit(&app, &bot.id));
        let owed = crate::runners::quota::limit_hit_for_bot(&app, &bot).await.expect("欠著的那一筆照擋（派送前讀的就是這支）");
        assert_eq!(owed.bucket.as_deref(), Some("five_hour"));
        assert!(crate::runners::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some(), "flush 的閘門也一樣");
        assert!(hold_on(&app, &queued).await.is_none(), "還沒寫成");

        sqlx::query("ALTER TABLE projects_unreadable RENAME TO projects").execute(app.db()).await.unwrap();
        let hit = crate::runners::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().expect("補上之後照一般的查法擋");
        assert!(!owes_limit_hit(&app, &bot.id), "補上了");
        assert_eq!(hits(&app).await, vec![("remote1/claude".to_string(), owed.at.clone())], "寫進遠端那一格，撞限時刻不變");
        assert_eq!(hit.at, owed.at);
        let held = hold_on(&app, &queued).await.expect("排著的那一則蓋上了憑據");
        assert_eq!((held["at"].as_str(), held["bucket"].as_str()), (Some(owed.at.as_str()), Some("five_hour")));
    }

    /// 欠著一筆（5 小時窗）的時候，同一個身份又撞了一筆更新的（週窗）而且記進去了：舊的那筆一併結清，不會在之後被補寫、
    /// 把記憶體裡較新的那筆蓋回去。
    #[tokio::test]
    async fn a_newer_hit_recorded_for_the_same_identity_settles_the_older_owed_one() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (bot, _queued) = bot_with_a_queued_prompt(&env, crate::config::LOCAL_HOST, None).await;
        sqlx::query("ALTER TABLE projects RENAME TO projects_unreadable").execute(app.db()).await.unwrap();
        assert!(mark_claude_limit_hit(&app, &bot, LIMIT).await.is_err());
        sqlx::query("ALTER TABLE projects_unreadable RENAME TO projects").execute(app.db()).await.unwrap();
        mark_claude_limit_hit(&app, &bot, "You've hit your weekly limit · resets Sep 25").await.unwrap();
        assert!(!owes_limit_hit(&app, &bot.id), "較舊的那筆一併結清");
        let hit = crate::runners::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().expect("照擋");
        assert_eq!(hit.bucket.as_deref(), Some("seven_day"), "記憶體留著較新的那筆，沒被舊的蓋回去");
    }

    /// 那一桶的讀數已經過了重置時間：閒置的 5h 窗，`/usage` 探測照樣回上一個窗的重置時間（2026-09-19 正式資料
    /// `claude:cc1`：five_hour 0%、重置 −0.2h，同時週窗 97%）。撞限的到期不能拿這個過去的時間——一記下就算過期、被
    /// `quota::set` 丟掉，等於沒撞限，排著的與派工照送、再撞一次。
    /// 認不出桶名的橫幅（`You've hit your limit`）：5h 窗閒著，撞的只可能是週窗，到期借 7d 的重置時間；
    /// 說是 session 的：5h 窗的讀數過期了，照保底（撞的那一刻 +5 小時）。
    #[tokio::test]
    async fn a_limit_hit_does_not_take_its_expiry_from_a_window_that_already_reset() {
        let hours = |h: i64| db::iso_at(chrono::Utc::now() + chrono::Duration::hours(h));
        let week_reset = hours(100);
        let reading = || crate::quota::Quota {
            five_hour: Some(crate::quota::Window { observed_at: None, used_pct: 0.0, resets_at: Some(hours(-1)) }),
            seven_day: Some(crate::quota::Window { observed_at: None, used_pct: 97.0, resets_at: Some(week_reset.clone()) }),
            fable: None,
            argon_weekly: None,
            reset_credits: None,
            limit_hit: None,
            plan: None,
            updated_at: db::now(),
            source: "claude-usage".into(),
            account: None,
            host: crate::config::LOCAL_HOST.into(),
        };
        let later_than = |until: &Option<String>, h: i64| {
            let u = chrono::DateTime::parse_from_rfc3339(until.as_deref().expect("要有到期時間")).unwrap();
            u.with_timezone(&chrono::Utc) > chrono::Utc::now() + chrono::Duration::hours(h)
        };

        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (bot, queued) = bot_with_a_queued_prompt(&env, crate::config::LOCAL_HOST, None).await;
        crate::quota::set(&app, crate::config::LOCAL_HOST, "claude", reading()).await;
        mark_claude_limit_hit(&app, &bot, "You've hit your limit · resets 5pm (Asia/Taipei)").await.unwrap();
        let hit = crate::runners::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().expect("撞限要擋住，不能一記下就過期");
        assert_eq!(hit.until.as_deref(), Some(week_reset.as_str()), "5h 窗閒著：到期借 7d 的重置時間");
        let held = hold_on(&app, &queued).await.expect("排著的那一則蓋上了憑據");
        assert!(later_than(&held["until"].as_str().map(String::from), 99), "憑據上的到期也不能是過去：{held}");

        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (bot, _queued) = bot_with_a_queued_prompt(&env, crate::config::LOCAL_HOST, None).await;
        crate::quota::set(&app, crate::config::LOCAL_HOST, "claude", reading()).await;
        mark_claude_limit_hit(&app, &bot, LIMIT).await.unwrap();
        let hit = crate::runners::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().expect("session 撞限要擋住");
        assert!(later_than(&hit.until, 4), "5h 窗的讀數過期了：保底 5 小時，不是過去的時間：{hit:?}");
    }

    /// 身份表還沒偵測完（重啟後、`tools::detect` 之前）：共用預設帳號的 cc0 會被猜成 `claude:cc0`，偵測完之後查詢端讀裸
    /// `claude`，那一格沒人看。現在不猜：欠著，偵測完的第一次查詢補進裸 `claude`。
    #[tokio::test]
    async fn a_limit_hit_before_the_identities_are_known_is_owed_until_its_key_is() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (bot, _queued) = bot_with_a_queued_prompt(&env, crate::config::LOCAL_HOST, Some("cc0")).await;
        assert!(mark_claude_limit_hit(&app, &bot, LIMIT).await.is_err());
        assert_eq!(hits(&app).await, vec![], "沒有猜一格 `claude:cc0`");
        assert!(crate::runners::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some(), "欠著照擋");

        let cc0 = crate::config::IdentityCfg { name: "cc0".into(), kind: "claude".into(), host: None, env: Default::default(), args: vec![] };
        app.tools.lock().await.insert(
            crate::config::LOCAL_HOST.to_string(),
            crate::tools::HostTools { tools: Default::default(), identities: Default::default(), shell_identities: vec![cc0], utc_offset_secs: None, herdr_cli: None, checked_at: db::now() },
        );
        assert!(crate::runners::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some());
        assert!(!owes_limit_hit(&app, &bot.id));
        assert_eq!(hits(&app).await.into_iter().map(|(k, _)| k).collect::<Vec<_>>(), vec!["claude".to_string()], "補進裸 `claude`");
    }

    const CODEX_LIMIT: &str = "■ You've hit your usage limit. Upgrade to Pro, or try again at Sep 19th, 2099 6:43 PM.";

    async fn codex_bot_with_a_queued_prompt(env: &crate::testing::Env, host: &str, identity: Option<&str>) -> (db::Bot, String) {
        let (bot, queued) = bot_with_a_queued_prompt(env, host, identity).await;
        sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&bot.id).execute(env.app.db()).await.unwrap();
        (db::bot(env.app.db(), &bot.id).await.unwrap().unwrap(), queued)
    }

    async fn set_remote_offset(app: &Arc<App>, host: &str, offset: Option<i32>) {
        app.tools.lock().await.insert(
            host.to_string(),
            crate::tools::HostTools { tools: Default::default(), identities: Default::default(), shell_identities: vec![], utc_offset_secs: offset, herdr_cli: None, checked_at: db::now() },
        );
    }

    /// #239：橫幅的時間是遠端主機的當地時間。遠端 UTC−05:00、橫幅 6:43 PM → 23:43Z，不是照 daemon 的時區讀；
    /// 讀不到遠端偏移就不猜——到期是保底（約 5 小時後），不是照本機時區算出來的鐘點。
    #[tokio::test]
    async fn a_remote_codex_banner_is_read_in_the_remote_hosts_zone_and_never_guessed() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (bot, _q) = codex_bot_with_a_queued_prompt(&env, "remote1", None).await;
        set_remote_offset(&app, "remote1", Some(-5 * 3600)).await;
        mark_codex_limit_hit(&app, &bot, CODEX_LIMIT).await.unwrap();
        assert_eq!(crate::runners::quota::limit_hit_for_bot(&app, &bot).await.and_then(|h| h.until).as_deref(), Some("2099-09-19T23:43:00.000Z"));

        let (bot2, _q) = codex_bot_with_a_queued_prompt(&env, "remote2", None).await;
        set_remote_offset(&app, "remote2", None).await;
        mark_codex_limit_hit(&app, &bot2, CODEX_LIMIT).await.unwrap();
        let until = crate::runners::quota::limit_hit_for_bot(&app, &bot2).await.and_then(|h| h.until).expect("保底到期");
        assert!(chrono::DateTime::parse_from_rfc3339(&until).unwrap() < chrono::Utc::now() + chrono::Duration::hours(6), "保底約 5 小時，不是 2099 年的橫幅時間");
    }

    /// #198：遠端 codex bot 撞限，那一刻讀不到它在哪台主機。以前退回 `local`——撞限寫進本機 `codex` 那一格，本機帳號被當成
    /// 用盡、派工停擺，遠端那個真的用盡的身份反而照樣被派工；記不進去也沒有欠帳。現在哪一格都不寫、欠著：派送前與 flush
    /// 照欠著的那一筆擋；讀得到之後補進 `remote1/codex`，撞限時刻與橫幅上的時間不變，排著的那一則也蓋上憑據。
    #[tokio::test]
    async fn a_codex_limit_hit_whose_host_cannot_be_read_is_owed_and_never_lands_on_the_local_key() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (bot, queued) = codex_bot_with_a_queued_prompt(&env, "remote1", None).await;

        sqlx::query("ALTER TABLE projects RENAME TO projects_unreadable").execute(app.db()).await.unwrap();
        assert!(mark_codex_limit_hit(&app, &bot, CODEX_LIMIT).await.is_err(), "記不進去就是錯");
        assert_eq!(hits(&app).await, vec![], "哪一格都沒寫，尤其不是本機的 `codex`");
        assert!(owes_limit_hit(&app, &bot.id));
        let owed = crate::runners::quota::limit_hit_for_bot(&app, &bot).await.expect("欠著的那一筆照擋（派送前讀的就是這支）");
        assert!(crate::runners::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some(), "flush 的閘門也一樣");
        let soon = chrono::Utc::now() + chrono::Duration::hours(6);
        assert!(owed.until.as_deref().is_some_and(|u| chrono::DateTime::parse_from_rfc3339(u).unwrap() < soon), "讀不到主機就不猜橫幅的時區：保底到期（#239）：{owed:?}");
        assert!(hold_on(&app, &queued).await.is_none(), "還沒寫成");

        sqlx::query("ALTER TABLE projects_unreadable RENAME TO projects").execute(app.db()).await.unwrap();
        let hit = crate::runners::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().expect("補上之後照一般的查法擋");
        assert!(!owes_limit_hit(&app, &bot.id), "補上了");
        assert_eq!(hits(&app).await, vec![("remote1/codex".to_string(), owed.at.clone())], "寫進遠端那一格，撞限時刻不變");
        assert_eq!((hit.at.as_str(), hit.until.as_deref()), (owed.at.as_str(), owed.until.as_deref()));
        let held = hold_on(&app, &queued).await.expect("排著的那一則蓋上了憑據");
        assert_eq!(held["at"].as_str(), Some(owed.at.as_str()));
    }

    /// codex 也不猜 key：身份表還沒偵測完時，共用預設帳號的 `cx0` 會被算成 `codex:cx0`，偵測完之後查詢端讀裸 `codex`。
    #[tokio::test]
    async fn a_codex_limit_hit_before_the_identities_are_known_is_owed_until_its_key_is() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (bot, _queued) = codex_bot_with_a_queued_prompt(&env, crate::config::LOCAL_HOST, Some("cx0")).await;
        assert!(mark_codex_limit_hit(&app, &bot, CODEX_LIMIT).await.is_err());
        assert_eq!(hits(&app).await, vec![], "沒有猜一格 `codex:cx0`");
        assert!(crate::runners::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some(), "欠著照擋");

        let cx0 = crate::config::IdentityCfg { name: "cx0".into(), kind: "codex".into(), host: None, env: Default::default(), args: vec![] };
        app.tools.lock().await.insert(
            crate::config::LOCAL_HOST.to_string(),
            crate::tools::HostTools { tools: Default::default(), identities: Default::default(), shell_identities: vec![cx0], utc_offset_secs: None, herdr_cli: None, checked_at: db::now() },
        );
        assert!(crate::runners::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some());
        assert!(!owes_limit_hit(&app, &bot.id));
        assert_eq!(hits(&app).await.into_iter().map(|(k, _)| k).collect::<Vec<_>>(), vec!["codex".to_string()], "補進裸 `codex`");
    }

    /// #225：grok 撞週限記在 **grok** 自己那一格（6d5e74ab 讓 grok 走 `mark_codex_limit_hit`，`record` 卻把 kind 寫死成 `codex`：codex
    /// 被標成用盡、grok 那格沒事）。同一台主機上另有一顆 codex bot：它不能被擋，grok 自己要被擋；撞限當下排著的那一則蓋上憑據；
    /// `/api/quota` 的來源標 `grok-limit-hit`，不是 `codex-limit-hit`。
    #[tokio::test]
    async fn a_grok_limit_hit_lands_on_the_grok_key_not_codex() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (grok, queued) = bot_with_a_queued_prompt(&env, crate::config::LOCAL_HOST, None).await;
        sqlx::query("UPDATE bots SET kind='grok' WHERE id=?").bind(&grok.id).execute(app.db()).await.unwrap();
        let grok = db::bot(app.db(), &grok.id).await.unwrap().unwrap();
        let codex = crate::testing::claude_bot(&app, &env.project_id, "cx").await;
        sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&codex.id).execute(app.db()).await.unwrap();
        let codex = db::bot(app.db(), &codex.id).await.unwrap().unwrap();

        mark_codex_limit_hit(&app, &grok, "You hit your weekly limit.").await.unwrap();

        let keys: Vec<String> = hits(&app).await.into_iter().map(|(k, _)| k).collect();
        let codex_blocked = crate::runners::quota::limit_hit_for_bot(&app, &codex).await.is_some();
        let grok_blocked = crate::runners::quota::limit_hit_for_bot(&app, &grok).await.is_some();
        assert_eq!((keys, codex_blocked, grok_blocked), (vec!["grok".to_string()], false, true));
        assert_eq!(app.quotas.lock().await.get("grok").map(|q| q.source.clone()).as_deref(), Some("grok-limit-hit"));
        assert!(hold_on(&app, &queued).await.is_some(), "撞限當下排著的那一則蓋上憑據");
    }

    /// 同類（#198 的留言）：「同一則只記一次」的標記（`runs.turn_error`）先寫、訊息後寫的話，訊息寫不進去之後下一次擷取被
    /// 標記擋掉——錯誤沒釘上、回合不收（輸入框鎖著），再也不重來。現在標記、訊息、收回合同一個交易。
    #[tokio::test]
    async fn an_api_error_that_cannot_be_recorded_is_captured_again_next_time() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "cut").await;
        let run = crate::testing::fake_run(&app, &bot.id).await;
        let conv = db::conversation_id(app.db(), &bot.id).await.unwrap();
        let turn = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
            .bind(&turn)
            .bind(&conv)
            .bind(&run)
            .bind(db::now())
            .execute(app.db())
            .await
            .unwrap();
        env.herdr.set_screen(&format!("pane-{}", bot.id), "❯ 幫我改\n⏺ API Error: Connection lost mid-response.\n✻ done\n❯\n");
        sqlx::query(
            "CREATE TRIGGER refuse_note BEFORE INSERT ON messages WHEN NEW.role='system'
             BEGIN SELECT RAISE(ABORT, 'injected: cannot write the error note'); END",
        )
        .execute(app.db())
        .await
        .unwrap();
        assert!(capture(&app, &bot.id, &run).await.is_err());
        let status = |app: Arc<App>, t: String| async move {
            let s: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(t).fetch_one(app.db()).await.unwrap();
            s
        };
        assert_eq!(status(app.clone(), turn.clone()).await, "in_flight");

        sqlx::query("DROP TRIGGER refuse_note").execute(app.db()).await.unwrap();
        capture(&app, &bot.id, &run).await.unwrap();
        assert_eq!(status(app.clone(), turn.clone()).await, "failed", "寫得進去了：這一次照樣記、回合收掉");
        let notes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='system'").bind(&turn).fetch_one(app.db()).await.unwrap();
        assert_eq!(notes, 1);
        let marked: Option<String> = sqlx::query_scalar("SELECT turn_error FROM runs WHERE id=?").bind(&run).fetch_one(app.db()).await.unwrap();
        assert_eq!(marked.as_deref(), Some("API Error: Connection lost mid-response."));
    }

    /// #1673：codex 滿載收掉的回合要收成失敗、釘上人話原因；不記撞限（那會把排著的派工壓住）。
    #[tokio::test]
    async fn codex_model_at_capacity_fails_the_turn_with_a_plain_reason() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "codex-capacity").await;
        sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&bot.id).execute(app.db()).await.unwrap();
        let run = crate::testing::fake_run(&app, &bot.id).await;
        let conv = db::conversation_id(app.db(), &bot.id).await.unwrap();
        let turn = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
            .bind(&turn)
            .bind(&conv)
            .bind(&run)
            .bind(db::now())
            .execute(app.db())
            .await
            .unwrap();
        env.herdr.set_screen(
            &format!("pane-{}", bot.id),
            "› skill repo 先pull\n■ Selected model is at capacity. Please try a different model.\n\n›\n  gpt-6.1-sol medium · ~/project/agents-manager · 5h 82% left\n",
        );

        capture(&app, &bot.id, &run).await.unwrap();

        let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(&turn).fetch_one(app.db()).await.unwrap();
        assert_eq!(status, "failed");
        let reason = "模型目前滿載，請換模型或稍後再試（Selected model is at capacity. Please try a different model.）";
        let notes: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'").bind(&turn).fetch_all(app.db()).await.unwrap();
        assert_eq!(notes, vec![reason.to_string()]);
        let marked: Option<String> = sqlx::query_scalar("SELECT turn_error FROM runs WHERE id=?").bind(&run).fetch_one(app.db()).await.unwrap();
        assert_eq!(marked.as_deref(), Some(reason));
        assert!(crate::runners::quota::limit_hit_for_bot(&app, &bot).await.is_none(), "滿載不是額度撞限");
    }

    #[tokio::test]
    async fn a_failed_codex_turn_keeps_its_streamed_reply_as_incomplete() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "codex-partial").await;
        sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&bot.id).execute(app.db()).await.unwrap();
        let run = crate::testing::fake_run(&app, &bot.id).await;
        let conv = db::conversation_id(app.db(), &bot.id).await.unwrap();
        let turn = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at, prompt_text)
             VALUES (?,?,?,'web','in_flight','ok',?,'跑一下測試')",
        )
        .bind(&turn)
        .bind(&conv)
        .bind(&run)
        .bind(db::now())
        .execute(app.db())
        .await
        .unwrap();
        env.herdr.set_screen(
            &format!("pane-{}", bot.id),
            "› 跑一下測試\n\n• 已找到失敗位置。\n• API Error: Connection lost mid-response.\n",
        );

        capture(&app, &bot.id, &run).await.unwrap();

        let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(&turn).fetch_one(app.db()).await.unwrap();
        assert_eq!(status, "failed");
        let messages: Vec<(String, String, i64, String)> = sqlx::query_as(
            "SELECT role, content, incomplete, source FROM messages WHERE turn_id=? ORDER BY rowid",
        )
        .bind(&turn)
        .fetch_all(app.db())
        .await
        .unwrap();
        assert_eq!(
            messages,
            vec![
                ("assistant".into(), "已找到失敗位置。".into(), 1, "terminal_fallback".into()),
                ("system".into(), "API Error: Connection lost mid-response.".into(), 1, "system".into()),
            ],
        );
    }

    /// 撞限記下的當下就蓋到排著的那一則上（不等 flush）；蓋不上就回錯、欠著——記憶體那一格照樣寫了（照擋），
    /// 補的時候再蓋一次。
    #[tokio::test]
    async fn a_limit_hit_stamps_the_queued_prompts_at_once_or_stays_owed() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (bot, queued) = bot_with_a_queued_prompt(&env, crate::config::LOCAL_HOST, None).await;
        sqlx::query("CREATE TRIGGER refuse_quota_hold BEFORE UPDATE OF quota_hold ON turns BEGIN SELECT RAISE(ABORT, 'injected: cannot write turns.quota_hold'); END")
            .execute(app.db())
            .await
            .unwrap();
        assert!(mark_claude_limit_hit(&app, &bot, LIMIT).await.is_err(), "憑據沒落地不算記好了");
        assert!(owes_limit_hit(&app, &bot.id));
        assert_eq!(hits(&app).await.len(), 1, "記憶體那一格寫了：這一輪照擋");
        assert!(hold_on(&app, &queued).await.is_none());

        sqlx::query("DROP TRIGGER refuse_quota_hold").execute(app.db()).await.unwrap();
        assert!(crate::runners::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some());
        assert!(!owes_limit_hit(&app, &bot.id));
        let held = hold_on(&app, &queued).await.expect("補的時候蓋上了");
        assert_eq!(held["boot"].as_str(), Some(app.boot_id.as_str()));
        assert_eq!(held["identity"], serde_json::Value::Null);
    }

}
