
    use super::*;
    use crate::runners::update_watch::{forget_disk_version, sweep, sweep_runs};
    use crate::state::App;
    use std::sync::Arc;

    fn fixture(name: &str) -> String {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let candidates = [
            manifest.join("src/lifecycle/fixtures"),
            manifest.join("../crates/am-lifecycle/src/lifecycle/fixtures"),
            manifest.join("../am-lifecycle/src/lifecycle/fixtures"),
        ];
        let path = candidates.iter().map(|dir| dir.join(name)).find(|path| path.is_file()).expect("fixture exists");
        std::fs::read_to_string(path).unwrap()
    }

    /// 2026-09-12 使用者實測：跑著 2.1.267、磁碟上 2.1.269。
    #[test]
    fn a_newer_version_on_disk_is_an_update_waiting_for_a_restart() {
        let n = version_notice("2.1.269", "2.1.267").expect("磁碟比較新 = 有更新");
        assert!(n.contains("2.1.269") && n.contains("2.1.267"), "兩個版本都要寫出來：{n}");
    }

    #[test]
    fn same_or_older_on_disk_is_not_an_update() {
        assert!(version_notice("2.1.267", "2.1.267").is_none());
        // 磁碟比較舊（降版、或探到別的 PATH）不是「有更新」，不要叫使用者重啟。
        assert!(version_notice("2.1.266", "2.1.267").is_none());
    }

    /// 版本號位數不同也要比得對：2.1.9 < 2.1.10（字串比較會給反的答案）。
    #[test]
    fn versions_compare_numerically_not_as_strings() {
        assert!(version_notice("2.1.10", "2.1.9").is_some());
        assert!(version_notice("2.1.9", "2.1.10").is_none());
    }

    #[test]
    fn unparsable_versions_are_silent() {
        assert!(version_notice("", "2.1.267").is_none());
        assert!(version_notice("2.1.269", "unknown").is_none());
    }

    #[test]
    fn running_version_comes_from_the_statusline_payload() {
        assert_eq!(running_version(Some(r#"{"version":"2.1.267 (Claude Code)","model_name":"Opus"}"#)).as_deref(), Some("2.1.267"));
        assert!(running_version(Some(r#"{"model_name":"Opus"}"#)).is_none());
        assert!(running_version(None).is_none());
    }

    // ── codex（issue #388）：整條 sweep 走過去，畫面用 MockHerdr 餵，`codex --version` 用預先種好的磁碟版本快取，不起真行程 ──

    const CODEX_MENU: &str = "\
>_ OpenAI Codex (v0.154.0)\n\n✨ Update available! 0.154.0 -> 0.155.1\n\n› 1. Update now (runs `npm install -g @openai/codex`)\n  2. Skip\n  3. Skip until next version\n";
    /// 磁碟版本快取已改掛 App（#759）；這幾條仍序列化，只是保守、成本低。
    /// 磁碟版本快取是全域的：這幾條測試各自種不同的版本，不能平行。
    fn serial() -> &'static tokio::sync::Mutex<()> {
        static M: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
        M.get_or_init(Default::default)
    }

    async fn seed_disk(app: &Arc<App>, host: &str, kind: &str, v: &str) {
        let fence = app.hosts.fence(host).await.expect("test host exists");
        app.disk_versions.lock().await.insert(
            format!("{kind}@{host}"),
            DiskVersionEntry { at: Instant::now(), authority: fence.authority_key(), version: Some(v.to_string()) },
        );
    }

    async fn notice_of(app: &Arc<App>, run_id: &str) -> Option<String> {
        db::run(&app.db, run_id).await.unwrap().unwrap().update_notice
    }

    async fn codex_bot_with_run(e: &crate::testing::Env) -> (String, String, String) {
        let bot = crate::testing::claude_bot(&e.app, &e.project_id, "cx").await;
        sqlx::query("UPDATE bots SET kind = 'codex' WHERE id = ?").bind(&bot.id).execute(&e.app.db).await.unwrap();
        let run = crate::testing::fake_run(&e.app, &bot.id).await;
        (bot.id.clone(), run, format!("pane-{}", bot.id))
    }

    fn remote_host_cfg(name: &str, target: &str) -> crate::config::HostCfg {
        crate::config::HostCfg {
            shared_session: false,
            name: name.into(),
            ssh: target.into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        }
    }

    #[tokio::test]
    async fn a_repointed_host_does_not_use_the_old_disk_version_cache_for_update_notices() {
        let _serial = serial().lock().await;
        let e = crate::testing::env().await;
        let host = "update-watch-597";
        let conn_a = e.app.hosts.insert_remote_for_test(remote_host_cfg(host, "target-a")).await;
        conn_a.connected.store(true, std::sync::atomic::Ordering::SeqCst); // 有 pane 在巡的主機是連著的；連不上的不讀磁碟版本
        let remote_herdr = crate::testing::MockHerdr::start(conn_a.client.socket_path().to_path_buf());
        let bot = crate::testing::claude_bot(&e.app, &e.project_id, "watch").await;
        let run = crate::testing::fake_run(&e.app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        remote_herdr.set_screen(&pane, "❯ hello\n");
        sqlx::query("UPDATE projects SET host = ? WHERE id = ?")
            .bind(host)
            .bind(&e.project_id)
            .execute(&e.app.db)
            .await
            .unwrap();
        sqlx::query("UPDATE runs SET status_json = ?, herdr_session = 'agents-manager' WHERE id = ?")
            .bind(r#"{"version":"2.1.0 (Claude Code)"}"#)
            .bind(&run)
            .execute(&e.app.db)
            .await
            .unwrap();

        let installed = Arc::new(std::sync::Mutex::new("2.2.0 (Claude Code)\n".to_string()));
        let probes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let installed_for_ssh = installed.clone();
        let probes_for_ssh = probes.clone();
        crate::hosts::set_ssh_fake(host, move |_| {
            probes_for_ssh.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(installed_for_ssh.lock().unwrap().clone())
        });

        sweep(&e.app).await;
        let cached_notice = notice_of(&e.app, &run).await.expect("A 的較新版本先建立通知");
        assert!(cached_notice.contains("2.2.0"), "A 版本要進入 notice：{cached_notice}");
        assert_eq!(probes.load(std::sync::atomic::Ordering::SeqCst), 1);

        *installed.lock().unwrap() = "2.0.0 (Claude Code)\n".to_string();
        e.app.hosts.replace_remote_for_test(&e.app, remote_host_cfg(host, "target-b")).await;
        e.app.hosts.get(host).await.unwrap().connected.store(true, std::sync::atomic::Ordering::SeqCst);
        sweep(&e.app).await;

        assert_eq!(probes.load(std::sync::atomic::Ordering::SeqCst), 2, "B 必須在 TTL 到期前重新讀版本");
        assert_eq!(notice_of(&e.app, &run).await, None, "B 的舊版本不可沿用 A 的 update_notice");
    }

    /// #714：同一輪巡邏順便讀出背景工作數、推 `bot_status`；run 結束之後不留帳。
    #[tokio::test]
    async fn the_sweep_reads_background_jobs_off_the_same_screen() {
        let _serial = serial().lock().await;
        let e = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&e.app, &e.project_id, "bg").await;
        let run = crate::testing::fake_run(&e.app, &bot.id).await;
        let screen = fixture("claude-2.1.281-background-shell.txt");
        e.herdr.set_screen(&format!("pane-{}", bot.id), &screen);
        let mut rx = e.app.subscribe();

        sweep(&e.app).await;
        assert_eq!(crate::background_jobs::get(&e.app, &run), 1);
        let frame = std::iter::from_fn(|| rx.try_recv().ok()).find(|f| f.kind == "bot_status").expect("推 bot_status");
        assert_eq!(frame.data["run"]["background_jobs"], 1);

        sqlx::query("UPDATE runs SET state = 'exited' WHERE id = ?").bind(&run).execute(&e.app.db).await.unwrap();
        sweep(&e.app).await;
        assert_eq!(crate::background_jobs::get(&e.app, &run), 0, "結束的 run 不留帳");
    }

    /// `delete_bot` 先定案（`deleted_at`）、再停機：停機那幾秒 bot 已經「沒了」但還在用它的 per-bot 行程帳（欠著的收尾寫入、
    /// 中斷標記…）。這時撞上 sweep 不能把帳清掉——名單要留著還有 active run 的軟刪 bot，停機完成（run 結束）後下一輪才清。
    #[tokio::test]
    async fn a_soft_deleted_bot_that_is_still_stopping_keeps_its_process_state() {
        let e = crate::testing::env().await;
        let stopping = crate::testing::claude_bot(&e.app, &e.project_id, "del-stopping").await;
        let stopped = crate::testing::claude_bot(&e.app, &e.project_id, "del-stopped").await;
        let alive = crate::testing::claude_bot(&e.app, &e.project_id, "alive").await;
        let run = crate::testing::fake_run(&e.app, &stopping.id).await;
        let old_run = crate::testing::fake_run(&e.app, &stopped.id).await;
        let long_ago = db::iso_in(-24 * 3600);
        for id in [&stopping.id, &stopped.id] {
            sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?").bind(&long_ago).bind(id).execute(&e.app.db).await.unwrap();
        }
        sqlx::query("UPDATE runs SET state = 'stopping' WHERE id = ?").bind(&run).execute(&e.app.db).await.unwrap();
        sqlx::query("UPDATE runs SET state = 'exited' WHERE id = ?").bind(&old_run).execute(&e.app.db).await.unwrap();

        let live = live_bot_ids(&e.app).await.unwrap();
        assert!(live.contains(&alive.id));
        assert!(live.contains(&stopping.id), "軟刪了、停機還沒完成（run 還 active）：帳要留著");
        assert!(!live.contains(&stopped.id), "停完很久了：可以清");

        sqlx::query("UPDATE runs SET state = 'exited' WHERE id = ?").bind(&run).execute(&e.app.db).await.unwrap();
        assert!(!live_bot_ids(&e.app).await.unwrap().contains(&stopping.id), "停機結束後下一輪才清");
    }

    /// 剛軟刪／退役的 bot 多留一段（可能馬上復原），退役很久的才不在名單裡；沒有 active run 也一樣。
    #[tokio::test]
    async fn a_recently_retired_bot_stays_in_the_live_set_for_a_while() {
        let e = crate::testing::env().await;
        let recent = crate::testing::claude_bot(&e.app, &e.project_id, "ret-recent").await;
        let old = crate::testing::claude_bot(&e.app, &e.project_id, "ret-old").await;
        sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?").bind(db::iso_in(-120)).bind(&recent.id).execute(&e.app.db).await.unwrap();
        sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?").bind(db::iso_in(-RETIRED_KEEP_SECS - 60)).bind(&old.id).execute(&e.app.db).await.unwrap();
        let live = live_bot_ids(&e.app).await.unwrap();
        assert!(live.contains(&recent.id), "兩分鐘前退役：可能馬上復原，帳留著");
        assert!(!live.contains(&old.id), "超過保留時間：清");
    }

    /// #767：「沒觀察過」跟「觀察過、是 0」是兩回事——前者（daemon 剛重啟、巡邏還沒輪到）沒有證據，一鍵重啟不擋、確認框標「未知」；
    /// 後者才是乾淨。API 的 `run.background_jobs`：沒觀察過是 `null`，不是 0。
    #[tokio::test]
    async fn an_unobserved_run_is_unknown_and_an_observed_clean_one_is_zero() {
        let _serial = serial().lock().await;
        let e = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&e.app, &e.project_id, "bgz").await;
        let run = crate::testing::fake_run(&e.app, &bot.id).await;
        let state_run = |app: &Arc<App>| {
            let r = Some(serde_json::json!({"id": run}));
            crate::background_jobs::run_json(app, &r, Some(run.as_str()))["background_jobs"].clone()
        };
        assert_eq!(crate::background_jobs::known(&e.app, &run), None);
        assert!(state_run(&e.app).is_null(), "沒觀察過：null");

        let screen = fixture("claude-2.1.281-no-background-shell.txt");
        e.herdr.set_screen(&format!("pane-{}", bot.id), &screen);
        let mut rx = e.app.subscribe();
        sweep(&e.app).await;
        assert_eq!(crate::background_jobs::known(&e.app, &run), Some(0), "看過、乾淨：0");
        assert_eq!(state_run(&e.app), 0);
        assert!(std::iter::from_fn(|| rx.try_recv().ok()).any(|f| f.kind == "bot_status"), "null → 0 也要推，確認框才不會一直停在「未知」");

        sqlx::query("UPDATE runs SET state = 'exited' WHERE id = ?").bind(&run).execute(&e.app.db).await.unwrap();
        sweep(&e.app).await;
        assert_eq!(crate::background_jobs::known(&e.app, &run), None, "結束的 run 不留帳");
    }

    /// #767：第一次看過也推 `bot_status`（null → 0）。daemon 剛重啟時每個 run 都是第一次，但每個 run 只推這一次：
    /// 之後的巡邏畫面沒變就不再推，不會變成每 30 秒一波事件風暴。
    #[tokio::test]
    async fn the_first_observation_of_many_runs_pushes_once_each_and_then_stays_quiet() {
        let _serial = serial().lock().await;
        let e = crate::testing::env().await;
        let screen = fixture("claude-2.1.281-no-background-shell.txt");
        let mut bots = Vec::new();
        for i in 0..5 {
            let bot = crate::testing::claude_bot(&e.app, &e.project_id, &format!("storm{i}")).await;
            crate::testing::fake_run(&e.app, &bot.id).await;
            e.herdr.set_screen(&format!("pane-{}", bot.id), &screen);
            bots.push(bot.id);
        }
        let mut rx = e.app.subscribe();
        let pushed = |rx: &mut tokio::sync::broadcast::Receiver<_>| -> usize {
            std::iter::from_fn(|| rx.try_recv().ok()).filter(|f: &crate::state::WsEvent| f.kind == "bot_status" && bots.iter().any(|b| f.data["bot_id"] == b.as_str())).count()
        };
        sweep(&e.app).await;
        assert_eq!(pushed(&mut rx), 5, "每個 run 第一次看過各推一次");
        sweep(&e.app).await;
        sweep(&e.app).await;
        assert_eq!(pushed(&mut rx), 0, "畫面沒變，之後的巡邏不再推");
    }

    /// #744：列舉 active run 失敗的那一輪不能清基準／背景工作帳；成功列舉出空清單才清。
    #[tokio::test]
    async fn a_failed_active_run_listing_keeps_baselines_and_background_jobs() {
        let _serial = serial().lock().await;
        let e = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&e.app, &e.project_id, "keep744").await;
        let run = crate::testing::fake_run(&e.app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        let screen = "❯ /model sonnet\n  ⎿  Set model to Sonnet 5.5 and saved as your default for new sessions\n";
        e.herdr.set_screen(&pane, screen);
        sweep(&e.app).await;
        let model = || async { db::run(&e.app.db, &run).await.unwrap().unwrap().runtime_model };
        assert_eq!(model().await, None, "第一次看到只當基準");
        let set_jobs = |n: u32| {
            crate::background_jobs::record(&mut e.app.background_jobs.lock().unwrap(), &run, n);
        };
        set_jobs(2);

        sweep_runs(&e.app, Err(anyhow::anyhow!("db is locked"))).await;
        assert_eq!(crate::background_jobs::get(&e.app, &run), 2, "讀失敗不清背景工作帳");

        e.herdr.set_screen(&pane, &format!("{screen}\n❯ /model haiku\n  ⎿  Set model to Haiku 4.5 and saved\n"));
        sweep(&e.app).await;
        assert_eq!(model().await.as_deref(), Some("claude-haiku-4-5"), "基準還在，之後的真切換被採用");

        set_jobs(2);
        sweep_runs(&e.app, Ok(vec![])).await;
        assert_eq!(crate::background_jobs::get(&e.app, &run), 0, "成功列舉出空清單才清");
        assert!(!crate::claude_live::has_baseline(&run), "成功列舉出空清單才清基準");
    }

    #[tokio::test]
    async fn a_codex_run_gets_a_pending_notice_that_says_it_must_be_installed_first() {
        let _serial = serial().lock().await;
        let e = crate::testing::env().await;
        let (_, run, pane) = codex_bot_with_run(&e).await;
        e.herdr.set_screen(&pane, CODEX_MENU);
        seed_disk(&e.app, "local", "codex", "codex-cli 0.154.0").await;
        sweep(&e.app).await;
        let n = notice_of(&e.app, &run).await.expect("codex 的提示要被巡到");
        assert!(n.contains("0.154.0") && n.contains("0.155.1") && n.contains("需安裝後重啟"), "{n}");
    }

    #[tokio::test]
    async fn claude_upstream_update_stays_visible_until_that_host_has_the_target_version() {
        let _serial = serial().lock().await;
        let e = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&e.app, &e.project_id, "claude-upstream").await;
        let run = crate::testing::fake_run(&e.app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        e.herdr.set_screen(&pane, "› conversation\n");
        sqlx::query("UPDATE runs SET status_json=? WHERE id=?")
            .bind(r#"{"version":"2.1.281 (Claude Code)"}"#)
            .bind(&run)
            .execute(&e.app.db)
            .await
            .unwrap();
        seed_disk(&e.app, "local", "claude", "2.1.281 (Claude Code)").await;
        crate::upstream_update::set_snapshot_for_test(&e.app.upstream_watch, crate::upstream_update::build_status(
            "claude",
            &Ok("2.1.284".into()),
            &[("local".into(), Ok("2.1.281 (Claude Code)".into()))],
            None,
        ))
        .await;

        sweep(&e.app).await;
        let pending = notice_of(&e.app, &run)
            .await
            .expect("上游比磁碟新時要持續顯示安裝提示");
        assert!(
            pending.contains("2.1.284") && pending.contains("需安裝"),
            "{pending}"
        );

        seed_disk(&e.app, "local", "claude", "2.1.284 (Claude Code)").await;
        sweep(&e.app).await;
        let installed = notice_of(&e.app, &run)
            .await
            .expect("裝到目標後改成重啟套用提示");
        assert!(
            installed.contains("已是 2.1.284") && installed.contains("重啟套用"),
            "{installed}"
        );
        assert!(!installed.contains("需安裝"), "安裝提示要清掉：{installed}");
    }

    #[tokio::test]
    async fn an_unreadable_disk_version_does_not_mark_a_claude_run_already_on_the_target_as_needing_install() {
        let _serial = serial().lock().await;
        let e = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&e.app, &e.project_id, "claude-unreadable-disk").await;
        let run = crate::testing::fake_run(&e.app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        e.herdr.set_screen(&pane, "› conversation\n");
        sqlx::query("UPDATE runs SET status_json=? WHERE id=?")
            .bind(r#"{"version":"2.1.284 (Claude Code)"}"#)
            .bind(&run)
            .execute(&e.app.db)
            .await
            .unwrap();
        // 磁碟版本讀不到（`claude --version` 逾時）：快取裡是 None，不是 2.1.281。
        let fence = e.app.hosts.fence("local").await.expect("test host exists");
        e.app.disk_versions.lock().await.insert(
            "claude@local".to_string(),
            DiskVersionEntry { at: Instant::now(), authority: fence.authority_key(), version: None },
        );
        // 別台主機落後，快照的 target 因此是 2.1.284；這台 local 自己其實已經在目標版。
        crate::upstream_update::set_snapshot_for_test(&e.app.upstream_watch, crate::upstream_update::build_status(
            "claude",
            &Ok("2.1.284".into()),
            &[
                ("local".into(), Ok("2.1.284 (Claude Code)".into())),
                ("other".into(), Ok("2.1.281 (Claude Code)".into())),
            ],
            None,
        ))
        .await;

        sweep(&e.app).await;
        assert_eq!(notice_of(&e.app, &run).await, None, "跑著的已是目標版，不該出現需安裝");

        // 反例：跑著的版本真的落後時，仍要提示需安裝。
        sqlx::query("UPDATE runs SET status_json=? WHERE id=?")
            .bind(r#"{"version":"2.1.281 (Claude Code)"}"#)
            .bind(&run)
            .execute(&e.app.db)
            .await
            .unwrap();
        sweep(&e.app).await;
        let pending = notice_of(&e.app, &run).await.expect("跑著的落後時要提示");
        assert!(pending.contains("需安裝"), "{pending}");
    }

    /// #1204：磁碟版本這一輪讀不到（`claude --version` 逾時、ssh 抖）不等於「還沒裝」：這顆 run 已經掛著「重啟套用」就沿用。
    /// 不拿跑著的舊版本改判成需安裝，也不因為這一輪比不出來就清掉（#974 同一條規則）。
    #[tokio::test]
    async fn an_unreadable_disk_version_keeps_a_claude_restart_notice() {
        let _serial = serial().lock().await;
        let e = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&e.app, &e.project_id, "claude-keep-restart").await;
        let run = crate::testing::fake_run(&e.app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        e.herdr.set_screen(&pane, "› conversation\n");
        let waiting = crate::update_watch::version_notice("2.1.284 (Claude Code)", "2.1.281").unwrap();
        sqlx::query("UPDATE runs SET status_json=?, update_notice=? WHERE id=?")
            .bind(r#"{"version":"2.1.281 (Claude Code)"}"#)
            .bind(&waiting)
            .bind(&run)
            .execute(&e.app.db)
            .await
            .unwrap();
        // 磁碟版本讀不到：快取裡是 None，不是 2.1.284。
        let fence = e.app.hosts.fence("local").await.expect("test host exists");
        e.app.disk_versions.lock().await.insert(
            "claude@local".to_string(),
            DiskVersionEntry { at: Instant::now(), authority: fence.authority_key(), version: None },
        );

        // 沒有上游快照：原本會把通知清成 NULL。
        sweep(&e.app).await;
        assert_eq!(notice_of(&e.app, &run).await.as_deref(), Some(waiting.as_str()), "磁碟讀不到時不清掉重啟套用");

        // 別台落後：原本會把重啟套用改成需安裝。
        crate::upstream_update::set_snapshot_for_test(&e.app.upstream_watch, crate::upstream_update::build_status(
            "claude",
            &Ok("2.1.284".into()),
            &[
                ("local".into(), Ok("2.1.284 (Claude Code)".into())),
                ("other".into(), Ok("2.1.281 (Claude Code)".into())),
            ],
            None,
        ))
        .await;
        sweep(&e.app).await;
        assert_eq!(notice_of(&e.app, &run).await.as_deref(), Some(waiting.as_str()), "也不改成需安裝");
    }

    #[tokio::test]
    async fn a_notice_written_during_the_sweep_is_not_overwritten_by_the_stale_snapshot() {
        let _serial = serial().lock().await;
        let e = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&e.app, &e.project_id, "claude-stale-snapshot").await;
        let run = crate::testing::fake_run(&e.app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        e.herdr.set_screen(&pane, "› conversation\n");
        sqlx::query("UPDATE runs SET status_json=? WHERE id=?")
            .bind(r#"{"version":"2.1.281 (Claude Code)"}"#)
            .bind(&run)
            .execute(&e.app.db)
            .await
            .unwrap();
        seed_disk(&e.app, "local", "claude", "2.1.281 (Claude Code)").await;
        crate::upstream_update::set_snapshot_for_test(&e.app.upstream_watch, crate::upstream_update::build_status(
            "claude",
            &Ok("2.1.284".into()),
            &[("local".into(), Ok("2.1.281 (Claude Code)".into()))],
            None,
        ))
        .await;

        // 掃描開頭讀到的舊列：notice 還是 NULL，所以這輪會算出「需安裝」。
        let stale = db::run(&e.app.db, &run).await.unwrap().unwrap();
        assert_eq!(stale.update_notice, None);
        // 掃描期間 cli_update 已裝好並把 notice 改成「已安裝，重啟套用」。
        let installed = crate::upstream_update::claude_installed_text("2.1.284", "2.1.281");
        sqlx::query("UPDATE runs SET update_notice=? WHERE id=?")
            .bind(&installed)
            .bind(&run)
            .execute(&e.app.db)
            .await
            .unwrap();

        sweep_runs(&e.app, Ok(vec![stale])).await;
        assert_eq!(notice_of(&e.app, &run).await.as_deref(), Some(installed.as_str()), "舊快照不能把剛裝好的蓋回需安裝");
    }

    #[tokio::test]
    async fn an_installed_claude_notice_survives_a_sweep_without_a_running_version() {
        let _serial = serial().lock().await;
        let e = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&e.app, &e.project_id, "claude-installed-no-version").await;
        let run = crate::testing::fake_run(&e.app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        e.herdr.set_screen(&pane, "› conversation\n");
        sqlx::query("UPDATE runs SET status_json = NULL, update_notice = ? WHERE id = ?")
            .bind(crate::upstream_update::claude_installed_text("2.1.284", "2.1.281"))
            .bind(&run)
            .execute(&e.app.db)
            .await
            .unwrap();
        seed_disk(&e.app, "local", "claude", "2.1.284 (Claude Code)").await;
        crate::upstream_update::set_snapshot_for_test(&e.app.upstream_watch, crate::upstream_update::build_status(
            "claude",
            &Ok("2.1.284".into()),
            &[("local".into(), Ok("2.1.284 (Claude Code)".into()))],
            None,
        ))
        .await;

        sweep(&e.app).await;

        let notice = notice_of(&e.app, &run)
            .await
            .expect("run 版本讀不到時，已安裝的重啟提示仍須持續存在");
        assert!(notice.contains("已安裝") && notice.contains("重啟套用"), "{notice}");
    }

    /// 畫面被推掉：選單／方框不在了，通知不消失；磁碟被人裝好之後改成「已安裝，重啟套用」。
    #[tokio::test]
    async fn when_the_prompt_leaves_the_screen_the_notice_stays_until_the_disk_has_the_new_version() {
        let _serial = serial().lock().await;
        let e = crate::testing::env().await;
        let (_, run, pane) = codex_bot_with_run(&e).await;
        e.herdr.set_screen(&pane, CODEX_MENU);
        seed_disk(&e.app, "local", "codex", "codex-cli 0.154.0").await;
        sweep(&e.app).await;
        let pending = notice_of(&e.app, &run).await.unwrap();
        e.herdr.set_screen(&pane, "› a long conversation now\n");
        sweep(&e.app).await;
        assert_eq!(notice_of(&e.app, &run).await, Some(pending), "提示被推掉不代表新版不存在");
        seed_disk(&e.app, "local", "codex", "codex-cli 0.155.1").await;
        sweep(&e.app).await;
        let n = notice_of(&e.app, &run).await.unwrap();
        assert!(n.contains("已安裝") && n.contains("重啟套用") && !n.contains("需安裝"), "{n}");
    }

    /// 只靠版本比對：從頭到尾沒看過提示，但看過啟動畫面的版本，磁碟後來是新的。
    #[tokio::test]
    async fn a_codex_run_with_no_prompt_at_all_is_noticed_by_the_version_comparison() {
        let _serial = serial().lock().await;
        let e = crate::testing::env().await;
        let (_, run, pane) = codex_bot_with_run(&e).await;
        e.herdr.set_screen(&pane, ">_ OpenAI Codex (v0.150.0)\n› hello\n");
        seed_disk(&e.app, "local", "codex", "codex-cli 0.150.0").await;
        sweep(&e.app).await;
        assert_eq!(notice_of(&e.app, &run).await, None);
        e.herdr.set_screen(&pane, "› later, the banner is gone\n");
        seed_disk(&e.app, "local", "codex", "codex-cli 0.151.2").await;
        sweep(&e.app).await;
        let n = notice_of(&e.app, &run).await.expect("版本比對補位");
        assert!(n.contains("0.151.2") && n.contains("0.150.0"), "{n}");
    }

    /// codex 新版**還沒安裝**時：批次不會自動重啟它（重啟一顆沒裝新版的 codex 換不到任何東西），
    /// 但它仍是候選——header 要看得到，只是被跳過並講清楚原因（2026-09-22：以前整顆連候選都不算，
    /// 這種還沒裝的 codex 有更新在 header 上完全消失，使用者以為只有 claude 會被巡）。
    #[tokio::test]
    async fn a_codex_notice_needing_install_is_a_candidate_but_is_skipped_not_restarted() {
        let _serial = serial().lock().await;
        let e = crate::testing::env().await;
        let (bot, run, pane) = codex_bot_with_run(&e).await;
        e.herdr.set_screen(&pane, CODEX_MENU);
        seed_disk(&e.app, "local", "codex", "codex-cli 0.154.0").await;
        sweep(&e.app).await;
        assert!(notice_of(&e.app, &run).await.is_some());
        let cands = crate::runners::bulk_restart::candidates(&e.app, None).await.unwrap();
        let mine = cands.iter().find(|c| c.bot_id == bot).expect("候選清單有它");
        assert!(mine.has_update && mine.needs_manual_install && crate::bulk_restart::is_candidate(mine));
        let (go, skip) = crate::bulk_restart::plan(&cands);
        assert!(go.iter().all(|c| c.bot_id != bot), "不會被自動重啟");
        let (_, why) = skip.iter().find(|(c, _)| c.bot_id == bot).expect("要在跳過清單裡才會出現在 header");
        assert_eq!(*why, crate::bulk_restart::Skip::NeedsManualInstall);
    }

    /// grok 框底的真畫面（grok 1.0.x，2026-10-03 g8 pane）：`/effort low` 之後框底跟著變。
    fn grok_screen(effort: &str) -> String {
        format!(
            "  ⏺ Switched to Grok 4.7 ({effort} effort)\n\n  ╭────────────────────────────────────────────────╮\n  │ ❯                                              │\n  ╰────────────── Grok 4.7 ({effort}) · always-approve ─╯\n\n  Shift+Tab:mode  │  Ctrl+.:shortcuts\n"
        )
    }

    async fn grok_bot_with_run(e: &crate::testing::Env, name: &str, managed_by: &str, effort: &str) -> (String, String, String) {
        let bot = crate::testing::claude_bot(&e.app, &e.project_id, name).await;
        sqlx::query("UPDATE bots SET kind = 'grok', managed_by = ?, model = 'grok-4.7', effort = ? WHERE id = ?")
            .bind(managed_by)
            .bind(effort)
            .bind(&bot.id)
            .execute(&e.app.db)
            .await
            .unwrap();
        let run = crate::testing::fake_run(&e.app, &bot.id).await;
        (bot.id.clone(), run, format!("pane-{}", bot.id))
    }

    /// 2026-10-03 mkng2n：`herdr agent start … --kind grok -- -m grok-4.7 --reasoning-effort high` 開的 child，
    /// 在 pane 裡 `/effort low` 之後側欄仍寫 grok-4.7-High：argv 只在收編時讀一次，框底才是現況。
    #[tokio::test]
    async fn a_grok_child_follows_an_effort_switch_typed_in_its_tui() {
        let _serial = serial().lock().await;
        let e = crate::testing::env().await;
        let (bot, run, pane) = grok_bot_with_run(&e, "gk-kid", "child", "high").await;
        e.herdr.set_screen(&pane, &grok_screen("low"));
        sweep(&e.app).await;
        let r = db::run(&e.app.db, &run).await.unwrap().unwrap();
        assert_eq!((r.runtime_model.as_deref(), r.runtime_effort.as_deref()), (Some("grok-4.7"), Some("low")));
        let b = db::bot(&e.app.db, &bot).await.unwrap().unwrap();
        assert_eq!(b.effort.as_deref(), Some("low"), "child 的設定是從 argv 抄來的，跟著 TUI 走（/api/state 的 effort）");

        // 讀不到框底（選單蓋住、畫面清掉）：沿用最後已知值，不清空。
        e.herdr.set_screen(&pane, "  Select effort\n  1. low\n  2. high\n");
        sweep(&e.app).await;
        let r = db::run(&e.app.db, &run).await.unwrap().unwrap();
        assert_eq!(r.runtime_effort.as_deref(), Some("low"));
    }

    /// 一般 bot：runtime 跟著框底，設定不動（重啟回設定值，畫成 drift）；runtime 本來未知（沒設強度＝CLI 預設）就不補，
    /// 免得多出一條「需重啟」的假 drift。
    #[tokio::test]
    async fn a_grok_user_bot_tracks_the_runtime_but_keeps_its_setting() {
        let _serial = serial().lock().await;
        let e = crate::testing::env().await;
        let (bot, run, pane) = grok_bot_with_run(&e, "gk-user", "user", "high").await;
        sqlx::query("UPDATE runs SET runtime_model = 'grok-4.7', runtime_effort = 'high' WHERE id = ?")
            .bind(&run)
            .execute(&e.app.db)
            .await
            .unwrap();
        e.herdr.set_screen(&pane, &grok_screen("low"));
        sweep(&e.app).await;
        let r = db::run(&e.app.db, &run).await.unwrap().unwrap();
        assert_eq!(r.runtime_effort.as_deref(), Some("low"));
        let b = db::bot(&e.app.db, &bot).await.unwrap().unwrap();
        assert_eq!(b.effort.as_deref(), Some("high"), "一般 bot 的設定不動");

        let (_, run2, pane2) = grok_bot_with_run(&e, "gk-default", "user", "high").await;
        e.herdr.set_screen(&pane2, &grok_screen("low"));
        sweep(&e.app).await;
        let r = db::run(&e.app.db, &run2).await.unwrap().unwrap();
        assert_eq!((r.runtime_model, r.runtime_effort), (None, None), "未知的 runtime 不拿畫面補");
    }

    /// codex child 在 TUI 換了模型／強度：runtime 早就跟著狀態列，child 的設定（/api/state、側欄）也要跟。
    #[tokio::test]
    async fn a_codex_child_follows_a_model_switch_on_its_status_line() {
        let _serial = serial().lock().await;
        let e = crate::testing::env().await;
        let (bot, run, pane) = codex_bot_with_run(&e).await;
        sqlx::query("UPDATE bots SET managed_by = 'child', model = 'gpt-6.1-sol', effort = 'high' WHERE id = ?")
            .bind(&bot)
            .execute(&e.app.db)
            .await
            .unwrap();
        sqlx::query("UPDATE runs SET runtime_model = 'gpt-6.1-sol', runtime_effort = 'high', runtime_fast = 0 WHERE id = ?")
            .bind(&run)
            .execute(&e.app.db)
            .await
            .unwrap();
        e.herdr.set_screen(&pane, "› Ask Codex\n\n  gpt-6-luna max · /tmp · Context 3% used · 5h 90% left\n");
        sweep(&e.app).await;
        let b = db::bot(&e.app.db, &bot).await.unwrap().unwrap();
        assert_eq!((b.model.as_deref(), b.effort.as_deref()), (Some("gpt-6-luna"), Some("max")));

        // 網頁改了 child 設定還沒套用：狀態列沒變，不能把設定蓋回去。
        sqlx::query("UPDATE bots SET effort = 'low' WHERE id = ?").bind(&bot).execute(&e.app.db).await.unwrap();
        sweep(&e.app).await;
        let b = db::bot(&e.app.db, &bot).await.unwrap().unwrap();
        assert_eq!(b.effort.as_deref(), Some("low"));
    }

    fn pane_reads(e: &crate::testing::Env) -> usize {
        e.herdr.calls_to("pane.read").len()
    }

    /// 框底沒變：一輪只讀一次，不再握鎖重讀。變了才重讀（套用可能已把畫面換掉）。
    #[tokio::test]
    async fn an_unchanged_grok_footer_is_not_read_twice() {
        let _serial = serial().lock().await;
        let e = crate::testing::env().await;
        let (_, run, pane) = grok_bot_with_run(&e, "gk-quiet", "user", "high").await;
        sqlx::query("UPDATE runs SET runtime_model = 'grok-4.7', runtime_effort = 'high' WHERE id = ?")
            .bind(&run)
            .execute(&e.app.db)
            .await
            .unwrap();
        e.herdr.set_screen(&pane, &grok_screen("high"));
        sweep(&e.app).await;
        assert_eq!(pane_reads(&e), 1, "沒有落差不再讀第二次");

        e.herdr.set_screen(&pane, &grok_screen("low"));
        sweep(&e.app).await;
        assert_eq!(pane_reads(&e), 3, "這一輪先讀到落差，鎖內再讀一次");
        let r = db::run(&e.app.db, &run).await.unwrap().unwrap();
        assert_eq!(r.runtime_effort.as_deref(), Some("low"));
    }

    /// 網頁把 child 強度改成 low、TUI 框底仍是 high：這一輪不能把設定蓋回 high。
    #[tokio::test]
    async fn a_web_edit_on_a_grok_child_survives_an_unchanged_footer() {
        let _serial = serial().lock().await;
        let e = crate::testing::env().await;
        let (bot, run, pane) = grok_bot_with_run(&e, "gk-web", "child", "low").await;
        sqlx::query("UPDATE runs SET runtime_model = 'grok-4.7', runtime_effort = 'high' WHERE id = ?")
            .bind(&run)
            .execute(&e.app.db)
            .await
            .unwrap();
        e.herdr.set_screen(&pane, &grok_screen("high"));
        sweep(&e.app).await;
        let b = db::bot(&e.app.db, &bot).await.unwrap().unwrap();
        assert_eq!(b.effort.as_deref(), Some("low"), "狀態列沒變，網頁上的設定留著");
    }

    /// 回覆裡的舊狀態列不把母 bot 的 runtime 改掉，需重啟不會多一條假的模型落差。
    #[tokio::test]
    async fn a_quoted_codex_status_line_does_not_mark_the_parent_for_restart() {
        let _serial = serial().lock().await;
        let e = crate::testing::env().await;
        let (bot, run, pane) = codex_bot_with_run(&e).await;
        sqlx::query("UPDATE bots SET model = 'gpt-6.1-sol', effort = 'high' WHERE id = ?")
            .bind(&bot)
            .execute(&e.app.db)
            .await
            .unwrap();
        sqlx::query("UPDATE runs SET runtime_model = 'gpt-6.1-sol', runtime_effort = 'high', runtime_fast = 0 WHERE id = ?")
            .bind(&run)
            .execute(&e.app.db)
            .await
            .unwrap();
        let mut screen = "  gpt-6-luna max · /tmp · Context 90% used\n".to_string();
        screen.push_str(&"  notes\n".repeat(12));
        screen.push_str("  gpt-6.1-sol high · /tmp · Context 3% used\n");
        e.herdr.set_screen(&pane, &screen);
        sweep(&e.app).await;
        let r = db::run(&e.app.db, &run).await.unwrap().unwrap();
        assert_eq!((r.runtime_model.as_deref(), r.runtime_effort.as_deref()), (Some("gpt-6.1-sol"), Some("high")));
        let b = db::bot(&e.app.db, &bot).await.unwrap().unwrap();
        assert_eq!((b.model.as_deref(), b.effort.as_deref()), (Some("gpt-6.1-sol"), Some("high")));
    }
