
    use super::*;
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::Json;
    use crate::runners::credential_spawn::{begin, finish, RotationFence, SpawnBegin, SpawnFinish};

    #[test]
    fn a_pending_rotation_refuses_new_spawns_and_an_existing_spawn_refuses_rotation() {
        let mut gate = Gate::default();
        let bot = "gate-test";
        gate.reserve(bot, "before-rotation", PERMIT_TTL).unwrap();
        assert_eq!(gate.begin_rotation(bot).unwrap_err(), FenceError::SpawnsInFlight(1));
        assert!(gate.reserve(bot, "during-failed-rotation", PERMIT_TTL).is_ok(), "failed fence setup must be cleared");
        gate.release(bot, "before-rotation");
        gate.release(bot, "during-failed-rotation");

        gate.begin_rotation(bot).unwrap();
        assert!(gate.reserve(bot, "during-rotation", PERMIT_TTL).is_err());
        gate.rotating.remove(bot);
        assert!(gate.reserve(bot, "after-aborted-rotation", PERMIT_TTL).is_ok());
    }

    /// #664：失敗的 agent start 留下的 permit 過了 TTL 就不再擋輪替。
    #[test]
    fn an_expired_spawn_permit_does_not_block_rotation() {
        let mut gate = Gate::default();
        let bot = "ttl";
        gate.reserve(bot, "stuck", PERMIT_TTL).unwrap();
        gate.age_permit(bot, "stuck", PERMIT_TTL + Duration::from_secs(1));
        gate.begin_rotation(bot).unwrap();
        assert!(!gate.permit_active(bot, "stuck"));
    }

    #[test]
    fn a_spawn_permit_is_bound_to_one_pane_and_abort_cannot_race_finish() {
        let mut gate = Gate::default();
        let bot = "single-pane";
        let permit = "one-use";
        gate.reserve(bot, permit, PERMIT_TTL).unwrap();

        gate.claim_finish(bot, permit, "w1:p1").unwrap();
        assert!(!gate.release(bot, permit), "abort must not remove a pane registration while finish is in progress");
        assert_eq!(gate.claim_finish(bot, permit, "w1:p2"), Err(FinishClaimError::InProgress));

        gate.unclaim_finish(bot, permit, "w1:p1");
        assert_eq!(gate.claim_finish(bot, permit, "w1:p2"), Err(FinishClaimError::PaneMismatch));
        gate.claim_finish(bot, permit, "w1:p1").unwrap();
        assert!(gate.complete_finish(bot, permit, "w1:p1"));
        assert_eq!(gate.claim_finish(bot, permit, "w1:p1"), Err(FinishClaimError::Missing));
    }

    fn bot_headers() -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        h.insert("X-AM-Bot-Token", "tok".parse().unwrap());
        h
    }

    /// `herdr agent start --timeout` 最長 300 秒：子 agent 慢慢啟動時，permit 不能在 `finish` 之前就過期
    /// （以前固定 60 秒：pane 其實開好了，finish 回 409「permit missing」、shim 報尚未登記）。
    /// permit 的有效期涵蓋那次的 timeout，期間照樣擋輪替。
    #[tokio::test]
    async fn a_long_agent_start_timeout_keeps_its_permit_until_finish() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "slow-spawner").await;
        crate::testing::fake_run(&app, &bot.id).await;
        let begin_with = |timeout_ms: &str| {
            let (app, id, t) = (app.clone(), bot.id.clone(), timeout_ms.to_string());
            async move {
                let (st, Json(v)) = begin(State(app), bot_headers(), axum::extract::Form(SpawnBegin { bot_id: id, timeout_ms: t, purpose: String::new() })).await;
                assert_eq!(st, StatusCode::OK, "{v}");
                v["permit_id"].as_str().unwrap().to_string()
            }
        };
        let permit = begin_with("120000").await;
        app.credential_spawn_gate.lock().unwrap().age_permit(&bot.id, &permit, Duration::from_secs(100));
        assert_eq!(RotationFence::begin(&app, &bot.id).err(), Some(FenceError::SpawnsInFlight(1)), "100 秒時 120 秒 timeout 的 spawn 還在飛，要擋輪替");
        env.herdr.set_agent("slow-child", "w1:p9", true);
        env.herdr.live_pane("w1:p9", crate::testing::LivePane { rows: Some(40), ..Default::default() });
        let (st, Json(v)) = finish(
            State(app.clone()),
            bot_headers(),
            axum::extract::Form(SpawnFinish { bot_id: bot.id.clone(), permit_id: permit.clone(), pane_id: "w1:p9".into(), purpose: String::new() }),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "pane 開好了、finish 要登記成功：{v}");

        // 沒帶 timeout（split／tab create、舊 shim）維持 60 秒。
        let quick = begin_with("").await;
        app.credential_spawn_gate.lock().unwrap().age_permit(&bot.id, &quick, PERMIT_TTL + Duration::from_secs(1));
        assert!(RotationFence::begin(&app, &bot.id).is_ok(), "沒帶 timeout 的 permit 過 60 秒就不擋輪替");
    }

    #[tokio::test]
    async fn a_spawn_permit_cannot_register_a_pane_after_its_host_is_repointed() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let host = format!("spawn-repoint-{}", crate::db::ulid().to_ascii_lowercase());
        let cfg = |ssh: &str| crate::config::HostCfg { daemon_url: None,
            shared_session: false,
            name: host.clone(),
            ssh: ssh.into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        };
        app.hosts.insert_remote_for_test(cfg("host-a")).await;
        sqlx::query("UPDATE projects SET host=? WHERE id=?").bind(&host).bind(&env.project_id).execute(&app.db).await.unwrap();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "spawn-repoint").await;
        sqlx::query("INSERT INTO runs (id, bot_id, state, agent_status, pane_id, started_at) VALUES (?,?,'running','idle','w1:parent',?)")
            .bind(crate::db::ulid())
            .bind(&bot.id)
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .unwrap();

        let (status, Json(begin_body)) = begin(
            State(app.clone()),
            bot_headers(),
            axum::extract::Form(SpawnBegin { bot_id: bot.id.clone(), timeout_ms: String::new(), purpose: String::new() }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{begin_body}");
        let permit_id = begin_body["permit_id"].as_str().unwrap().to_string();

        app.hosts.replace_remote_for_test(&app, cfg("host-b")).await;
        let (status, Json(finish_body)) = finish(
            State(app.clone()),
            bot_headers(),
            axum::extract::Form(SpawnFinish {
                bot_id: bot.id.clone(),
                permit_id,
                pane_id: "w1:pane-7".into(),
                purpose: "child".into(),
            }),
        )
        .await;

        assert_eq!(status, StatusCode::CONFLICT, "a permit from the retired host must be rejected: {finish_body}");
        assert_eq!(finish_body["reason"], "spawn_host_superseded");
        let registered: i64 = sqlx::query_scalar("SELECT count(*) FROM panes WHERE host=? AND pane_id='w1:pane-7'")
            .bind(&host)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(registered, 0, "a retired host's pane must not be claimed on its replacement");
    }

    /// 有效期有上限（herdr 自己最多等 300 秒）：亂填的 timeout 不能讓 permit 永遠擋輪替。
    #[test]
    fn the_permit_lifetime_is_bounded() {
        assert_eq!(permit_ttl(""), PERMIT_TTL);
        assert_eq!(permit_ttl("abc"), PERMIT_TTL);
        assert_eq!(permit_ttl("30000"), PERMIT_TTL, "預設 30 秒的啟動：60 秒綽綽有餘");
        assert_eq!(permit_ttl("120000"), Duration::from_secs(150));
        assert_eq!(permit_ttl("999999999999"), MAX_PERMIT_TTL);
    }
