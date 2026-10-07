
    use super::*;
    use crate::db;
    use crate::runners::background_hook::on_stop;
    use crate::testing as tt;

    fn fixture() -> Value {
        let path = format!("{}/src/lifecycle/fixtures/claude-2.1.287-stop-background-tasks.json", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn the_real_payload_shape_parses_and_an_absent_key_means_an_old_claude() {
        let r = parse(&fixture()).expect("2.1.287 payload");
        assert_eq!(r.tasks.len(), 2);
        assert_eq!((r.tasks[0].kind.as_str(), r.tasks[0].status.as_str()), ("shell", "running"));
        assert_eq!(r.tasks[0].command.as_deref(), Some("sleep 600 && echo done"));
        assert_eq!(r.crons, vec![Cron { id: "c1".into(), schedule: "30 14 2 10 *".into(), recurring: false, prompt: "check the build".into() }]);

        let mut old = fixture();
        old.as_object_mut().unwrap().remove("background_tasks");
        assert_eq!(parse(&old), None, "舊版 claude 沒有這個鍵：不是『零個』，是『沒說』");
        let mut empty = fixture();
        empty["background_tasks"] = json!([]);
        assert_eq!(parse(&empty).map(|r| r.tasks.len()), Some(0), "空陣列＝說了：沒有");
        let mut odd = fixture();
        odd["background_tasks"] = json!("nope");
        assert_eq!(parse(&odd), None, "形狀不對當沒說（退回畫面）");
        // 欄位缺的元素照收：type 不明就叫 unknown，仍然算一個在跑的工作。
        let mut sparse = fixture();
        sparse["background_tasks"] = json!([{"id": "x"}, 7]);
        assert_eq!(parse(&sparse).unwrap().tasks.len(), 1, "非物件的元素略過");
        assert_eq!(parse(&sparse).unwrap().tasks[0].kind, "unknown");
    }

    #[test]
    fn only_shells_can_be_services_and_everything_else_counts() {
        let mut p = fixture();
        p["background_tasks"] = json!([
            {"id": "1", "type": "shell", "status": "running", "description": "a"},
            {"id": "2", "type": "shell", "status": "running", "description": "dev server"},
            {"id": "3", "type": "subagent", "status": "running", "description": "review", "agent_type": "Explore"},
        ]);
        let mut s = Snapshot { at: Instant::now(), reported: parse(&p).unwrap(), services: None };
        assert_eq!(s.jobs(), 3, "服務還沒查到：不扣");
        s.services = Some(1);
        assert_eq!(s.jobs(), 2);
        s.services = Some(9);
        assert_eq!(s.jobs(), 1, "扣到只剩非 shell 為止，不會變負的");
    }

    async fn setup() -> (tt::Env, db::Run) {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        let run_id = tt::fake_run(&env.app, &bot.id).await;
        let run = db::run(&env.app.db, &run_id).await.unwrap().unwrap();
        (env, run)
    }

    /// Stop 一到就有數字、清單，並推 `bot_status`；下一則 Stop（空陣列）立刻歸零；沒有這個鍵的 Stop 什麼都不動。
    #[tokio::test]
    async fn a_stop_with_background_tasks_sets_the_count_and_the_details_at_once() {
        let (env, run) = setup().await;
        let app = env.app.clone();
        let mut rx = app.subscribe();
        assert_eq!(crate::background_jobs::known(&app, &run.id), None);

        on_stop(&app, &run, &fixture()).await;
        assert_eq!(crate::background_jobs::known(&app, &run.id), Some(2), "不用等 30 秒巡邏");
        let frame = rx.try_recv().expect("數字變了要推 bot_status");
        assert_eq!(frame.kind, "bot_status");
        let v = crate::background_jobs::run_json(&app, &Some(run.clone()), Some(&run.id));
        assert_eq!(v["background_jobs"], 2);
        assert_eq!(v["background_source"], "hook");
        assert_eq!(v["background_tasks"][0]["description"], "wait for the remote build");
        assert_eq!(v["background_tasks"][0]["type"], "shell");
        assert_eq!(v["session_crons"][0]["schedule"], "30 14 2 10 *");

        let mut none = fixture();
        none.as_object_mut().unwrap().remove("background_tasks");
        on_stop(&app, &run, &none).await;
        assert_eq!(crate::background_jobs::known(&app, &run.id), Some(2), "沒有這個鍵：不動，留給畫面判斷");

        let mut empty = fixture();
        empty["background_tasks"] = json!([]);
        on_stop(&app, &run, &empty).await;
        assert_eq!(crate::background_jobs::known(&app, &run.id), Some(0));
        let v = crate::background_jobs::run_json(&app, &Some(run.clone()), Some(&run.id));
        assert_eq!(v["background_tasks"], json!([]), "報過『沒有』也是證據：清單是空的、不是 null");
        assert_eq!(v["session_crons"].as_array().map(Vec::len), Some(1));
    }

    #[tokio::test]
    async fn the_screen_does_not_overrule_a_fresh_hook_report_but_does_overrule_a_stale_one() {
        let (env, run) = setup().await;
        let app = env.app.clone();
        on_stop(&app, &run, &fixture()).await;
        // 畫面（還沒更新）說 0：剛報的 hook 帳不被推翻。
        assert_eq!(reconcile(&app, &run.id, 0), 2);
        // 過了寬限、畫面仍然完全沒有背景：背景在沒有新 Stop 的情況下結束了，丟掉 hook 帳。
        app.background_hook.lock().unwrap().get_mut(&run.id).unwrap().at = Instant::now() - GRACE - Duration::from_secs(1);
        assert_eq!(reconcile(&app, &run.id, 0), 0);
        assert_eq!(details(&app, &run.id), (Value::Null, Value::Null), "丟掉就沒有清單了");
        // 沒有 hook 帳＝畫面的數字。
        assert_eq!(reconcile(&app, &run.id, 3), 3);

        // 過了寬限，畫面有一個 shell：以現場數字取代舊 hook 的兩個 shell。
        on_stop(&app, &run, &fixture()).await;
        app.background_hook.lock().unwrap().get_mut(&run.id).unwrap().at = Instant::now() - GRACE - Duration::from_secs(1);
        assert_eq!(reconcile(&app, &run.id, 1), 1);
        assert_eq!(details(&app, &run.id), (Value::Null, Value::Null), "stale shell-only details are dropped");
        // hook 說 0：寬限內畫面說 1 也是 0；過了寬限就退場、只看畫面。
        let mut empty = fixture();
        empty["background_tasks"] = json!([]);
        on_stop(&app, &run, &empty).await;
        assert_eq!(reconcile(&app, &run.id, 1), 0);
        app.background_hook.lock().unwrap().get_mut(&run.id).unwrap().at = Instant::now() - GRACE - Duration::from_secs(1);
        assert_eq!(reconcile(&app, &run.id, 1), 1);
        assert_eq!(details(&app, &run.id), (Value::Null, Value::Null));
    }

    /// Once the hook snapshot is stale, the pane screen owns the shell count. A finished shell must
    /// not remain counted just because another shell from the same Stop report is still visible.
    #[tokio::test]
    async fn a_stale_hook_shell_count_is_replaced_by_the_current_screen_count() {
        let (env, run) = setup().await;
        let app = env.app.clone();
        let payload = json!({
            "background_tasks": [
                {"id": "shell-1", "type": "shell", "status": "running", "description": "sleep 1"},
                {"id": "shell-2", "type": "shell", "status": "running", "description": "sleep 2"}
            ],
            "session_crons": []
        });
        on_stop(&app, &run, &payload).await;
        app.background_hook.lock().unwrap().get_mut(&run.id).unwrap().at = Instant::now() - GRACE - Duration::from_secs(1);

        assert_eq!(reconcile(&app, &run.id, 1), 1, "the stale hook said two, but only one shell remains on screen");
        assert_eq!(details(&app, &run.id), (Value::Null, Value::Null), "screen-derived counts must not expose stale hook task details");
    }

    /// A hook snapshot is authoritative only inside its freshness window. Once stale, even a
    /// subagent/workflow report must yield to the current screen fallback.
    #[tokio::test]
    async fn a_stale_non_shell_hook_task_falls_back_to_the_current_screen() {
        let (env, run) = setup().await;
        let app = env.app.clone();
        let payload = json!({
            "background_tasks": [{
                "id": "agent-1", "type": "subagent", "status": "running", "description": "review"
            }],
            "session_crons": []
        });
        on_stop(&app, &run, &payload).await;
        app.background_hook.lock().unwrap().get_mut(&run.id).unwrap().at = Instant::now() - GRACE - Duration::from_secs(1);

        assert_eq!(reconcile(&app, &run.id, 2), 2, "stale hook data yields to current screen count");
        assert_eq!(details(&app, &run.id), (Value::Null, Value::Null), "stale hook details are no longer authoritative");
    }

    #[tokio::test]
    async fn a_run_that_ended_leaves_nothing_behind() {
        let (env, run) = setup().await;
        on_stop(&env.app, &run, &fixture()).await;
        retain_runs(&env.app, &[]);
        assert_eq!(details(&env.app, &run.id), (Value::Null, Value::Null));
    }

    /// 端到端：真的走 `hookrecv::process`（fence、bot 鎖、classify），Stop 的 `background_tasks` 進到 API 的 run 物件；
    /// 舊世代（run_id 對不上）的 Stop 不算。
    #[tokio::test]
    async fn the_hook_pipeline_feeds_it_and_a_stale_generation_does_not() {
        let (env, run) = setup().await;
        let app = env.app.clone();
        let body = |run_id: Option<&str>| crate::hookrecv::HookBody {
            bot_id: run.bot_id.clone(),
            provider: "claude".into(),
            payload: fixture(),
            received_at: None,
            truncated: false,
            run_id: run_id.map(String::from),
        };
        crate::hookrecv::process(&app, &body(Some("some-older-run"))).await.unwrap();
        assert_eq!(crate::background_jobs::known(&app, &run.id), None, "上一代的 hook 不改這一代的帳");
        crate::hookrecv::process(&app, &body(Some(&run.id))).await.unwrap();
        assert_eq!(crate::background_jobs::known(&app, &run.id), Some(2));
        let state = crate::api::state_json(&app).await.unwrap();
        assert_eq!(state["projects"][0]["bots"][0]["run"]["background_tasks"][1]["description"], "dev server");
    }
