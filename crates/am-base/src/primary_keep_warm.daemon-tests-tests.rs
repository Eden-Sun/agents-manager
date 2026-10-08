
    use super::*;
    use crate::testing as tt;

    async fn settle_skip(env: &tt::Env, bot: &db::Bot, run: &db::Run, now: chrono::DateTime<chrono::Utc>) {
        let db = DbContext::new(env.app.db.clone());
        let events = crate::lifecycle::app_ports_p4::AppEventSink::new(&env.app);
        settle_skip_with(&db, &events, bot, run, now).await;
    }

    #[test]
    fn thresholds_are_58_and_110_minutes() {
        assert_eq!((KEEP_WARM_AFTER_SECS, WARM_COMPACT_AFTER_SECS), (3480, 6600));
    }

    #[test]
    fn the_warm_compactions_own_activity_is_not_a_new_anchor() {
        let c = db::parse_ts("2026-10-04T12:00:00.000Z").unwrap();
        let at = |m: i64| c + chrono::Duration::minutes(m);
        assert!(is_warm_compact_echo(at(0), Some(c)));
        assert!(is_warm_compact_echo(at(3), Some(c)), "/compact 讓 statusLine 變、報 working");
        assert!(!is_warm_compact_echo(at(30), Some(c)), "之後真的有活動就重新計時");
        assert!(!is_warm_compact_echo(at(-120), Some(c)), "壓縮之前的錨點照常");
        assert!(!is_warm_compact_echo(at(0), None));
    }

    #[test]
    fn cold_cache_is_skipped_and_hot_cache_is_warm_compacted_with_margin() {
        let m = |min: i64| min * 60;
        let ttl = cache_clock::ttl_secs("claude").unwrap();
        assert_eq!(ttl, m(60));

        let ctx = Some(45.0);
        assert_eq!(decide(m(57), m(57), ttl, false, false, ctx, 30.0), Step::Wait);
        assert_eq!(decide(m(58), m(58), ttl, false, false, ctx, 30.0), Step::KeepWarm);
        assert_eq!(decide(m(59), m(59), ttl, false, false, ctx, 30.0), Step::KeepWarm, "TTL 到期前仍可保溫");
        assert_eq!(decide(m(60), m(60), ttl, false, false, ctx, 30.0), Step::Wait, "超過保溫視窗後 cache 已冷，不補送");
        assert_eq!(decide(m(80), m(80), ttl, false, false, ctx, 30.0), Step::Wait, "冷 cache 不保溫");

        assert_eq!(decide(m(110), m(110), ttl, true, false, ctx, 30.0), Step::Wait, "冷 cache 不壓縮");
        assert_eq!(decide(m(110), m(52), ttl, true, false, ctx, 30.0), Step::WarmCompact, "58 分保溫後，110 分時 cache 仍熱就壓縮");
        assert_eq!(decide(m(110), m(58), ttl, true, false, ctx, 30.0), Step::Wait, "壓縮前留兩分鐘 TTL 餘裕");
        assert_eq!(decide(m(24 * 60), m(24 * 60), ttl, false, false, ctx, 30.0), Step::Wait, "daemon 重啟後發現年齡很大的 cache 不碰");
        assert_eq!(decide(m(300), m(52), ttl, true, true, ctx, 30.0), Step::Wait, "同一個錨點只壓縮一次");
    }

    /// 2026-10-05：熱壓只在 context 用量超過門檻時才做（claude 30%、codex 50%）；不到或讀不到用量就讓它涼掉（也不再保溫）。
    #[test]
    fn warm_compact_needs_more_than_the_kinds_context_threshold() {
        let m = |min: i64| min * 60;
        let ttl = 3600;
        assert_eq!((WARM_COMPACT_MIN_CONTEXT_PCT_CLAUDE, WARM_COMPACT_MIN_CONTEXT_PCT_CODEX), (30.0, 50.0));
        assert_eq!(warm_compact_min_context_pct("claude"), Some(30.0));
        assert_eq!(warm_compact_min_context_pct("codex"), Some(50.0));
        assert_eq!(warm_compact_min_context_pct("grok"), None);

        // codex：> 50% 才壓；40%（claude 會壓）codex 不壓，剛好 50% 不壓，讀不到不壓。
        let codex = |pct: Option<f64>| decide(m(110), m(52), ttl, true, false, pct, WARM_COMPACT_MIN_CONTEXT_PCT_CODEX);
        assert_eq!(codex(Some(50.1)), Step::WarmCompact, "codex > 50% 照壓");
        assert_eq!(codex(Some(90.0)), Step::WarmCompact);
        assert_eq!(codex(Some(50.0)), Step::Wait, "剛好 50% 不壓（要超過）");
        assert_eq!(codex(Some(40.0)), Step::Wait, "codex 40% 不壓縮（同樣的用量 claude 會壓）");
        assert_eq!(codex(Some(30.1)), Step::Wait);
        assert_eq!(codex(None), Step::Wait, "codex 拿不到 context 保守不壓縮");
        assert_eq!(decide(m(110), m(52), ttl, true, false, Some(40.0), WARM_COMPACT_MIN_CONTEXT_PCT_CLAUDE), Step::WarmCompact, "claude 40% 照壓");
        // codex 到 110 分以後也不再保溫。
        assert_eq!(decide(m(115), m(57), ttl, false, false, Some(20.0), WARM_COMPACT_MIN_CONTEXT_PCT_CODEX), Step::Wait);

        let at = |pct: Option<f64>| decide(m(110), m(52), ttl, true, false, pct, WARM_COMPACT_MIN_CONTEXT_PCT_CLAUDE);
        assert_eq!(at(Some(30.1)), Step::WarmCompact, "> 30% 照壓");
        assert_eq!(at(Some(81.0)), Step::WarmCompact);
        assert_eq!(at(Some(30.0)), Step::Wait, "剛好 30% 不壓（要超過）");
        assert_eq!(at(Some(12.5)), Step::Wait, "≤ 30% 不壓縮");
        assert_eq!(at(None), Step::Wait, "拿不到 context 保守不壓縮");
        // 不壓縮之後也不保溫：110 分以後一律等，直到真的活動重新計時。
        for min in [110, 115, 120, 200] {
            assert_eq!(decide(m(min), m(min - 58), ttl, true, false, Some(10.0), 30.0), Step::Wait, "{min} 分");
            assert_eq!(decide(m(min), m(min - 58), ttl, false, false, Some(10.0), 30.0), Step::Wait, "沒保溫過也不補保溫：{min} 分");
        }
        // 58 分的保溫不看 context。
        assert_eq!(decide(m(58), m(58), ttl, false, false, Some(5.0), 30.0), Step::KeepWarm);
        assert_eq!(decide(m(58), m(58), ttl, false, false, None, 30.0), Step::KeepWarm);
    }

    #[test]
    fn context_used_pct_comes_from_the_statusline_for_claude() {
        let sj = |p: &str| format!(r#"{{"context_window":{{"used_percentage":{p},"context_window_size":1000000}}}}"#);
        assert_eq!(context_used_pct("claude", "r1", Some(&sj("45"))), Some(45.0));
        assert_eq!(context_used_pct("claude", "r1", Some(&sj("12.5"))), Some(12.5));
        assert_eq!(context_used_pct("claude", "r1", Some(&sj("null"))), None, "剛開的 session 用量是 null");
        assert_eq!(context_used_pct("claude", "r1", Some("{}")), None);
        assert_eq!(context_used_pct("claude", "r1", Some("壞掉")), None);
        assert_eq!(context_used_pct("claude", "r1", None), None);
        assert_eq!(context_used_pct("grok", "r1", Some(&sj("90"))), None, "只有 claude／codex");
        assert_eq!(context_used_pct("codex", "r-no-rollout", Some(&sj("90"))), None, "codex 不看 statusLine，沒讀到 rollout 就是 None");
    }

    #[test]
    fn only_an_idle_primary_claude_or_codex_with_nothing_pending_is_touched() {
        assert!(eligible("claude", true, "running", "idle", false, false));
        assert!(eligible("codex", true, "running", "idle", false, false));
        assert!(!eligible("grok", true, "running", "idle", false, false));
        assert!(!eligible("agy", true, "running", "idle", false, false));
        assert!(!eligible("claude", false, "running", "idle", false, false), "不是主力");
        assert!(!eligible("claude", true, "stopping", "idle", false, false));
        for status in ["blocked", "working", "unknown"] {
            assert!(!eligible("claude", true, "running", status, false, false), "{status}：絕不送");
        }
        assert!(!eligible("claude", true, "running", "idle", true, false), "有回合在飛");
        assert!(!eligible("claude", true, "running", "idle", false, true), "有排隊的回合");
    }

    #[test]
    fn the_window_swallows_statusline_activity_until_it_settles() {
        let run = "r-keep-warm-window";
        let a = r#"{"cost":{"total_api_duration_ms":100},"context_window":{"total_output_tokens":5}}"#;
        let b = r#"{"cost":{"total_api_duration_ms":180},"context_window":{"total_output_tokens":9}}"#;
        let c = r#"{"cost":{"total_api_duration_ms":260},"context_window":{"total_output_tokens":14}}"#;
        assert!(!window_open(run));
        open_window(run);
        assert!(window_open(run));
        assert!(!cache_clock::on_statusline(run, Some(a), Some(b), "2026-10-04T11:00:00.000Z"), "保溫造成的指紋變化不算活動");
        assert_eq!(cache_clock::statusline_at(run), None);
        // 太早（不到 WINDOW_MIN）不會被巡邏關掉；寬限過了才關。
        settle_window(run, true);
        assert!(window_open(run));
        age_window_opened_for_test(run, WINDOW_MIN);
        settle_window(run, false);
        assert!(window_open(run), "回合還在跑就不收");
        settle_window(run, true);
        assert!(window_open(run), "收尾後還有寬限");
        age_window_closed_for_test(run, WINDOW_GRACE);
        settle_window(run, true);
        assert!(!window_open(run));
        assert!(cache_clock::on_statusline(run, Some(b), Some(c), "2026-10-04T12:00:00.000Z"), "視窗關了，真的活動照算");
        // 視窗有最長期限。
        open_window(run);
        age_window_opened_for_test(run, WINDOW_MAX);
        assert!(!window_open(run));
        drop_window(run);
    }

    #[test]
    fn a_keep_warm_in_flight_does_not_reset_the_age() {
        let run = "r-keep-warm-annotate";
        let t = cache_clock::LastTurn { status: "completed".into(), completed_at: Some("2026-10-04T10:00:00.000Z".into()), kept_warm_at: None, ..Default::default() };
        let mut json = serde_json::json!({"id": run, "agent_status": "working"});
        open_window(run);
        cache_clock::annotate(&mut json, "claude", Some(&t));
        assert_eq!(json["last_api_at"], "2026-10-04T10:00:00.000Z", "保溫回合在跑：年齡照真實的算");
        // 真的有使用者回合在飛（last_turn 是 in_flight）就是熱的。
        let live = cache_clock::LastTurn { status: "in_flight".into(), completed_at: None, kept_warm_at: None, ..Default::default() };
        let mut json = serde_json::json!({"id": run, "agent_status": "working"});
        cache_clock::annotate(&mut json, "claude", Some(&live));
        assert_ne!(json["last_api_at"], "2026-10-04T10:00:00.000Z");
        drop_window(run);
    }

    async fn primary(env: &tt::Env, name: &str, agent_status: &str) -> (db::Bot, db::Run) {
        let bot = tt::claude_bot(&env.app, &env.project_id, name).await;
        sqlx::query("UPDATE bots SET is_primary = 1 WHERE id = ?").bind(&bot.id).execute(&env.app.db).await.unwrap();
        let run_id = tt::fake_run(&env.app, &bot.id).await;
        sqlx::query("UPDATE runs SET agent_status = ? WHERE id = ?").bind(agent_status).bind(&run_id).execute(&env.app.db).await.unwrap();
        let bot = db::bot(&env.app.db, &bot.id).await.unwrap().unwrap();
        let run = db::active_run(&env.app.db, &bot.id).await.unwrap().unwrap();
        (bot, run)
    }

    async fn turn(env: &tt::Env, bot: &db::Bot, id: &str, crid: Option<&str>, status: &str, created: &str, completed: Option<&str>) {
        let conv = db::conversation_id(&env.app.db, &bot.id).await.unwrap();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, origin, status, delivery, client_request_id, created_at, completed_at)
             VALUES (?,?, 'web', ?, 'ok', ?, ?, ?)",
        )
        .bind(id)
        .bind(&conv)
        .bind(status)
        .bind(crid)
        .bind(created)
        .bind(completed)
        .execute(&env.app.db)
        .await
        .unwrap();
    }

    fn at(min_ago: i64, now: chrono::DateTime<chrono::Utc>) -> String {
        db::iso_at(now - chrono::Duration::minutes(min_ago))
    }

    /// 錨點＝最後一筆真的回合；保溫回合（持久的 `keepalive:` 前綴）不算，保溫過就不再續，壓縮靠系統訊息記號。
    #[tokio::test]
    async fn plan_reads_the_anchor_from_real_turns_only_and_remembers_what_it_did() {
        let env = tt::env().await;
        let now = chrono::Utc::now();
        let (bot, run) = primary(&env, "kw-plan", "idle").await;

        // 沒有任何活動紀錄：不編時間。
        assert_eq!(plan(&env.app, &bot, &run, now).await.unwrap(), None);

        // 59 分鐘前的真回合 → cache 還熱，該保溫。
        turn(&env, &bot, "t-real", None, "completed", &at(64, now), Some(&at(59, now))).await;
        let p = plan(&env.app, &bot, &run, now).await.unwrap().unwrap();
        assert_eq!((p.step, p.anchor.as_str()), (Step::KeepWarm, at(59, now).as_str()));
        assert_eq!(keep_warm_crid(&p.anchor), format!("keep-warm:{}", at(59, now)));

        // 保溫回合剛跑完：年齡仍從真回合算（不被重置），而且不再續。
        let kept_at = db::iso_at(now);
        turn(&env, &bot, "t-keep", Some(&keep_warm_crid(&p.anchor)), "completed", &kept_at, Some(&kept_at)).await;
        let after = plan(&env.app, &bot, &run, now).await.unwrap().unwrap();
        assert_eq!((after.step, after.anchor.as_str()), (Step::Wait, at(59, now).as_str()), "保溫回合不算活動，且同錨點只續一次");
        let last = cache_clock::last_turn_for_bot(&env.app.db, &bot.id).await.unwrap().unwrap();
        assert_eq!(last.completed_at.as_deref(), Some(at(59, now).as_str()));
        assert_eq!(last.kept_warm_at.as_deref(), Some(kept_at.as_str()), "顏色用的「保溫時間」來自保溫回合");

        // 同一錨點到 110 分鐘：context 用量 > 30% 才壓縮（拿不到、≤ 30% 都不壓）；記號一寫就不再壓縮。
        let later = now + chrono::Duration::minutes(52);
        let p = plan(&env.app, &bot, &run, later).await.unwrap().unwrap();
        assert_eq!((p.step, p.age_secs / 60), (Step::Wait, 111), "run 還沒有 context 用量：保守不壓縮");
        let with_ctx = |pct: &str| format!(r#"{{"context_window":{{"used_percentage":{pct}}}}}"#);
        let set_ctx = |pct: String| {
            let (db, run_id) = (env.app.db.clone(), run.id.clone());
            async move { sqlx::query("UPDATE runs SET status_json = ? WHERE id = ?").bind(pct).bind(run_id).execute(&db).await.unwrap() }
        };
        set_ctx(with_ctx("30")).await;
        let low = db::active_run(&env.app.db, &bot.id).await.unwrap().unwrap();
        assert_eq!(plan(&env.app, &bot, &low, later).await.unwrap().unwrap().step, Step::Wait, "30% 不壓縮，讓它涼掉");
        set_ctx(with_ctx("45")).await;
        let run = db::active_run(&env.app.db, &bot.id).await.unwrap().unwrap();
        let p = plan(&env.app, &bot, &run, later).await.unwrap().unwrap();
        assert_eq!((p.step, p.age_secs / 60), (Step::WarmCompact, 111), "> 30% 照壓");
        let conv = db::conversation_id(&env.app.db, &bot.id).await.unwrap();
        let note = format!("{WARM_COMPACT_NOTE_PREFIX}cache 年齡已 111 分鐘，自動送出 /compact");
        crate::lifecycle::insert_message(&env.app, &conv, None, "system", &note, "system", false, None).await.unwrap();
        let p = plan(&env.app, &bot, &run, later + chrono::Duration::seconds(5)).await.unwrap().unwrap();
        assert_eq!(p.step, Step::Wait, "壓縮記號比錨點晚，同一錨點不再壓縮");
        // 熱壓後視為涼掉：不再保溫、也不再壓縮（錨點沒變、年齡繼續往上），而且熱壓不算讓 cache 變熱、熱壓前的保溫也不算。
        for min in [10, 30, 60, 300] {
            let t = later + chrono::Duration::minutes(min);
            assert_eq!(plan(&env.app, &bot, &run, t).await.unwrap().map(|p| p.step), Some(Step::Wait), "熱壓後 {min} 分");
        }
        let last = cache_clock::last_turn_for_bot(&env.app.db, &bot.id).await.unwrap().unwrap();
        assert_eq!(last.kept_warm_at, None, "熱壓後 cache_kept_warm_at 不帶：晶片顯示涼");
        let mut json = serde_json::json!({"id": run.id, "agent_status": "idle"});
        cache_clock::annotate(&mut json, "claude", Some(&last));
        assert!(json["cache_kept_warm_at"].is_null());
        assert_eq!(json["last_api_at"], at(59, now), "真實年齡照舊；web 以 max(last_api_at, cache_kept_warm_at) 起算＝已超過 TTL＝涼");

        // 真的新活動（使用者回合）→ 錨點更新、計數重來：58 分後重新保溫，新的保溫時間又算讓 cache 變熱。
        let fresh = later + chrono::Duration::minutes(20);
        turn(&env, &bot, "t-user", None, "completed", &db::iso_at(fresh), Some(&db::iso_at(fresh))).await;
        assert_eq!(plan(&env.app, &bot, &run, fresh + chrono::Duration::minutes(1)).await.unwrap(), None);
        let p = plan(&env.app, &bot, &run, fresh + chrono::Duration::minutes(58)).await.unwrap().unwrap();
        assert_eq!((p.step, p.anchor.as_str()), (Step::KeepWarm, db::iso_at(fresh).as_str()), "活動後重新計時");
        let kw_at = fresh + chrono::Duration::minutes(58);
        turn(&env, &bot, "t-kw2", Some(&keep_warm_crid(&p.anchor)), "completed", &db::iso_at(kw_at), Some(&db::iso_at(kw_at))).await;
        let last = cache_clock::last_turn_for_bot(&env.app.db, &bot.id).await.unwrap().unwrap();
        assert_eq!(last.kept_warm_at.as_deref(), Some(db::iso_at(kw_at).as_str()), "熱壓之後的新保溫又算");
    }

    #[tokio::test]
    async fn plan_never_touches_a_blocked_busy_or_non_primary_bot() {
        let env = tt::env().await;
        let now = chrono::Utc::now();
        for status in ["blocked", "working", "unknown"] {
            let (bot, run) = primary(&env, &format!("kw-{status}"), status).await;
            turn(&env, &bot, &format!("t-{status}"), None, "completed", &at(75, now), Some(&at(70, now))).await;
            assert_eq!(plan(&env.app, &bot, &run, now).await.unwrap(), None, "{status}");
        }
        let (bot, run) = primary(&env, "kw-queued", "idle").await;
        turn(&env, &bot, "t-q1", None, "completed", &at(75, now), Some(&at(70, now))).await;
        turn(&env, &bot, "t-q2", None, "queued", &at(1, now), None).await;
        assert_eq!(plan(&env.app, &bot, &run, now).await.unwrap(), None, "有排隊的回合");
        let (mut bot, run) = primary(&env, "kw-plain", "idle").await;
        turn(&env, &bot, "t-p1", None, "completed", &at(75, now), Some(&at(70, now))).await;
        bot.is_primary = 0;
        assert_eq!(plan(&env.app, &bot, &run, now).await.unwrap(), None, "不是主力");
    }

    /// 把保溫那一輪（持久的 crid）塞進去：舊資料用 `keepalive:` 前綴。
    async fn plain_message(env: &tt::Env, bot: &db::Bot, turn_id: &str, role: &str, at: &str) -> String {
        let conv = db::conversation_id(&env.app.db, &bot.id).await.unwrap();
        let id = db::ulid();
        sqlx::query("INSERT INTO messages (id, conversation_id, turn_id, role, content, source, created_at) VALUES (?,?,?,?,'x','hook',?)")
            .bind(&id)
            .bind(&conv)
            .bind(turn_id)
            .bind(role)
            .bind(at)
            .execute(&env.app.db)
            .await
            .unwrap();
        id
    }

    /// 「不用保溫」：按下之後這一輪跳過；真的活動（新回合）後自動恢復；新的錨點照樣 58 分才保溫。
    #[tokio::test]
    async fn skip_passes_this_round_and_real_activity_restores_keep_warm() {
        let env = tt::env().await;
        let now = chrono::Utc::now();
        let (bot, run) = primary(&env, "kw-skip", "idle").await;
        turn(&env, &bot, "t-real", None, "completed", &at(64, now), Some(&at(59, now))).await;
        assert_eq!(plan(&env.app, &bot, &run, now).await.unwrap().unwrap().step, Step::KeepWarm);

        assert!(set_skip(&env.app.db, &bot.id, true).await.unwrap(), "第一次按：狀態變了");
        assert!(!set_skip(&env.app.db, &bot.id, true).await.unwrap(), "再按同一個值是 no-op");
        assert_eq!(plan(&env.app, &bot, &run, now).await.unwrap(), None, "跳過這一輪：不保溫");
        let later = now + chrono::Duration::minutes(55);
        assert_eq!(plan(&env.app, &bot, &run, later).await.unwrap(), None, "也不熱壓");
        // 沒有活動：巡邏不會把它清掉。
        settle_skip(&env, &bot, &run, later).await;
        assert!(skip_since(&env.app.db, &bot.id).await.unwrap().is_some());
        // 保溫自己的回合（視窗開著）不算活動。
        open_window(&run.id);
        let fresh = now + chrono::Duration::minutes(1);
        turn(&env, &bot, "t-kw-self", Some(&keep_warm_crid("x")), "completed", &db::iso_at(fresh), Some(&db::iso_at(fresh))).await;
        settle_skip(&env, &bot, &run, later).await;
        assert!(skip_since(&env.app.db, &bot.id).await.unwrap().is_some());
        drop_window(&run.id);

        // 真的活動：使用者回合在按下之後完成 → 巡邏恢復。
        turn(&env, &bot, "t-user", None, "completed", &db::iso_at(fresh), Some(&db::iso_at(fresh))).await;
        settle_skip(&env, &bot, &run, later).await;
        assert!(skip_since(&env.app.db, &bot.id).await.unwrap().is_none(), "活動後自動恢復");
        let p = plan(&env.app, &bot, &run, fresh + chrono::Duration::minutes(59)).await.unwrap().unwrap();
        assert_eq!(p.step, Step::KeepWarm, "新的錨點照樣到 58 分才保溫");
    }

    /// 即使巡邏還沒來，新的（非保溫）prompt 一進來就恢復；保溫自己的 prompt 不算。
    #[tokio::test]
    async fn a_new_prompt_clears_skip_but_the_keep_warm_prompt_does_not() {
        let env = tt::env().await;
        let (bot, _run) = primary(&env, "kw-note", "idle").await;
        set_skip(&env.app.db, &bot.id, true).await.unwrap();
        note_prompt(&env.app, &bot.id, &keep_warm_crid("2026-10-04T10:00:00.000Z")).await;
        note_prompt(&env.app, &bot.id, "keepalive:2026-10-04T10:00:00.000Z").await;
        assert!(skip_since(&env.app.db, &bot.id).await.unwrap().is_some());
        note_prompt(&env.app, &bot.id, "web-123").await;
        assert!(skip_since(&env.app.db, &bot.id).await.unwrap().is_none());
    }

    /// 跳過的狀態在 DB：daemon 重啟（重開 DB、記憶體視窗歸零）後仍然跳過。
    #[tokio::test]
    async fn skip_survives_a_daemon_restart() {
        let env = tt::env().await;
        let now = chrono::Utc::now();
        let (bot, run) = primary(&env, "kw-restart", "idle").await;
        turn(&env, &bot, "t-real", None, "completed", &at(64, now), Some(&at(59, now))).await;
        set_skip(&env.app.db, &bot.id, true).await.unwrap();
        drop_window(&run.id);
        let reopened = crate::app_ports_p1::open(&env.dir.join("data").join("db.sqlite3")).await.unwrap();
        assert!(skip_since(&reopened, &bot.id).await.unwrap().is_some(), "重開 DB 後還在");
        assert_eq!(plan(&env.app, &bot, &run, now).await.unwrap(), None);
        let last = cache_clock::last_turn_for_bot(&reopened, &bot.id).await.unwrap().unwrap();
        assert!(last.keep_warm_skip, "run JSON 的 keep_warm_skip 也從 DB 來");
    }

    #[tokio::test]
    async fn skip_route_only_accepts_a_primary_claude_or_codex() {
        let env = tt::env().await;
        let (bot, _run) = primary(&env, "kw-route", "idle").await;
        assert!(skip_route(&env.app, &bot.id, true).await.unwrap());
        assert!(skip_since(&env.app.db, &bot.id).await.unwrap().is_some());
        assert!(!skip_route(&env.app, &bot.id, false).await.unwrap());
        assert!(skip_since(&env.app.db, &bot.id).await.unwrap().is_none(), "再按一次取消");

        let plain = tt::claude_bot(&env.app, &env.project_id, "kw-plain").await;
        match skip_route(&env.app, &plain.id, true).await {
            Err(crate::lifecycle::LcError::BadValue(v)) => assert_eq!(v["error"], "not_primary"),
            other => panic!("not_primary expected: {other:?}"),
        }
        assert!(skip_since(&env.app.db, &plain.id).await.unwrap().is_none());
        assert!(matches!(skip_route(&env.app, "no-such-bot", true).await, Err(crate::lifecycle::LcError::NotFound(_))));
    }

    /// 保溫回覆：保溫回合完成後才有；使用者送新 prompt（非保溫回合）就清成 `null`。
    #[tokio::test]
    async fn keep_warm_reply_is_set_after_the_reply_and_cleared_by_the_next_user_prompt() {
        let env = tt::env().await;
        let now = chrono::Utc::now();
        let (bot, _run) = primary(&env, "kw-replied", "idle").await;
        turn(&env, &bot, "t-real", None, "completed", &at(64, now), Some(&at(59, now))).await;
        let last = cache_clock::last_turn_for_bot(&env.app.db, &bot.id).await.unwrap().unwrap();
        assert_eq!(last.keep_warm_replied_at, None, "還沒保溫");
        turn(&env, &bot, "t-kw", Some(&keep_warm_crid("a")), "in_flight", &at(1, now), None).await;
        let last = cache_clock::last_turn_for_bot(&env.app.db, &bot.id).await.unwrap().unwrap();
        assert_eq!(last.keep_warm_replied_at, None, "保溫還沒回覆完");
        sqlx::query("UPDATE turns SET status='completed', completed_at=? WHERE id='t-kw'").bind(at(0, now)).execute(&env.app.db).await.unwrap();
        let last = cache_clock::last_turn_for_bot(&env.app.db.clone(), &bot.id).await.unwrap().unwrap();
        assert_eq!(last.keep_warm_replied_at.as_deref(), Some(at(0, now).as_str()));
        // 帶進 run JSON。
        let mut json = serde_json::json!({"id": "r-kw-replied", "agent_status": "idle"});
        cache_clock::annotate(&mut json, "claude", Some(&last));
        assert_eq!(json["keep_warm_replied_at"], at(0, now));
        // 使用者送出新的 prompt → 清成 null。
        let fresh = db::iso_at(now + chrono::Duration::seconds(30));
        turn(&env, &bot, "t-user2", None, "in_flight", &fresh, None).await;
        let last = cache_clock::last_turn_for_bot(&env.app.db, &bot.id).await.unwrap().unwrap();
        assert_eq!(last.keep_warm_replied_at, None);
        let mut json = serde_json::json!({"id": "r-kw-replied", "agent_status": "working"});
        cache_clock::annotate(&mut json, "claude", Some(&last));
        assert!(json["keep_warm_replied_at"].is_null());
    }

    /// 舊資料的 `keepalive:` 前綴一樣被認得（cache_clock、keep_warm 標記、未讀、保溫過沒）。
    #[tokio::test]
    async fn the_legacy_keepalive_prefix_is_still_recognised() {
        let env = tt::env().await;
        let now = chrono::Utc::now();
        let (bot, run) = primary(&env, "kw-legacy", "idle").await;
        turn(&env, &bot, "t-real", None, "completed", &at(64, now), Some(&at(59, now))).await;
        // 舊前綴的保溫已經送過 → 同錨點不再保溫，kept_warm_at 與保溫回覆也照算。
        turn(&env, &bot, "t-old", Some("keepalive:old-anchor"), "completed", &at(1, now), Some(&at(0, now))).await;
        let after = plan(&env.app, &bot, &run, now).await.unwrap().unwrap();
        assert_eq!(after.step, Step::Wait, "舊前綴的保溫回合也算「這個錨點之後保溫過了」");
        let last = cache_clock::last_turn_for_bot(&env.app.db, &bot.id).await.unwrap().unwrap();
        assert_eq!((last.completed_at.as_deref(), last.kept_warm_at.as_deref()), (Some(at(59, now).as_str()), Some(at(0, now).as_str())));
        assert_eq!(last.keep_warm_replied_at.as_deref(), Some(at(0, now).as_str()));

        // 訊息標記與未讀：新舊前綴的訊息都標 keep_warm、都不算未讀；一般回合的 assistant 才算。
        turn(&env, &bot, "t-new", Some("keep-warm:new-anchor"), "completed", &at(2, now), Some(&at(2, now))).await;
        turn(&env, &bot, "t-chat", None, "completed", &at(3, now), Some(&at(3, now))).await;
        let mut ids = vec![];
        for (t, role) in [("t-old", "user"), ("t-old", "assistant"), ("t-new", "user"), ("t-new", "assistant"), ("t-chat", "user"), ("t-chat", "assistant")] {
            ids.push((t, role, plain_message(&env, &bot, t, role, &at(0, now)).await));
        }
        for (t, role, id) in ids {
            let flag: i64 = sqlx::query_scalar("SELECT keep_warm FROM messages WHERE id=?").bind(&id).fetch_one(&env.app.db).await.unwrap();
            assert_eq!(flag, i64::from(t != "t-chat"), "{t} {role}");
        }
        let unread = crate::read_marks::unread_counts(&env.app.db).await.unwrap();
        assert_eq!(unread.get(&bot.id), Some(&1), "只有一般回合的回覆算未讀");
        // 新增的 migrate 會把舊列補上標記。
        sqlx::query("UPDATE messages SET keep_warm = 0").execute(&env.app.db).await.unwrap();
        let reopened = crate::app_ports_p1::open(&env.dir.join("data").join("db.sqlite3")).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE keep_warm = 1").fetch_one(&reopened).await.unwrap();
        assert_eq!(n, 4, "舊資料回填");
    }

    // ---- #872：act 帶計畫的 run id ----

    use am_core::{BotId, NoticeRequest, PortError, RunId, TurnError, TurnId};
    use std::collections::VecDeque;

    #[derive(Default)]
    struct FakeTurns {
        prompts: Mutex<Vec<PromptRequest>>,
        compacts: Mutex<Vec<(String, Option<String>)>>,
        /// 每次 `compact_bot` 依序取一個結果；用完回 `Ok`。
        compact_script: Mutex<VecDeque<Result<(), TurnError>>>,
    }

    impl TurnControl for FakeTurns {
        fn start_bot(&self, _bot: BotId) -> impl std::future::Future<Output = Result<RunId, TurnError>> + Send + '_ {
            async { Err(TurnError::Failed("unused".into())) }
        }
        fn stop_bot(&self, _bot: BotId, _reason: Option<String>) -> impl std::future::Future<Output = Result<(), TurnError>> + Send + '_ {
            async { Ok(()) }
        }
        fn send_prompt(&self, request: PromptRequest) -> impl std::future::Future<Output = Result<TurnId, TurnError>> + Send + '_ {
            async move {
                self.prompts.lock().unwrap().push(request);
                Ok("turn-fake".to_string())
            }
        }
        fn compact_bot(&self, bot: BotId, expected_run_id: Option<RunId>) -> impl std::future::Future<Output = Result<(), TurnError>> + Send + '_ {
            async move {
                self.compacts.lock().unwrap().push((bot, expected_run_id));
                self.compact_script.lock().unwrap().pop_front().unwrap_or(Ok(()))
            }
        }
        fn interrupt(&self, _bot: BotId, _reason: String) -> impl std::future::Future<Output = Result<(), TurnError>> + Send + '_ {
            async { Ok(()) }
        }
        fn queue_notice(&self, _notice: NoticeRequest) -> impl std::future::Future<Output = Result<(), TurnError>> + Send + '_ {
            async { Ok(()) }
        }
    }

    #[derive(Default)]
    struct FakeNotes {
        notes: Mutex<Vec<(String, String)>>,
        /// 前 N 次 `append_system_message` 回 Err（`usize::MAX`＝永遠）。
        fail_first: Mutex<usize>,
    }

    impl FakeNotes {
        fn failing(n: usize) -> Self {
            Self { fail_first: Mutex::new(n), ..Default::default() }
        }
    }

    impl SystemMessageWriter for FakeNotes {
        fn append_system_message(&self, bot: BotId, content: String) -> impl std::future::Future<Output = Result<(), PortError>> + Send + '_ {
            async move {
                let mut left = self.fail_first.lock().unwrap();
                if *left > 0 {
                    *left = left.saturating_sub(1);
                    return Err(PortError::Unavailable("note store down".into()));
                }
                self.notes.lock().unwrap().push((bot, content));
                Ok(())
            }
        }
    }

    struct FixedClock(chrono::DateTime<chrono::Utc>);

    impl Clock for FixedClock {
        fn now_unix_ms(&self) -> i64 {
            self.0.timestamp_millis()
        }
    }

    async fn sweep(env: &tt::Env, turns: &FakeTurns, notes: &FakeNotes, now: chrono::DateTime<chrono::Utc>) {
        let db = DbContext::new(env.app.db.clone());
        let events = crate::lifecycle::app_ports_p4::AppEventSink::new(&env.app);
        sweep_with(&db, turns, notes, &events, &FixedClock(now), &tokio_util::sync::CancellationToken::new()).await;
    }

    /// 該熱壓的主力：錨點 111 分前、53 分前保溫過（cache 仍熱）、context 45%。
    async fn warm_compact_due(env: &tt::Env, name: &str, now: chrono::DateTime<chrono::Utc>) -> (db::Bot, db::Run) {
        let (bot, run) = primary(env, name, "idle").await;
        turn(env, &bot, &format!("t-real-{name}"), None, "completed", &at(116, now), Some(&at(111, now))).await;
        let anchor = at(111, now);
        turn(env, &bot, &format!("t-kw-{name}"), Some(&keep_warm_crid(&anchor)), "completed", &at(54, now), Some(&at(53, now))).await;
        sqlx::query("UPDATE runs SET status_json = ? WHERE id = ?")
            .bind(r#"{"context_window":{"used_percentage":45}}"#)
            .bind(&run.id)
            .execute(&env.app.db)
            .await
            .unwrap();
        let run = db::active_run(&env.app.db, &bot.id).await.unwrap().unwrap();
        assert_eq!(plan(&env.app, &bot, &run, now).await.unwrap().unwrap().step, Step::WarmCompact, "前提：該熱壓");
        (bot, run)
    }

    /// #872：保溫與熱壓都帶計畫當時的 run id，由 bot 鎖內比對。
    #[tokio::test]
    async fn keep_warm_and_warm_compact_carry_the_planned_run_id() {
        let env = tt::env().await;
        let now = chrono::Utc::now();

        let (kw_bot, kw_run) = primary(&env, "kw-carry-keep", "idle").await;
        turn(&env, &kw_bot, "t-real-keep", None, "completed", &at(64, now), Some(&at(59, now))).await;
        let turns = FakeTurns::default();
        sweep(&env, &turns, &FakeNotes::default(), now).await;
        {
            let prompts = turns.prompts.lock().unwrap();
            assert_eq!(prompts.len(), 1);
            assert_eq!(prompts[0].bot_id, kw_bot.id);
            assert_eq!(prompts[0].expected_run_id.as_deref(), Some(kw_run.id.as_str()));
        }
        drop_window(&kw_run.id);

        let env = tt::env().await;
        let (bot, run) = warm_compact_due(&env, "kw-carry-compact", now).await;
        let turns = FakeTurns::default();
        sweep(&env, &turns, &FakeNotes::default(), now).await;
        assert_eq!(*turns.compacts.lock().unwrap(), vec![(bot.id.clone(), Some(run.id.clone()))]);
        drop_window(&run.id);
    }

    /// #872：鎖內被 `superseded_run` 擋下（回 Busy）＝沒壓縮：不寫熱壓說明、視窗關掉，下一輪依新 run 重判。
    #[tokio::test]
    async fn a_superseded_compact_records_no_note_and_closes_the_window() {
        let env = tt::env().await;
        let now = chrono::Utc::now();
        let (bot, run) = warm_compact_due(&env, "kw-superseded", now).await;
        let turns = FakeTurns::default();
        turns.compact_script.lock().unwrap().push_back(Err(TurnError::Busy(bot.id.clone())));
        let notes = FakeNotes::default();
        sweep(&env, &turns, &notes, now).await;
        assert_eq!(turns.compacts.lock().unwrap().len(), 1);
        assert!(notes.notes.lock().unwrap().is_empty(), "沒壓縮就不寫熱壓說明");
        assert!(!window_open(&run.id), "被拒絕時視窗不留著");
    }
