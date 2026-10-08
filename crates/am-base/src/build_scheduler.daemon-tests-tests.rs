
    use super::*;
    use crate::runners::build_scheduler::{get_status, post_acquire, post_release, post_renew};
    use axum::extract::{Form, State};
    use axum::http::HeaderMap;
    use axum::response::IntoResponse;
    use crate::state::App;
    use std::sync::Arc;
    use crate::testing as tt;

    async fn a_bot(env: &tt::Env, hook_token: &str) -> String {
        let id = crate::db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,?,'claude','[]',0,1,?,?)",
        )
        .bind(&id)
        .bind(&env.project_id)
        .bind(format!("bot-{id}"))
        .bind(hook_token)
        .bind(crate::db::now())
        .execute(&env.app.db)
        .await
        .unwrap();
        id
    }

    async fn set_max_concurrent(app: &Arc<App>, n: usize) {
        app.cfg
            .update(|cfg| {
                cfg.build.max_concurrent = n;
                Ok(())
            })
            .await
            .unwrap();
    }

    /// #322／#639：0 與超大的 lease_ttl_secs 都不能寫進設定。0 會讓名額立刻過期；u64::MAX 用 `as i64` 變 -1，更大的值讓時鐘加法 panic。
    #[tokio::test]
    async fn a_lease_ttl_outside_the_range_is_rejected_and_does_not_panic() {
        let env = tt::env().await;
        let app = env.app.clone();
        set_max_concurrent(&app, 1).await;
        for bad in [0_u64, u64::MAX, 10_000_000_000_000] {
            let err = app.cfg.update(|cfg| { cfg.build.lease_ttl_secs = bad; Ok(()) }).await.unwrap_err().to_string();
            assert!(err.contains("lease_ttl_secs") && err.contains("未變更"), "{bad}: {err}");
        }
        assert_eq!(app.cfg.build_fresh().await.lease_ttl().unwrap(), 180);
        assert!(matches!(acquire(&app, "A:1", None, "test", "local").await.unwrap(), Acquired::Granted { .. }));
        assert!(matches!(acquire(&app, "B:2", None, "test", "local").await.unwrap(), Acquired::Waiting { .. }));
        let huge = crate::config::BuildCfg { lease_ttl_secs: u64::MAX, ..crate::config::BuildCfg::default() };
        let err = expires_at_after(&huge).unwrap_err().to_string();
        assert!(err.contains("lease_ttl_secs"), "{err}");
    }

    /// #327：release 的 DB 寫失敗不能回 released:true（名額會佔到 TTL 而呼叫端以為已放）。
    #[tokio::test]
    async fn a_release_that_cannot_write_says_so() {
        let env = tt::env().await;
        let app = env.app.clone();
        let Acquired::Granted { token, .. } = acquire(&app, "A:1", None, "test", "local").await.unwrap() else { panic!() };
        tt::make_table_unreadable(&app, "build_slots").await;
        let (code, body) = post_release(State(app.clone()), HeaderMap::new(), Form(ReleaseIn { holder: "A:1".into(), token: token.clone(), bot_id: None })).await;
        tt::make_table_readable(&app, "build_slots").await;
        assert_eq!(code, axum::http::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.0["released"], false);
    }

    /// 任何一顆 bot（含被 prompt injection 的）都能呼叫 acquire，`holder` 又是呼叫端自己取的：每個新 holder 一列，
    /// 不設上限的話，一顆 bot 用不同 holder 狂送就能把佇列塞滿、讓真正的建置排不到（FIFO 擋在最前面的是幽靈），
    /// 也能把整張表撐大。每顆 bot 最多同時佔 [`MAX_ROWS_PER_BOT`] 列；已經有的列重送照舊（冪等），別顆 bot 不受影響。
    #[tokio::test]
    async fn a_bot_cannot_flood_the_build_queue_with_holders() {
        let env = tt::env().await;
        let app = env.app.clone();
        set_max_concurrent(&app, 1).await;
        for i in 0..MAX_ROWS_PER_BOT {
            let r = acquire(&app, &format!("flood:{i}"), Some("BOTA"), "x", "local").await.unwrap();
            assert!(matches!(r, Acquired::Granted { .. } | Acquired::Waiting { .. }), "第 {i} 列：{r:?}");
        }
        assert_eq!(acquire(&app, "flood:extra", Some("BOTA"), "x", "local").await.unwrap(), Acquired::TooManyForBot);
        // 已經有的列重送不受影響（waiting 的重 poll、held 的重問）。
        assert!(!matches!(acquire(&app, "flood:1", Some("BOTA"), "x", "local").await.unwrap(), Acquired::TooManyForBot));
        // 別顆 bot 照樣排得進去。
        assert!(matches!(acquire(&app, "b:1", Some("BOTB"), "x", "local").await.unwrap(), Acquired::Waiting { .. }));
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM build_slots").fetch_one(&app.db).await.unwrap();
        assert_eq!(rows as usize, MAX_ROWS_PER_BOT + 1, "表不會被撐大");
        // 手動（UI token，沒有 bot 身分）不受這個上限管：那是人在操作。
        assert!(matches!(acquire(&app, "manual:1", None, "x", "local").await.unwrap(), Acquired::Waiting { .. }));
    }

    /// holder／purpose／host 是呼叫端給的字串，原樣進 DB 再顯示在網頁：不設長度上限等於讓呼叫端往 DB 寫任意大的東西。
    #[tokio::test]
    async fn oversized_holder_purpose_and_host_are_rejected() {
        let env = tt::env().await;
        let app = env.app.clone();
        let mut h = HeaderMap::new();
        h.insert("X-AM-Token", app.ui_token.parse().unwrap());
        let ok = |holder: &str, purpose: &str, host: &str| AcquireIn { holder: holder.into(), bot_id: None, purpose: purpose.into(), host: host.into() };
        for (holder, purpose, host) in [("h".repeat(MAX_FIELD_CHARS + 1), String::new(), "local".to_string()), ("h".into(), "p".repeat(MAX_FIELD_CHARS + 1), "local".into()), ("h".into(), String::new(), "x".repeat(MAX_FIELD_CHARS + 1))] {
            let err = post_acquire(State(app.clone()), h.clone(), Form(ok(&holder, &purpose, &host))).await.unwrap_err();
            assert!(matches!(err, LcError::Bad(_)), "{err:?}");
        }
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM build_slots").fetch_one(&app.db).await.unwrap();
        assert_eq!(rows, 0, "被擋下來的不能留下任何一列");
        let _ = post_acquire(State(app.clone()), h, Form(ok("h", "p", "local"))).await.unwrap();
    }

    /// The acquire form is not the only path accepting an untrusted holder: renew/release carry it too.
    /// Keep the same field bound there so a caller cannot bypass the DB/log cap with a long holder.
    #[tokio::test]
    async fn oversized_holders_are_rejected_by_renew_and_release_too() {
        let env = tt::env().await;
        let app = env.app.clone();
        let Acquired::Granted { token, .. } = acquire(&app, "held", None, "test", "local").await.unwrap() else { panic!() };
        let huge = "h".repeat(MAX_FIELD_CHARS + 1);

        let renew_error = post_renew(State(app.clone()), Form(RenewIn { holder: huge.clone(), token: token.clone() })).await.unwrap_err();
        assert!(matches!(renew_error, LcError::Bad(_)), "renew bypassed the holder field cap: {renew_error:?}");

        let (code, body) = post_release(State(app.clone()), HeaderMap::new(), Form(ReleaseIn { holder: huge, token: token.clone(), bot_id: None })).await;
        assert_eq!(code, axum::http::StatusCode::BAD_REQUEST, "release bypassed the holder field cap: {body:?}");
        assert_eq!(body.0["released"], false);
        assert!(matches!(renew(&app, "held", &token).await.unwrap(), Ok(_)), "rejected oversized requests must leave the real lease intact");
    }

    /// 核心驗收條件（issue #90）：N 個同時的 acquire，只有設定的名額數真的拿到，其餘回 waiting。
    #[tokio::test]
    async fn only_the_configured_number_of_concurrent_acquires_are_granted() {
        let env = tt::env().await;
        let app = env.app.clone();
        set_max_concurrent(&app, 2).await;

        let mut handles = Vec::new();
        for i in 0..5 {
            let app = app.clone();
            handles.push(tokio::spawn(async move { acquire(&app, &format!("agent-{i}:{i}"), None, "test", "local").await.unwrap() }));
        }
        let results: Vec<Acquired> = futures::future::join_all(handles).await.into_iter().map(|r| r.unwrap()).collect();
        let granted = results.iter().filter(|r| matches!(r, Acquired::Granted { .. })).count();
        let waiting = results.iter().filter(|r| matches!(r, Acquired::Waiting { .. })).count();
        assert_eq!(granted, 2, "{results:?}");
        assert_eq!(waiting, 3, "{results:?}");

        let s = status(&app).await.unwrap();
        assert_eq!(s["active"], 2);
        assert_eq!(s["slots"].as_array().unwrap().len(), 5, "等待中的也看得到");
    }

    /// issue #428：本機隊伍排得再長，也要看得出來是不是因為外部編譯主機連不上。
    /// 「沒開」「開了但還沒有人試過」「上一次連不上」是三種狀態，不能混成同一個布林。
    #[tokio::test]
    async fn the_status_says_whether_the_remote_build_host_was_reachable_last_time() {
        let env = tt::env().await;
        let app = env.app.clone();

        let s = status(&app).await.unwrap();
        assert_eq!(s["remote"]["enabled"], json!(false), "預設沒開");
        assert_eq!(s["remote"]["remote_reachable"], json!(null), "沒開就不是 false——那會亮一個假的紅燈");

        app.cfg
            .update(|cfg| {
                cfg.build.remote.enabled = true;
                cfg.build.remote.user = "me".into();
                cfg.build.remote.host = "box".into();
                Ok(())
            })
            .await
            .unwrap();
        let s = status(&app).await.unwrap();
        assert_eq!(s["remote"]["target"], json!("me@box:22"));
        assert_eq!(s["remote"]["remote_reachable"], json!(null), "開了但還沒有人試過：不知道");

        std::fs::create_dir_all(&app.data_dir).unwrap();
        crate::remote_health::record(
            &app.data_dir,
            &crate::remote_health::Health {
                reachable: false,
                checked_at: "2026-09-24T10:00:00.000Z".into(),
                target: "me@box:22".into(),
                reason: Some("ssh 回 255".into()),
            },
        )
        .unwrap();
        let s = status(&app).await.unwrap();
        assert_eq!(s["remote"]["remote_reachable"], json!(false));
        assert_eq!(s["remote"]["checked_at"], json!("2026-09-24T10:00:00.000Z"), "什麼時候的結論要講");
        assert_eq!(s["remote"]["reason"], json!("ssh 回 255"), "原因照 helper 記的帶出來");
    }

    /// 放掉一個名額之後，等待中的下一次 acquire 就能拿到——不是永遠卡住。
    #[tokio::test]
    async fn releasing_a_slot_frees_capacity_for_a_waiter() {
        let env = tt::env().await;
        let app = env.app.clone();
        set_max_concurrent(&app, 1).await;

        let Acquired::Granted { token, .. } = acquire(&app, "first", None, "test", "local").await.unwrap() else { panic!() };
        let Acquired::Waiting { .. } = acquire(&app, "second", None, "test", "local").await.unwrap() else { panic!("滿了應該要等") };

        release(&app, "first", &token).await.unwrap();
        let Acquired::Granted { .. } = acquire(&app, "second", None, "test", "local").await.unwrap() else { panic!("放掉了，下一個該拿到") };
    }

    /// 持有者沒有 renew、TTL 過期：下一次 acquire 收回這個名額，不是永遠卡死（daemon 重啟也一樣，
    /// 因為 acquire 每次都先 reap 過期列——不需要額外處理「重啟後」這個特例）。
    #[tokio::test]
    async fn a_holder_that_stops_renewing_loses_its_slot_after_ttl() {
        let env = tt::env().await;
        let app = env.app.clone();
        set_max_concurrent(&app, 1).await;

        acquire(&app, "dead", None, "test", "local").await.unwrap();
        // 模擬 TTL 已過（不用真的等），直接把 expires_at 撥回過去。
        sqlx::query("UPDATE build_slots SET expires_at = '2020-01-01T00:00:00.000Z' WHERE holder = 'dead'").execute(&app.db).await.unwrap();

        let Acquired::Granted { .. } = acquire(&app, "new-holder", None, "test", "local").await.unwrap() else {
            panic!("過期的名額應該被收回，換人拿到")
        };
    }

    /// 重call 已經握著的名額是幂等的：同一個 holder 再 acquire 一次拿回同一個 token，不會被降級成 waiting。
    #[tokio::test]
    async fn re_acquiring_an_already_held_slot_is_idempotent() {
        let env = tt::env().await;
        let app = env.app.clone();
        set_max_concurrent(&app, 1).await;
        let Acquired::Granted { token: t1, .. } = acquire(&app, "me", None, "test", "local").await.unwrap() else { panic!() };
        let Acquired::Granted { token: t2, .. } = acquire(&app, "me", None, "test", "local").await.unwrap() else { panic!("已經握著的不該變成 waiting") };
        assert_eq!(t1, t2);
    }

    /// A build-slot token is a bearer secret. It must not reuse the daemon's public, monotonic ULID
    /// generator, whose next value can be predicted from any ID exposed in the same millisecond.
    #[tokio::test]
    async fn build_slot_tokens_are_independent_128_bit_bearer_secrets() {
        let env = tt::env().await;
        let Acquired::Granted { token, .. } = acquire(&env.app, "secret-check", None, "test", "local").await.unwrap() else { panic!() };
        assert_eq!(token.len(), 32, "slot bearer tokens need 128 random bits, not a timestamped ULID: {token}");
        assert!(token.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()), "slot token must be lower-case hex: {token}");
    }

    /// renew 要對得上 token 才續得動；沒有這一列（沒拿過／已被收回）一律要求重新 acquire。
    #[tokio::test]
    async fn renew_checks_the_token_and_refuses_a_slot_nobody_holds() {
        let env = tt::env().await;
        let app = env.app.clone();
        set_max_concurrent(&app, 1).await;
        let Acquired::Granted { token, expires_at: first_exp } = acquire(&app, "me", None, "test", "local").await.unwrap() else { panic!() };

        assert_eq!(renew(&app, "me", "wrong-token").await.unwrap(), Err(RenewErr::TokenMismatch));
        assert_eq!(renew(&app, "nobody", "anything").await.unwrap(), Err(RenewErr::NotFound));

        // 毫秒級的時間戳：兩次呼叫緊接在一起，機器夠快就可能落在同一毫秒——睡一下確保牆上時間真的往前走，
        // 不然這條斷言測的是「機器夠不夠慢」而不是「續約有沒有真的延長」。
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let Ok(new_exp) = renew(&app, "me", &token).await.unwrap() else { panic!() };
        assert!(new_exp > first_exp, "續約要往後延");
    }

    #[tokio::test]
    async fn a_forged_token_cannot_release_another_holders_slot() {
        let env = tt::env().await;
        let app = env.app.clone();
        let Acquired::Granted { token, .. } = acquire(&app, "victim", Some("BOTA"), "test", "local").await.unwrap() else { panic!() };

        // Release is intentionally idempotent and reports success for an unknown token. Check the
        // stored lease itself so an unauthenticated caller cannot free somebody else's capacity.
        let (code, body) = post_release(
            State(app.clone()),
            HeaderMap::new(),
            Form(ReleaseIn { holder: "victim".into(), token: "guessed-token".into(), bot_id: None }),
        )
        .await;
        assert_eq!(code, axum::http::StatusCode::OK);
        assert_eq!(body.0["released"], true);
        assert!(matches!(renew(&app, "victim", &token).await.unwrap(), Ok(_)), "the valid lease must remain held");
    }

    /// issue #913：等名額途中放棄的 shim 空 token 取消自己的號碼牌，後面的人立刻能拿到名額，不必等 `STALE_WAITING`。
    #[tokio::test]
    async fn a_waiter_that_gives_up_releases_its_place_at_once() {
        let env = tt::env().await;
        let app = env.app.clone();
        set_max_concurrent(&app, 1).await;
        let Acquired::Granted { token, .. } = acquire(&app, "A:1", None, "test", "local").await.unwrap() else { panic!() };
        let Acquired::Waiting { .. } = acquire(&app, "B:2", None, "test", "local").await.unwrap() else { panic!("滿了應該要等") };
        // C 排在 B 後面；A 放掉名額後，B 的號碼牌（沒有 poll、也沒被取消）還在最前面，C 要讓它。
        let Acquired::Waiting { .. } = acquire(&app, "C:3", None, "test", "local").await.unwrap() else { panic!("滿了應該要等") };
        release(&app, "A:1", &token).await.unwrap();
        assert!(matches!(acquire(&app, "C:3", None, "test", "local").await.unwrap(), Acquired::Waiting { .. }), "B 的號碼牌還在前面，C 不能插隊");
        // B 放棄：不改 last_seen（沒有等 60 秒），空 token 直接退號碼牌。
        release_as(&app, "B:2", "", None).await.unwrap();
        assert!(matches!(acquire(&app, "C:3", None, "test", "local").await.unwrap(), Acquired::Granted { .. }), "B 退號碼牌後 C 立刻拿到");
    }

    /// 空 token 只動 `waiting` 列：持有中的名額不會被它放掉。
    #[tokio::test]
    async fn release_with_an_empty_token_does_not_touch_a_held_slot() {
        let env = tt::env().await;
        let app = env.app.clone();
        set_max_concurrent(&app, 1).await;
        let Acquired::Granted { token, .. } = acquire(&app, "A:1", Some("BOTA"), "test", "local").await.unwrap() else { panic!() };
        release_as(&app, "A:1", "", Some("BOTA")).await.unwrap();
        release_as(&app, "A:1", "", None).await.unwrap();
        assert!(matches!(renew(&app, "A:1", &token).await.unwrap(), Ok(_)), "held 列還在、token 還有效");
    }

    /// 別顆 bot 的號碼牌不能被取消（沿用 acquire 的 holder 歸屬守衛）；自己的可以。
    #[tokio::test]
    async fn a_waiting_row_of_another_bot_is_not_cancelled() {
        let env = tt::env().await;
        let app = env.app.clone();
        set_max_concurrent(&app, 1).await;
        acquire(&app, "H:1", None, "test", "local").await.unwrap();
        let Acquired::Waiting { .. } = acquire(&app, "W:2", Some("BOTA"), "test", "local").await.unwrap() else { panic!("滿了應該要等") };
        let waiting = || async { status(&app).await.unwrap()["slots"].as_array().unwrap().iter().any(|v| v["holder"] == "W:2") };
        release_as(&app, "W:2", "", Some("BOTB")).await.unwrap();
        assert!(waiting().await, "BOTB 取消不了 BOTA 的號碼牌");
        release_as(&app, "W:2", "", Some("BOTA")).await.unwrap();
        assert!(!waiting().await, "自己的可以取消");
    }

    /// HTTP 層：空 token 沒帶身分 → 403；帶 bot 身分只能取消自己的；一般 token 的放名額照舊不需要標頭。
    #[tokio::test]
    async fn cancelling_a_wait_over_http_needs_an_identity() {
        let env = tt::env().await;
        let app = env.app.clone();
        set_max_concurrent(&app, 1).await;
        let bot = a_bot(&env, "tok-a").await;
        acquire(&app, "H:1", None, "test", "local").await.unwrap();
        let Acquired::Waiting { .. } = acquire(&app, "W:2", Some(&bot), "test", "local").await.unwrap() else { panic!("滿了應該要等") };
        let (code, _) = post_release(State(app.clone()), HeaderMap::new(), Form(ReleaseIn { holder: "W:2".into(), token: String::new(), bot_id: None })).await;
        assert_eq!(code, axum::http::StatusCode::FORBIDDEN, "沒有身分不能取消");
        let mut headers = HeaderMap::new();
        headers.insert("X-AM-Bot-Id", bot.parse().unwrap());
        headers.insert("X-AM-Bot-Token", "tok-a".parse().unwrap());
        let (code, _) = post_release(State(app.clone()), headers, Form(ReleaseIn { holder: "W:2".into(), token: String::new(), bot_id: Some(bot.clone()) })).await;
        assert_eq!(code, axum::http::StatusCode::OK);
        let s = status(&app).await.unwrap();
        assert!(!s["slots"].as_array().unwrap().iter().any(|v| v["holder"] == "W:2"), "取消後號碼牌不在了");
    }

    /// sweep 收掉過期的 held 與太久沒 poll 的 waiting；還在正常範圍內的 waiting 不動（正在排隊，只是隊伍長）。
    /// 過期、太久沒 poll 都用直接改 DB 模擬「時間過去了」，不透過 acquire——acquire 自己也會 lazy reap
    /// 過期的 held 列，在這裡呼叫只會混淆「到底是誰收的」。
    #[tokio::test]
    async fn sweep_reaps_dead_held_and_stale_waiting_but_leaves_live_waiters() {
        let env = tt::env().await;
        let app = env.app.clone();
        set_max_concurrent(&app, 1).await;
        acquire(&app, "holder", None, "test", "local").await.unwrap();
        let Acquired::Waiting { .. } = acquire(&app, "dead-waiter", None, "test", "local").await.unwrap() else { panic!("滿了應該要等") };
        let Acquired::Waiting { .. } = acquire(&app, "live-waiter", None, "test", "local").await.unwrap() else { panic!("滿了應該要等") };

        sqlx::query("UPDATE build_slots SET expires_at = '2020-01-01T00:00:00.000Z' WHERE holder = 'holder'").execute(&app.db).await.unwrap();
        // dead-waiter 早就不再 poll 了；live-waiter 這一刻還在 poll（last_seen 不動，維持剛剛 acquire 留下的現在時刻）。
        sqlx::query("UPDATE build_slots SET last_seen = '2020-01-01T00:00:00.000Z' WHERE holder = 'dead-waiter'").execute(&app.db).await.unwrap();

        let (held, waiting) = sweep(&app).await;
        assert_eq!((held, waiting), (1, 1));
        let s = status(&app).await.unwrap();
        let holders: Vec<String> = s["slots"].as_array().unwrap().iter().map(|v| v["holder"].as_str().unwrap().to_string()).collect();
        assert_eq!(holders, vec!["live-waiter".to_string()], "holder 過期收掉、dead-waiter 太久沒 poll 收掉，live-waiter 還在排隊沒被誤收");
    }

    /// FIFO（使用者 2026-09-18 交辦）：名額空出來時只有排最前面的拿得到，就算別人這一刻剛好也在問、
    /// 名額也剛好空著。用「後進場的先發問」故意打亂 poll 順序，證明放行順序看的是**進場順序**不是**發問順序**。
    #[tokio::test]
    async fn waiters_are_granted_in_the_order_they_first_queued_not_the_order_they_poll_in() {
        let env = tt::env().await;
        let app = env.app.clone();
        set_max_concurrent(&app, 1).await;

        let Acquired::Granted { token: first_token, .. } = acquire(&app, "first", None, "t", "local").await.unwrap() else { panic!() };
        // 進場順序：a, b, c（每個之間睡一下，確保 since 的毫秒級排序穩定）。
        let Acquired::Waiting { .. } = acquire(&app, "a", None, "t", "local").await.unwrap() else { panic!() };
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let Acquired::Waiting { .. } = acquire(&app, "b", None, "t", "local").await.unwrap() else { panic!() };
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let Acquired::Waiting { .. } = acquire(&app, "c", None, "t", "local").await.unwrap() else { panic!() };

        release(&app, "first", &first_token).await.unwrap();

        // 發問順序刻意倒過來：c 先問、b 再問、a 最後問——沒有一個排在 a 前面拿得到。
        let Acquired::Waiting { .. } = acquire(&app, "c", None, "t", "local").await.unwrap() else { panic!("c 排最後，不該搶到") };
        let Acquired::Waiting { .. } = acquire(&app, "b", None, "t", "local").await.unwrap() else { panic!("b 前面還有 a，不該搶到") };
        let Acquired::Granted { token: a_token, .. } = acquire(&app, "a", None, "t", "local").await.unwrap() else { panic!("a 排最早，該輪到它") };

        release(&app, "a", &a_token).await.unwrap();
        let Acquired::Waiting { .. } = acquire(&app, "c", None, "t", "local").await.unwrap() else { panic!("c 還是排最後") };
        let Acquired::Granted { .. } = acquire(&app, "b", None, "t", "local").await.unwrap() else { panic!("該輪到 b 了") };
    }

    /// 排最前面的號碼牌死了（不再 poll）：不能永遠擋住後面活著的人（使用者實測手工腳本的舊版本會餓死 74 分鐘）。
    #[tokio::test]
    async fn a_dead_waiter_at_the_front_of_the_queue_does_not_block_the_ones_behind_it() {
        let env = tt::env().await;
        let app = env.app.clone();
        set_max_concurrent(&app, 1).await;

        let Acquired::Granted { token, .. } = acquire(&app, "first", None, "t", "local").await.unwrap() else { panic!() };
        let Acquired::Waiting { .. } = acquire(&app, "dead-front", None, "t", "local").await.unwrap() else { panic!() };
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let Acquired::Waiting { .. } = acquire(&app, "alive-second", None, "t", "local").await.unwrap() else { panic!() };

        // dead-front 排最前面，但早就不再 poll 了。
        sqlx::query("UPDATE build_slots SET last_seen = '2020-01-01T00:00:00.000Z' WHERE holder = 'dead-front'").execute(&app.db).await.unwrap();
        release(&app, "first", &token).await.unwrap();

        let Acquired::Granted { .. } = acquire(&app, "alive-second", None, "t", "local").await.unwrap() else {
            panic!("死掉的號碼牌不該永遠擋住後面活著的人")
        };
        // dead-front 的列也該一併被清掉，不是留著佔 GET /build-slots 的版面。
        let s = status(&app).await.unwrap();
        let holders: Vec<String> = s["slots"].as_array().unwrap().iter().map(|v| v["holder"].as_str().unwrap().to_string()).collect();
        assert!(!holders.contains(&"dead-front".to_string()), "{holders:?}");
    }

    /// issue #813：拿到名額的回應與狀態都帶 `test_threads`（shim 拿它注入 `RUST_TEST_THREADS`），預設 8；設定超過上限的夾到 256。
    #[tokio::test]
    async fn the_grant_tells_the_shim_how_many_test_threads_a_slot_gets() {
        let env = tt::env().await;
        let app = env.app.clone();
        let h = auth_headers(None, Some("test-token"), None);
        let out = post_acquire(State(app.clone()), h, Form(AcquireIn { holder: "manual:host:813".into(), bot_id: None, purpose: "test".into(), host: "local".into() }))
            .await
            .unwrap();
        assert_eq!(out.0["granted"], true, "{}", out.0);
        assert_eq!(out.0["test_threads"], 8, "{}", out.0);
        assert_eq!(status(&app).await.unwrap()["test_threads"], 8);

        let huge = crate::config::BuildCfg { test_threads: 100_000, ..crate::config::BuildCfg::default() };
        assert_eq!(huge.test_threads(), crate::config::MAX_BUILD_TEST_THREADS);
        let off = crate::config::BuildCfg { test_threads: 0, ..crate::config::BuildCfg::default() };
        assert_eq!(off.test_threads(), 0, "0＝不設，交給 libtest 的預設");
    }

    fn auth_headers(bot_token: Option<&str>, ui_token: Option<&str>, bot_id: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(t) = bot_token {
            h.insert("X-AM-Bot-Token", t.parse().unwrap());
        }
        if let Some(t) = ui_token {
            h.insert("X-AM-Token", t.parse().unwrap());
        }
        if let Some(id) = bot_id {
            h.insert("X-AM-Bot-Id", id.parse().unwrap());
        }
        h
    }

    /// bot 用自己的 hook token 就能參與排程，不需要一般 UI token（bot 的 pane 裡本來就拿不到那個）；
    /// 人工 host shell 用 UI token 一樣放行；兩個都沒有／都不對 → 401。
    #[tokio::test]
    async fn build_slot_bot_or_user_credentials_are_exclusive() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = a_bot(&env, "tok-123").await;

        let h = auth_headers(Some("tok-123"), None, Some(&bot_id));
        let out = post_acquire(
            State(app.clone()),
            h,
            Form(AcquireIn { holder: "b1".into(), bot_id: Some(bot_id.clone()), purpose: "test".into(), host: "local".into() }),
        )
        .await
        .unwrap();
        assert_eq!(out.0["granted"], true);

        let h = auth_headers(None, Some("test-token"), None);
        let out = post_acquire(State(app.clone()), h, Form(AcquireIn { holder: "manual:host:1".into(), bot_id: None, purpose: "".into(), host: "local".into() }))
            .await
            .unwrap();
        assert_eq!(out.0["granted"], true);

        // Old shims that send body bot_id plus the hook token (no X-AM-Bot-Id) still authenticate during
        // pane migration. Both slots are taken by now, so this one queues — the point is it is not refused.
        let h = auth_headers(Some("tok-123"), None, None);
        let out = post_acquire(
            State(app.clone()),
            h,
            Form(AcquireIn { holder: "b1-legacy".into(), bot_id: Some(bot_id.clone()), purpose: "test".into(), host: "local".into() }),
        )
        .await
        .unwrap();
        assert!(out.0["granted"].is_boolean(), "{}", out.0);

        let h = auth_headers(Some("wrong"), None, Some(&bot_id));
        assert!(matches!(
            post_acquire(State(app.clone()), h, Form(AcquireIn { holder: "b2".into(), bot_id: Some(bot_id.clone()), purpose: "".into(), host: "local".into() })).await,
            Err(LcError::Forbidden(_))
        ));

        let h = auth_headers(Some("wrong"), Some("test-token"), Some(&bot_id));
        assert!(matches!(
            post_acquire(State(app.clone()), h, Form(AcquireIn { holder: "b4".into(), bot_id: Some(bot_id.clone()), purpose: "".into(), host: "local".into() })).await,
            Err(LcError::Forbidden(_))
        ), "a bad Bot proof plus valid UI token must not downgrade to User");

        let h = auth_headers(Some("tok-123"), None, Some("another-bot"));
        assert!(matches!(
            post_acquire(State(app.clone()), h, Form(AcquireIn { holder: "b5".into(), bot_id: Some(bot_id.clone()), purpose: "".into(), host: "local".into() })).await,
            Err(LcError::Forbidden(_))
        ), "header identity must match the bot id used by the scheduler");

        let h = HeaderMap::new();
        assert!(matches!(
            post_acquire(State(app.clone()), h, Form(AcquireIn { holder: "b3".into(), bot_id: None, purpose: "".into(), host: "local".into() })).await,
            Err(LcError::Forbidden(_))
        ));
    }

    /// A holder string is caller-chosen and must not let one bot recover another bot's lease token
    /// or take over its FIFO position.
    #[tokio::test]
    async fn a_bot_cannot_reuse_another_bots_build_slot_holder() {
        let env = tt::env().await;
        let app = env.app.clone();
        set_max_concurrent(&app, 1).await;
        let owner_id = a_bot(&env, "owner-token").await;
        let attacker_id = a_bot(&env, "attacker-token").await;

        let owner = post_acquire(
            State(app.clone()),
            auth_headers(Some("owner-token"), None, Some(&owner_id)),
            Form(AcquireIn { holder: "shared-holder".into(), bot_id: Some(owner_id.clone()), purpose: "owner".into(), host: "local".into() }),
        )
        .await
        .unwrap();
        let owner_token = owner.0["token"].as_str().unwrap().to_string();

        let denied = post_acquire(
            State(app.clone()),
            auth_headers(Some("attacker-token"), None, Some(&attacker_id)),
            Form(AcquireIn {
                holder: "shared-holder".into(),
                bot_id: Some(attacker_id.clone()),
                purpose: "take owner lease".into(),
                host: "local".into(),
            }),
        )
        .await
        .unwrap_err()
        .into_response();
        assert_eq!(denied.status(), axum::http::StatusCode::FORBIDDEN);

        // The owner still gets the same secret; the attacker did not replace the row.
        let owner_again = post_acquire(
            State(app.clone()),
            auth_headers(Some("owner-token"), None, Some(&owner_id)),
            Form(AcquireIn { holder: "shared-holder".into(), bot_id: Some(owner_id.clone()), purpose: "owner".into(), host: "local".into() }),
        )
        .await
        .unwrap();
        assert_eq!(owner_again.0["token"].as_str(), Some(owner_token.as_str()));

        // The same check protects queued rows: a bot cannot replace another bot's saved queue entry.
        release(&app, "shared-holder", &owner_token).await.unwrap();
        let blocker = acquire(&app, "blocker", None, "test", "local").await.unwrap();
        let Acquired::Waiting { .. } = acquire(&app, "queued-holder", Some(&owner_id), "owner", "local").await.unwrap() else {
            panic!("the owner's second holder should wait")
        };
        let denied = post_acquire(
            State(app.clone()),
            auth_headers(Some("attacker-token"), None, Some(&attacker_id)),
            Form(AcquireIn {
                holder: "queued-holder".into(),
                bot_id: Some(attacker_id),
                purpose: "take queue position".into(),
                host: "local".into(),
            }),
        )
        .await
        .unwrap_err()
        .into_response();
        assert_eq!(denied.status(), axum::http::StatusCode::FORBIDDEN);

        let queued_owner: Option<String> = sqlx::query_scalar("SELECT bot_id FROM build_slots WHERE holder = 'queued-holder'")
            .fetch_optional(&app.db)
            .await
            .unwrap();
        assert_eq!(queued_owner.as_deref(), Some(owner_id.as_str()));
        if let Acquired::Granted { token, .. } = blocker {
            release(&app, "blocker", &token).await.unwrap();
        } else {
            panic!("the blocker should hold the only slot")
        }
    }
