
    //! 每道守衛各有一條會因為拿掉它而紅的測試；共用與不共用兩種都跑，看得出差別只在旗標。
    use crate::db;
    use crate::herdr::HerdrClient;
    use crate::state::App;
    use crate::testing as tt;
    use serde_json::{json, Value};
    use std::sync::Arc;

    pub(crate) const HOST: &str = "sh1";

    /// 另一顆 daemon 也在用的遠端主機：自己的一個 mock herdr，設定裡 `shared_session = true`。
    pub(crate) struct Shared {
        pub herdr: tt::MockHerdr,
        pub client: HerdrClient,
        pub project_id: String,
    }

    pub(crate) async fn shared_host(env: &tt::Env, shared: bool) -> Shared {
        let app = &env.app;
        let sock = env.dir.join("sh1.sock");
        let herdr = tt::MockHerdr::start(sock.clone());
        let cfg = crate::config::HostCfg {
            name: HOST.into(),
            ssh: "sh1.invalid".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "test".into(),
            remote_path: String::new(),
            shared_session: shared,
        };
        let conn = app.hosts.insert_remote_with_client_for_test(cfg.clone(), HerdrClient::new(sock.clone())).await;
        conn.connected.store(true, std::sync::atomic::Ordering::SeqCst);
        app.cfg
            .update(move |c| {
                c.hosts.retain(|h| h.name != HOST);
                c.hosts.push(cfg);
                Ok(())
            })
            .await
            .unwrap();
        let project_id = db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, '/tmp/sh', 'sh', ?, ?)")
            .bind(&project_id)
            .bind(HOST)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        Shared { herdr, client: HerdrClient::new(sock), project_id }
    }

    pub(crate) async fn set_shared(app: &Arc<App>, shared: bool) {
        app.cfg
            .update(move |c| {
                for h in c.hosts.iter_mut().filter(|h| h.name == HOST) {
                    h.shared_session = shared;
                }
                Ok(())
            })
            .await
            .unwrap();
    }

    fn entry(name: &str, ws: &str, tab: &str, pane: &str) -> Value {
        json!({"name": name, "agent": "claude", "agent_status": "idle",
               "workspace_id": ws, "tab_id": tab, "pane_id": pane, "cwd": "/tmp/sh"})
    }

    async fn run_row(app: &Arc<App>, bot: &str, state: &str, ws: &str, tab: &str, pane: &str, agent: &str) {
        let ended = (state != "running").then(|| db::iso_in(-600));
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, started_at, ended_at)
             VALUES (?,?,?,'idle',?,?,?,?,'test',?,?)",
        )
        .bind(db::ulid())
        .bind(bot)
        .bind(state)
        .bind(ws)
        .bind(tab)
        .bind(pane)
        .bind(agent)
        .bind(db::iso_in(-1200))
        .bind(ended)
        .execute(&app.db)
        .await
        .unwrap();
    }

    async fn children(app: &Arc<App>) -> Vec<String> {
        sqlx::query_scalar("SELECT name FROM bots WHERE managed_by = 'child' AND deleted_at IS NULL ORDER BY name")
            .fetch_all(&app.db)
            .await
            .unwrap()
    }

    /// 別顆 daemon 的 agent 名字剛好是 `<我的 bot>-<字尾>`、開在它自己的 workspace：共用主機上不認領；
    /// 同名前綴、開在我自己 workspace 裡的照常認領。不共用時兩顆都認領（前綴跨 tab 本來就算）。
    #[tokio::test]
    async fn a_foreign_agent_with_our_prefix_is_not_claimed_on_a_shared_host() {
        let env = tt::env().await;
        let app = env.app.clone();
        let sh = shared_host(&env, true).await;
        let (mine, root) = sh.client.workspace_create("/tmp/sh", "sh", json!({})).await.unwrap();
        let kid = sh.client.tab_create(&mine.workspace_id, "/tmp/sh", "kid", json!({})).await.unwrap();
        let (theirs, their_root) = sh.client.workspace_create("/tmp/x", "x", json!({})).await.unwrap();
        let bot = tt::claude_bot(&app, &sh.project_id, "alfa").await;
        let agent = crate::config::agent_name("sh", &bot.id);
        run_row(&app, &bot.id, "running", &mine.workspace_id, &root.tab_id, &root.pane_id, &agent).await;
        *sh.herdr.agents.lock().unwrap() = vec![
            entry(&agent, &mine.workspace_id, &root.tab_id, &root.pane_id),
            entry(&format!("{agent}-ui"), &mine.workspace_id, &kid.tab_id, &kid.pane_id),
            entry(&format!("{agent}-x"), &theirs.workspace_id, &their_root.tab_id, &their_root.pane_id),
        ];

        crate::reconcile::reconcile_host(&app, HOST).await.unwrap();
        assert_eq!(children(&app).await, ["ui"], "只認領自己 workspace 裡的");

        set_shared(&app, false).await;
        crate::reconcile::reconcile_host(&app, HOST).await.unwrap();
        assert_eq!(children(&app).await, ["ui", "x"], "不共用時前綴跨 workspace 照舊算");
    }

    /// 結束的 run 的 pane id 被別顆 daemon 的新 pane 用到（在它的 workspace 裡）：共用主機上不當孤兒關。
    #[tokio::test]
    async fn a_reused_pane_id_in_a_foreign_workspace_is_not_closed_on_a_shared_host() {
        let env = tt::env().await;
        let app = env.app.clone();
        let sh = shared_host(&env, true).await;
        let (theirs, their_root) = sh.client.workspace_create("/tmp/x", "x", json!({})).await.unwrap();
        let bot = tt::claude_bot(&app, &sh.project_id, "alfa").await;
        run_row(&app, &bot.id, "exited", "w-old", "t-old", &their_root.pane_id, "gone").await;
        sh.herdr.agents.lock().unwrap().clear();

        crate::reconcile::reconcile_host(&app, HOST).await.unwrap();
        assert!(sh.client.pane_get(&their_root.pane_id).await.unwrap().is_some(), "別人的 pane 不關");

        set_shared(&app, false).await;
        crate::reconcile::reconcile_host(&app, HOST).await.unwrap();
        assert!(sh.client.pane_get(&their_root.pane_id).await.unwrap().is_none(), "不共用時照舊當孤兒關");
        let _ = theirs;
    }

    /// pane 掃描：共用主機上別人的 pane 不進 `panes`（不 GC、不通知、不當 scratch）；自己 workspace 的照收。
    #[tokio::test]
    async fn only_our_panes_are_scanned_on_a_shared_host() {
        let env = tt::env().await;
        let app = env.app.clone();
        let sh = shared_host(&env, true).await;
        sqlx::query("UPDATE projects SET workspace_id = 'wM' WHERE id = ?").bind(&sh.project_id).execute(&app.db).await.unwrap();
        let pane = |id: &str, ws: &str| json!({"pane_id": id, "workspace_id": ws, "tab_id": format!("{ws}:t1"), "cwd": "/home/u", "agent": null, "revision": 1});
        let snapshot = json!({
            "workspaces": [{"workspace_id": "wM", "label": "sh"}, {"workspace_id": "wX", "label": "x"}],
            "panes": [pane("wM:p1", "wM"), pane("wX:p1", "wX")],
        });
        crate::panes::scan_snapshot(&app, HOST, &snapshot).await.unwrap();
        let ids: Vec<String> = sqlx::query_scalar("SELECT pane_id FROM panes ORDER BY pane_id").fetch_all(&app.db).await.unwrap();
        assert_eq!(ids, ["wM:p1"]);
        crate::panes::notify_unowned_and_orphans(&app, HOST).await.unwrap();
        let notified: Vec<String> =
            sqlx::query_scalar("SELECT pane_id FROM panes WHERE unowned_notified_at IS NOT NULL").fetch_all(&app.db).await.unwrap();
        assert_eq!(notified, ["wM:p1"], "別人的 pane 不推 pane_unowned");

        set_shared(&app, false).await;
        crate::panes::scan_snapshot(&app, HOST, &snapshot).await.unwrap();
        let ids: Vec<String> = sqlx::query_scalar("SELECT pane_id FROM panes ORDER BY pane_id").fetch_all(&app.db).await.unwrap();
        assert_eq!(ids, ["wM:p1", "wX:p1"]);
    }

    /// 遠端 bot 目錄：共用主機上一律不搬（掃描不動、刪除時那一趟也不動、不記成欠著）。
    #[tokio::test]
    async fn remote_bot_dirs_are_left_alone_on_a_shared_host() {
        let env = tt::env().await;
        let app = env.app.clone();
        let sh = shared_host(&env, true).await;
        let bot = tt::claude_bot(&app, &sh.project_id, "alfa").await;
        sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?").bind(db::now()).bind(&bot.id).execute(&app.db).await.unwrap();

        assert_eq!(crate::runners::remote_purge::sweep(&app, HOST).await, (0, 0));
        assert!(!crate::lifecycle::purge_bot_dir(&app, &bot.id, HOST).await);
        let marks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM remote_bot_dir_purges").fetch_one(&app.db).await.unwrap();
        assert_eq!(marks, 0, "沒去碰，也不記成欠著");
    }

    /// 移交出去的專案（#708）的 bot 目錄：刪除那一趟與開機清掃都不動（接手的 daemon 可能在用）。
    #[tokio::test]
    async fn a_handed_off_bots_dir_is_never_purged_here() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
        let dir = app.bot_dir(&bot.id).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        sqlx::query("UPDATE projects SET handed_off_to = 'agm-host' WHERE id = ?").bind(&env.project_id).execute(&app.db).await.unwrap();
        assert!(!crate::lifecycle::purge_bot_dir(&app, &bot.id, crate::config::LOCAL_HOST).await);
        assert!(dir.exists());

        sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?").bind(db::now()).bind(&bot.id).execute(&app.db).await.unwrap();
        crate::lifecycle::purge_deleted_bot_dirs(&app).await;
        assert!(dir.exists(), "開機清掃也不動");

        sqlx::query("UPDATE projects SET handed_off_to = NULL WHERE id = ?").bind(&env.project_id).execute(&app.db).await.unwrap();
        crate::lifecycle::purge_deleted_bot_dirs(&app).await;
        assert!(!dir.exists(), "收回之後照舊清");
    }

    /// 探測標記：共用主機才有，重啟後不變（存在資料目錄）。
    #[tokio::test]
    async fn the_probe_tag_is_only_for_shared_hosts_and_survives_a_restart() {
        let env = tt::env().await;
        let app = env.app.clone();
        shared_host(&env, true).await;
        let tag = super::probe_tag(&app, HOST).await.expect("共用主機有標記");
        assert_eq!(super::probe_tag(&app, HOST).await.as_deref(), Some(tag.as_str()));
        assert_eq!(super::daemon_tag(&app.data_dir), tag, "存在資料目錄，重啟後同一個");
        assert_eq!(super::probe_tag(&app, crate::config::LOCAL_HOST).await, None);
        set_shared(&app, false).await;
        assert_eq!(super::probe_tag(&app, HOST).await, None);
    }

    /// 「自己的」除了 run 與專案 workspace，還有剛開的 child（spawn hint）、預覽 pane、自己開的 host shell；
    /// 移交出去的專案（#708）不算。
    #[tokio::test]
    async fn owned_covers_hints_previews_and_host_shells_but_not_handed_off_projects() {
        let env = tt::env().await;
        let app = env.app.clone();
        let sh = shared_host(&env, true).await;
        let bot = tt::claude_bot(&app, &sh.project_id, "alfa").await;
        sqlx::query("INSERT INTO spawn_hints (pane_id, host, bot_id, created_at) VALUES ('wK:p1', ?, ?, ?)")
            .bind(HOST)
            .bind(&bot.id)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO bot_previews (bot_id, host, pane_id, status, updated_at) VALUES (?, ?, 'wP:p1', 'running', ?)")
            .bind(&bot.id)
            .bind(HOST)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        app.host_shells.lock().await.push(crate::api::shell::HostShell {
            host: HOST.into(),
            herdr_session: "test".into(),
            workspace_id: "wS".into(),
            tab_id: "wS:t1".into(),
            pane_id: "wS:p1".into(),
            cwd: "/tmp".into(),
            created_at: db::now(),
        });
        let theirs = db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, workspace_id, handed_off_to, created_at) VALUES (?, '/tmp/t', 't', ?, 'wT', 'agm-host', ?)")
            .bind(&theirs)
            .bind(HOST)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let o = super::owned(&app, HOST).await.unwrap();
        for pane in ["wK:p1", "wP:p1", "wS:p1"] {
            assert!(o.panes.contains(pane), "{pane}");
        }
        assert!(o.workspaces.contains("wS"));
        assert!(!o.workspaces.contains("wT"), "移交出去的專案不是自己的");
    }

    /// Handoff removes a project's workspace and active runs from the local ownership set. Its
    /// auxiliary pane records must be filtered by the same ownership boundary on shared sessions.
    #[tokio::test]
    async fn owned_excludes_hints_and_previews_from_handed_off_projects() {
        let env = tt::env().await;
        let app = env.app.clone();
        let sh = shared_host(&env, true).await;
        let mine = tt::claude_bot(&app, &sh.project_id, "mine").await;
        let handed_off = db::ulid();
        sqlx::query(
            "INSERT INTO projects (id, path, label, host, handed_off_to, created_at) VALUES (?, '/tmp/handed-off', 'old', ?, 'other-daemon', ?)",
        )
        .bind(&handed_off)
        .bind(HOST)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let theirs = tt::claude_bot(&app, &handed_off, "theirs").await;
        for (bot, pane) in [(&mine, "wM:p1"), (&theirs, "wT:p1")] {
            sqlx::query("INSERT INTO spawn_hints (pane_id, host, bot_id, created_at) VALUES (?, ?, ?, ?)")
                .bind(pane)
                .bind(HOST)
                .bind(&bot.id)
                .bind(db::now())
                .execute(&app.db)
                .await
                .unwrap();
            sqlx::query("INSERT INTO bot_previews (bot_id, host, pane_id, status, updated_at) VALUES (?, ?, ?, 'running', ?)")
                .bind(&bot.id)
                .bind(HOST)
                .bind(pane.replace(":p", ":preview"))
                .bind(db::now())
                .execute(&app.db)
                .await
                .unwrap();
        }

        let owned = super::owned(&app, HOST).await.unwrap();
        assert!(owned.panes.contains("wM:p1"), "own spawn hint stays owned");
        assert!(owned.panes.contains("wM:preview1"), "own preview stays owned");
        assert!(!owned.panes.contains("wT:p1"), "handed-off spawn hint belongs to the other daemon");
        assert!(!owned.panes.contains("wT:preview1"), "handed-off preview belongs to the other daemon");
    }

    #[test]
    fn only_our_tagged_probes_are_sweepable_on_a_shared_host() {
        let is_probe = |l: &str| l.starts_with("am-quota-claude");
        assert!(super::sweepable("am-quota-claude", None, is_probe), "一般主機照舊全清");
        assert!(super::sweepable("am-quota-claude@x", None, is_probe));
        assert!(super::sweepable("am-quota-claude-cc1@mine", Some("mine"), is_probe));
        assert!(!super::sweepable("am-quota-claude@other", Some("mine"), is_probe));
        assert!(!super::sweepable("am-quota-claude", Some("mine"), is_probe), "沒標記的可能是別顆舊版 daemon 的");
        assert!(!super::sweepable("proj@mine", Some("mine"), is_probe));
    }
