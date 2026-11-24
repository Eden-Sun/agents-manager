//! #1862 S2a：從 `crates/am-supervisor/src/supervisor/responder.rs` 搬來的 daemon 整合測試（真 `App`／runner／事件／額度路徑），
//! 保留三個子模組階層：`supervisor::responder::{tests,flow_tests,gave_up_report_tests}::…` → `supervisor_tests::responder::{…}::…`。
//! 測試本文與斷言不變；私有函式由 am-supervisor 的 `*_for_test`（test-hooks）轉呼叫，下面用別名維持原名。

#![allow(unused_imports)]

use crate::supervisor::responder::*;
use crate::config::{BotCfg, ProjectCfg};
use crate::lifecycle::{self, LcError};
use crate::supervisor::ports::ConfigProjection;
use sqlx::SqlitePool;
use crate::supervisor::ports::{ControllerRuntime, TurnOps, QuotaOps};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use crate::supervisor::roles::{self, Role};
use crate::supervisor::{store, watchdog};
use crate::state::App;
use std::sync::Arc;
use crate::supervisor::responder::{
    answered_since_for_test as answered_since, digest_for_test as digest, deploy_files_for_test as deploy_files, note_login_problem_for_test as note_login_problem,
    quota_retry_at_for_test as quota_retry_at, report_gave_up_for_test as report_gave_up,
    shared_window_readings_complete_for_test as shared_window_readings_complete, CAPTURE_SETTLE_SECS_FOR_TEST as CAPTURE_SETTLE_SECS,
};

mod tests {
    use super::*;

    /// 協調者的設定做到一半失敗（config 已經寫進 `AGM-responder`，部署檔案那一步失敗）：再按一次要能接著做完，
    /// 不能把上一次自己寫進去的那一顆當成「別的 bot 已經叫這個名字」回 409 `name_taken`（同 `setup` 那條，issue #181）。
    #[tokio::test]
    async fn a_responder_setup_that_failed_halfway_can_be_run_again() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let row = roles::get(&app.db, Role::Responder).await.unwrap();
        app.tools.lock().await.insert(
            crate::config::LOCAL_HOST.to_string(),
            crate::tools::HostTools {
                tools: Default::default(),
                identities: Default::default(),
                shell_identities: vec![crate::config::IdentityCfg { name: row.identity.clone(), kind: "claude".into(), host: None, env: Default::default(), args: vec![] }],
                utc_offset_secs: None, herdr_cli: None, checked_at: crate::db::now(),
            },
        );
        let bin = dir(&app).join("bin");
        std::fs::create_dir_all(dir(&app)).unwrap();
        std::fs::write(&bin, "not a directory").unwrap();
        assert!(ensure_env(&app, None, None, None).await.is_err(), "前提：做到一半失敗");
        std::fs::remove_file(&bin).unwrap();

        let (_, bot_id, _) = ensure_env(&app, None, None, None).await.map_err(|e| format!("{e:?}")).expect("再跑一次要能做完");
        let named: Vec<String> =
            app.cfg.get().await.projects.iter().flat_map(|p| p.bots.iter()).filter(|b| b.name == BOT_NAME).filter_map(|b| b.id.clone()).collect();
        assert_eq!(named, vec![bot_id.clone()], "接著用上一次寫進去的那一顆，不另外長一顆");
        assert_eq!(roles::get(&app.db, Role::Responder).await.unwrap().bot_id.as_deref(), Some(bot_id.as_str()));
    }

    /// 名稱不當參照：做到一半失敗之後那顆 `AGM-responder` 被改名，重跑仍靠記號找回它（同 `setup` 那條）。
    #[tokio::test]
    async fn a_half_made_responder_is_found_by_its_mark_even_after_a_rename() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let row = roles::get(&app.db, Role::Responder).await.unwrap();
        app.tools.lock().await.insert(
            crate::config::LOCAL_HOST.to_string(),
            crate::tools::HostTools {
                tools: Default::default(),
                identities: Default::default(),
                shell_identities: vec![crate::config::IdentityCfg { name: row.identity.clone(), kind: "claude".into(), host: None, env: Default::default(), args: vec![] }],
                utc_offset_secs: None, herdr_cli: None, checked_at: crate::db::now(),
            },
        );
        let bin = dir(&app).join("bin");
        std::fs::create_dir_all(dir(&app)).unwrap();
        std::fs::write(&bin, "not a directory").unwrap();
        assert!(ensure_env(&app, None, None, None).await.is_err(), "前提：做到一半失敗");
        std::fs::remove_file(&bin).unwrap();

        let half_made = app.cfg.get().await.projects.iter().flat_map(|p| p.bots.iter()).find(|b| b.name == BOT_NAME).and_then(|b| b.id.clone()).expect("config 已寫進協調者");
        let id = half_made.clone();
        app.cfg
            .update(move |cfg| {
                for b in cfg.projects.iter_mut().flat_map(|p| p.bots.iter_mut()) {
                    if b.id.as_deref() == Some(id.as_str()) {
                        b.name = "使用者改的名".into();
                    }
                }
                Ok(())
            })
            .await
            .unwrap();
        crate::projection::project_config(&app.cfg, &app.db).await.unwrap();

        let (_, bot_id, _) = ensure_env(&app, None, None, None).await.map_err(|e| format!("{e:?}")).expect("改名後重跑要能做完");
        assert_eq!(bot_id, half_made, "靠記號找回改名後的那一顆");
        let total: usize = app.cfg.get().await.projects.iter().flat_map(|p| p.bots.iter()).count();
        assert_eq!(total, 1, "沒有另外長一顆");
    }

    /// config 上一顆 bot 的最小樣子（`toml::from_str` 省得把每個欄位寫一遍）。
    fn cfg_bot(id: &str, name: &str) -> BotCfg {
        toml::from_str(&format!("id = '{id}'\nname = '{name}'\nkind = 'claude'\n")).unwrap()
    }

    fn cfg_project(id: &str, path: &std::path::Path, label: &str, bots: Vec<BotCfg>) -> ProjectCfg {
        ProjectCfg { mode: "parent_child".into(), role_policy: Default::default(), deputy: None, specialist_dispatch: true,
            handed_off_to: None,
            id: Some(id.into()),
            path: crate::config::canonical_path(&path.to_string_lossy()).unwrap(),
            label: label.into(),
            host: crate::config::LOCAL_HOST.into(),
            bots,
        }
    }

    /// 線上的舊形狀：協調者自成一個專案。併入之後它要跟巡檢同一個專案、還是同一顆 bot（歷史不斷），
    /// 而且**工作目錄不跟著搬**——claude 的 session 以 cwd 為鍵，兩個角色共用目錄會互相接到對方的 session。
    #[tokio::test]
    async fn the_responder_folds_into_the_manager_project_but_keeps_its_own_directory() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let agm_dir = app.data_dir.join("supervisor").join("AGM");
        let resp_dir = dir(app);
        std::fs::create_dir_all(&agm_dir).unwrap();
        std::fs::create_dir_all(&resp_dir).unwrap();
        let resp_path = crate::config::canonical_path(&resp_dir.to_string_lossy()).unwrap();
        app.cfg
            .update(|cfg| {
                cfg.projects = vec![
                    cfg_project("p-agm", &agm_dir, "AGM", vec![cfg_bot("b-agm", "AGM")]),
                    cfg_project("p-resp", &resp_dir, BOT_NAME, vec![cfg_bot("b-resp", BOT_NAME)]),
                ];
                Ok(())
            })
            .await
            .unwrap();
        crate::projection::project_config(&app.cfg, &app.db).await.unwrap();
        store::get_or_init(&app.db).await.unwrap();
        store::set_env(&app.db, "b-agm", "p-agm", &agm_dir.to_string_lossy()).await.unwrap();
        roles::set_env(&app.db, Role::Responder, "b-resp", "p-resp", &resp_path).await.unwrap();

        assert_eq!(merge_into_manager_project(app).await.unwrap().as_deref(), Some("p-agm"));

        let bot = crate::db::bot(&app.db, "b-resp").await.unwrap().unwrap();
        assert_eq!(bot.project_id, "p-agm", "協調者要跟巡檢在同一個專案");
        assert!(bot.deleted_at.is_none(), "是搬家不是重建：同一顆 bot、同一段對話歷史");
        assert_eq!(bot.cwd.as_deref(), Some(resp_path.as_str()));
        let old = crate::db::project(&app.db, "p-resp").await.unwrap().unwrap();
        assert!(old.deleted_at.is_some(), "空掉的舊專案不留在側欄");
        let cfg = app.cfg.get().await;
        assert!(cfg.projects.iter().all(|p| p.id.as_deref() != Some("p-resp")), "config 也不該再有那個專案");
        assert_eq!(cfg.projects.iter().find(|p| p.id.as_deref() == Some("p-agm")).unwrap().bots.len(), 2);
        let row = roles::get(&app.db, Role::Responder).await.unwrap();
        assert_eq!(row.project_id.as_deref(), Some("p-agm"));
        assert_eq!(row.cwd.as_deref(), Some(resp_path.as_str()), "角色表記的還是自己的目錄");

        let before = app.cfg.get().await;
        assert_eq!(merge_into_manager_project(app).await.unwrap().as_deref(), Some("p-agm"));
        assert_eq!(app.cfg.get().await.projects, before.projects);
    }

    /// 巡檢還沒設定（新裝、或 supervisors 那列還沒有專案）時不要亂搬：什麼都不動。
    #[tokio::test]
    async fn without_a_manager_project_nothing_moves() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let resp_dir = dir(app);
        std::fs::create_dir_all(&resp_dir).unwrap();
        app.cfg
            .update(|cfg| {
                cfg.projects = vec![cfg_project("p-resp", &resp_dir, BOT_NAME, vec![cfg_bot("b-resp", BOT_NAME)])];
                Ok(())
            })
            .await
            .unwrap();
        crate::projection::project_config(&app.cfg, &app.db).await.unwrap();
        roles::set_env(&app.db, Role::Responder, "b-resp", "p-resp", &resp_dir.to_string_lossy()).await.unwrap();

        assert_eq!(merge_into_manager_project(app).await.unwrap(), None);
        assert_eq!(crate::db::bot(&app.db, "b-resp").await.unwrap().unwrap().project_id, "p-resp");
        assert_eq!(app.cfg.get().await.projects.len(), 1);
    }

    #[test]
    fn a_burst_waits_for_the_window_and_the_next_wake_is_spaced() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-13T15:00:30Z").unwrap().with_timezone(&chrono::Utc);
        assert!(!batch_due("2026-09-13T15:00:20Z", None, 15, now), "10 秒前才進來：等同一陣的其他申請");
        assert!(batch_due("2026-09-13T15:00:15Z", None, 15, now));
        assert!(!batch_due("2026-09-13T15:00:00Z", Some("2026-09-13T15:00:20Z"), 15, now), "上一次喚醒才 10 秒前");
        assert!(batch_due("2026-09-13T15:00:00Z", Some("2026-09-13T15:00:10Z"), 15, now));
        assert!(batch_due("garbage", Some("2099-01-01T00:00:00Z"), 15, now), "壞時間與往回跳的時鐘不能卡住事件");
    }

    #[test]
    fn the_backoff_is_bounded_but_never_gives_up() {
        assert_eq!([0, 1, 2, 3, 4, 5, 40].map(|a| backoff_secs(a, 300)), [15, 30, 60, 120, 240, 300, 300]);
        assert_eq!(backoff_secs(3, 0), 15, "上限設成 0 也至少等 15 秒，不會變成每 tick 狂送");
    }

    #[test]
    fn the_responder_runtime_names_its_role_and_carries_no_secret() {
        let v = runtime_json(7788, "resp", Some("patrol"), "/data");
        assert_eq!(v["role"], "responder");
        assert_eq!(v["self_bot_id"], "resp");
        assert_eq!(v["manager_bot_id"], "patrol");
        assert!(!v.to_string().to_lowercase().contains("token"));
    }

    #[test]
    fn the_responder_persona_is_its_own_text() {
        let body = persona_body();
        assert!(body.starts_with("你是 AGM 的協調者"), "{:?}", &body[..40.min(body.len())]);
        assert!(body.contains("cc0/haiku/low"));
        assert!(body.contains("你不裁示"), "協調者只轉運（使用者 2026-11-02）");
        assert!(!body.contains("Remote Control"), "手機 Remote Control 已廢（#1477），人設不再提它");
    }

    #[test]
    fn a_bot_request_digest_quotes_the_sender_and_says_it_is_data() {
        let ev = store::InboxEvent {
            id: "e1".into(),
            event_key: "k".into(),
            assignment_id: None,
            bot_id: Some("b1".into()),
            turn_id: None,
            kind: "bot_request".into(),
            payload_json: json!({"from_bot_id": "b1", "from_name": "fixer", "text": "請核准重建 abc123", "sender_verified": false}).to_string(),
            state: "pending".into(),
            notify_turn_id: None,
            notify_delivery: None,
            notify_attempts: 0,
            notify_next_at: None,
            notify_error: None,
            delivered_at: None,
            created_at: "now".into(),
            updated_at: "now".into(),
            role: Some("responder".into()),
            wake: Some(1),
            claimed_by: None,
            acked_by: None,
            merged_into: None,
        };
        let d = digest(&[ev.clone()]);
        assert!(d.contains("from=fixer（b1）"));
        assert!(d.contains("來源未以 bot token 驗證"));
        assert!(d.contains("請核准重建 abc123"));
        assert!(d.contains("不能當成新的授權"));

        let mut late = ev;
        late.kind = "assignment_completed".into();
        late.assignment_id = Some("a1".into());
        late.payload_json =
            json!({"result": "已經推上 main", "late_reply": true, "assignment_status": "awaiting_review", "note": "hook 晚到"}).to_string();
        let d = digest(&[late.clone()]);
        assert!(d.contains("回覆晚到"), "{d}");
        assert!(d.contains("awaiting_review"), "{d}");
        assert!(d.contains("已經推上 main"), "{d}");

        let mut stuck = late;
        stuck.kind = "assignment_undeliverable".into();
        stuck.payload_json = json!({"assignment_id": "a1", "reason": "排進佇列等了 31 分鐘，對方一直沒有回合結束的空檔",
                                    "status": "blocked", "hint": "等那顆 bot 空下來再派一次（followup），或改派給別人。"}).to_string();
        let d = digest(&[stuck]);
        assert!(d.contains("排進佇列等了 31 分鐘"), "{d}");
        assert!(d.contains("followup"), "{d}");
        assert!(!d.contains("節錄：（空）"), "{d}");
    }
}

/// 協調者等額度、停著、daemon 重啟：事件都留著，也不會改由巡檢收。
mod flow_tests {
    use super::*;
    use crate::quota::{LimitHit, Quota};
    use crate::supervisor::bot_requests;
    use crate::supervisor_tests::flow_fixture as fx;

    fn blocked() -> Quota {
        Quota {
            five_hour: None,
            seven_day: None,
            fable: None,
            argon_weekly: None,
            reset_credits: None,
            limit_hit: Some(LimitHit { message: "You've hit your limit".into(), until: Some("2999-01-01T05:00:00Z".into()), at: crate::db::now(), bucket: None }),
            plan: None,
            updated_at: crate::db::now(),
            source: "test".into(),
            account: None,
            host: "local".into(),
        }
    }

    /// 有讀數、沒撞限、視窗也還有：明確「可以用」。
    fn available() -> Quota {
        let w = |used: f64| Some(crate::quota::Window { observed_at: None, used_pct: used, resets_at: Some("2999-01-01T05:00:00Z".into()) });
        Quota { five_hour: w(10.0), seven_day: w(20.0), fable: w(10.0), argon_weekly: None, reset_credits: None, limit_hit: None, plan: None,
                updated_at: crate::db::now(), source: "test".into(), account: None, host: "local".into() }
    }

    async fn age_everything(app: &Arc<App>) {
        sqlx::query("UPDATE supervisor_inbox SET created_at='2026-01-01T00:00:00Z'").execute(&app.db).await.unwrap();
    }

    /// issue #101：兩格都見底時，「最近的重置」是照**時刻**挑的。`resets_at` 有的是 CLI 回報的原樣字串
    /// （秒），有的是毫秒——`.500Z` 在字串上排在 `Z` 前面，照字串挑會挑到晚的那個。
    #[tokio::test]
    async fn the_nearest_reset_is_picked_by_instant_across_precisions() {
        let app = fx::app().await;
        fx::configure_responder(&app).await;
        let bot = roles::responder_bot(&app.db).await.unwrap().unwrap();
        let w = |at: &str| Some(crate::quota::Window { observed_at: None, used_pct: 99.0, resets_at: Some(at.into()) });
        let mut q = available();
        q.five_hour = w("2999-01-01T05:00:00.500Z");
        q.seven_day = w("2999-01-01T05:00:00Z");
        app.quotas.lock().await.insert("claude:cc0".into(), q);
        assert_eq!(quota_state(&app, &bot).await, QuotaState::Blocked(Some("2999-01-01T05:00:00Z".into())));

        let mut q = available();
        q.five_hour = Some(crate::quota::Window { observed_at: None, used_pct: 10.0, resets_at: Some("2999-01-01T05:00:00.500Z".into()) });
        q.seven_day = Some(crate::quota::Window { observed_at: None, used_pct: 10.0, resets_at: Some("2999-01-01T05:00:00Z".into()) });
        q.fable = None;
        q.limit_hit = Some(LimitHit { message: "You've hit your limit".into(), until: None, at: crate::db::now(), bucket: None });
        app.quotas.lock().await.insert("claude:cc0".into(), q);
        assert_eq!(quota_state(&app, &bot).await, QuotaState::Blocked(Some("2999-01-01T05:00:00Z".into())));
    }

    /// 額度見底時下一次再試：重置時間比退避早就等重置。`reset` 是秒格式、`retry` 是毫秒格式時，
    /// 同一秒內舊格式的整點更早——字串比較把它判成比較晚，白白多等一個退避。
    #[test]
    fn the_quota_retry_waits_for_a_reset_that_comes_first_by_instant() {
        let retry = "2999-01-01T05:00:00.500Z";
        assert_eq!(quota_retry_at(Some("2999-01-01T05:00:00Z"), retry.into()), "2999-01-01T05:00:00Z", "05:00:00.000 早於 05:00:00.500");
        assert_eq!(quota_retry_at(Some("2999-01-01T05:00:01Z"), retry.into()), retry, "重置比退避晚就照退避");
        assert_eq!(quota_retry_at(Some("2000-01-01T00:00:00Z"), retry.into()), retry, "已經過去的重置不算");
        assert_eq!(quota_retry_at(None, retry.into()), retry);
    }

    #[tokio::test]
    async fn an_out_of_quota_responder_keeps_its_requests_and_patrol_never_gets_them() {
        let app = fx::app().await;
        fx::configure_responder(&app).await;
        bot_requests::intercept(&app, "patrol", "w1", "請核准重建", Some("r1"), &[], true, "api", bot_requests::ReplyMark::default()).await.unwrap().unwrap();
        age_everything(&app).await;
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES ('run-r','resp','running','idle',?)")
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .unwrap();
        app.quotas.lock().await.insert("claude:cc0".into(), blocked());

        notify(&app).await;
        let row = roles::get(&app.db, Role::Responder).await.unwrap();
        assert_eq!(row.status, "waiting_quota");
        assert_eq!(row.wakes, 0);
        assert!(row.notify_next_at.is_some(), "有下一次重試的時間，而且有上限");
        let (state, attempts): (String, i64) = sqlx::query_as("SELECT state, notify_attempts FROM supervisor_inbox").fetch_one(&app.db).await.unwrap();
        assert_eq!((state.as_str(), attempts), ("pending", 0), "等額度不算送失敗，也不燒重試次數");
        assert!(roles::due_for(&app.db, Role::Patrol, true, "2999-01-01T00:00:00Z", 5).await.unwrap().is_empty(), "不倒回巡檢");
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns").fetch_one(&app.db).await.unwrap();
        assert_eq!(turns, 0);
    }

    /// issue #101：協調者已經在等額度、記著的重置時間是舊版寫的秒格式，新讀數是同一個時刻的毫秒寫法——
    /// 那不是「重置時間變了」，什麼都不該寫（字串 `!=` 會判成變了，白白重寫狀態、多推一次事件）。
    #[tokio::test]
    async fn a_stored_reset_in_the_old_spelling_of_the_same_instant_is_not_rewritten() {
        let app = fx::app().await;
        fx::configure_responder(&app).await;
        bot_requests::intercept(&app, "patrol", "w1", "請核准重建", Some("r1"), &[], true, "api", bot_requests::ReplyMark::default()).await.unwrap().unwrap();
        age_everything(&app).await;
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES ('run-r','resp','running','idle',?)")
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let mut q = blocked();
        q.limit_hit.as_mut().unwrap().until = Some("2999-01-01T05:00:00.000Z".into());
        app.quotas.lock().await.insert("claude:cc0".into(), q);

        notify(&app).await;
        let legacy = "2999-01-01T05:00:00Z";
        sqlx::query("UPDATE supervisor_roles SET quota_reset_at=? WHERE role='responder'").bind(legacy).execute(&app.db).await.unwrap();

        notify(&app).await;
        let row = roles::get(&app.db, Role::Responder).await.unwrap();
        assert_eq!(row.status, "waiting_quota");
        assert_eq!(row.quota_reset_at.as_deref(), Some(legacy), "同一個時刻，不重寫");
    }

    /// `cc0` 這種「就是預設帳號」的身份，env 是空的，讀數寫在**裸 `claude`** 那一把。固定拼
    /// `claude:<identity>` 的話協調者永遠讀不到自己的額度，也就永遠不知道自己撞限了；反過來，
    /// 有自己 env 的 cc2 只讀自己那把，不借用 cc0 的數字。
    #[tokio::test]
    async fn the_quota_key_follows_the_identity_table_not_a_guess() {
        let app = fx::app().await;
        fx::configure_responder(&app).await;
        app.cfg
            .update(|cfg| {
                cfg.identities.push(crate::config::IdentityCfg {
                    name: "cc0".into(),
                    kind: "claude".into(),
                    host: None,
                    env: Default::default(),
                    args: vec![],
                });
                let mut env = std::collections::BTreeMap::new();
                env.insert("CLAUDE_CONFIG_DIR".to_string(), "/home/u/.claude-cc2".to_string());
                cfg.identities.push(crate::config::IdentityCfg {
                    name: "cc2".into(),
                    kind: "claude".into(),
                    host: None,
                    env,
                    args: vec![],
                });
                Ok(())
            })
            .await
            .unwrap();
        let bot = roles::responder_bot(&app.db).await.unwrap().unwrap();

        app.quotas.lock().await.insert("claude".into(), blocked());
        assert!(matches!(quota_state(&app, &bot).await, QuotaState::Blocked(_)), "cc0 要讀得到自己的額度");

        sqlx::query("UPDATE bots SET identity='cc2' WHERE id='resp'").execute(&app.db).await.unwrap();
        let bot = roles::responder_bot(&app.db).await.unwrap().unwrap();
        assert_eq!(quota_state(&app, &bot).await, QuotaState::Unknown, "沒有自己的讀數就是不知道，不借 cc0 的");
        app.quotas.lock().await.insert("claude:cc2".into(), available());
        assert_eq!(quota_state(&app, &bot).await, QuotaState::Available);
    }

    /// 對抗式審查：「可以用」要有**新鮮**的證據。三小時沒更新的讀數、或開機從快取回填、還沒被真探測換掉的（`quota_stale`），
    /// 只能是「不知道」：不然探測壞掉的帳號會一直被宣告恢復。見底（Blocked）不受影響——寧可多擋。
    #[tokio::test]
    async fn an_old_or_cache_restored_reading_cannot_declare_the_account_available() {
        let app = fx::app().await;
        fx::configure_responder(&app).await;
        app.cfg
            .update(|cfg| {
                let mut env = std::collections::BTreeMap::new();
                env.insert("CLAUDE_CONFIG_DIR".to_string(), "/home/u/.claude-cc2".to_string());
                cfg.identities.push(crate::config::IdentityCfg { name: "cc2".into(), kind: "claude".into(), host: None, env, args: vec![] });
                Ok(())
            })
            .await
            .unwrap();
        sqlx::query("UPDATE bots SET identity='cc2' WHERE id='resp'").execute(&app.db).await.unwrap();
        let bot = roles::responder_bot(&app.db).await.unwrap().unwrap();
        let mut old = available();
        old.updated_at = crate::db::iso_at(chrono::Utc::now() - chrono::Duration::hours(3));
        app.quotas.lock().await.insert("claude:cc2".into(), old);
        assert_eq!(quota_state(&app, &bot).await, QuotaState::Unknown, "三小時前的讀數不是證據");

        app.quotas.lock().await.insert("claude:cc2".into(), available());
        app.quota_stale.lock().await.insert("claude:cc2".into());
        assert_eq!(quota_state(&app, &bot).await, QuotaState::Unknown, "開機回填的快取不是證據");

        app.quota_stale.lock().await.clear();
        assert_eq!(quota_state(&app, &bot).await, QuotaState::Available, "新鮮的讀數照舊可用");
    }

    /// 額度是**帳號的**：cc1 的協調者不能看著 cc0 的讀數決定要不要等。讀不到自己的就是不知道，
    /// 不知道不等於見底。
    #[tokio::test]
    async fn another_accounts_quota_is_not_this_responders_quota() {
        let app = fx::app().await;
        fx::configure_responder(&app).await;
        sqlx::query("UPDATE bots SET identity='cc1' WHERE id='resp'").execute(&app.db).await.unwrap();
        bot_requests::intercept(&app, "patrol", "w1", "請核准重建", Some("r1"), &[], true, "api", bot_requests::ReplyMark::default()).await.unwrap().unwrap();
        age_everything(&app).await;
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES ('run-r','resp','running','idle',?)")
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .unwrap();
        app.quotas.lock().await.insert("claude".into(), blocked());
        notify(&app).await;
        assert_ne!(roles::get(&app.db, Role::Responder).await.unwrap().status, "waiting_quota", "別人的額度不是自己的");

        app.quotas.lock().await.insert("claude:cc1".into(), blocked());
        notify(&app).await;
        assert_eq!(roles::get(&app.db, Role::Responder).await.unwrap().status, "waiting_quota");
    }

    /// review3 c3 H2：預設部署下巡檢（cc0、fable）撞 Fable 上限，同是 cc0、跑 opus 的協調者不能跟著等到 Fable 週窗重置。
    /// 看的是協調者實際在跑的模型：`/model` 換成 fable 了，那就是它的撞限。
    #[tokio::test]
    async fn a_fable_hit_does_not_park_a_responder_running_opus() {
        let app = fx::app().await;
        fx::configure_responder(&app).await;
        sqlx::query("UPDATE bots SET model='opus' WHERE id='resp'").execute(&app.db).await.unwrap();
        bot_requests::intercept(&app, "patrol", "w1", "請核准重建", Some("r1"), &[], true, "api", bot_requests::ReplyMark::default()).await.unwrap().unwrap();
        age_everything(&app).await;
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES ('run-r','resp','running','idle',?)")
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let mut q = available();
        q.limit_hit = Some(LimitHit {
            message: "You've reached your Fable limit. Run /usage-credits to continue or switch models with /model.".into(),
            until: Some("2999-01-01T05:00:00Z".into()),
            at: crate::db::now(),
            bucket: Some("fable".into()),
        });
        app.quotas.lock().await.insert("claude:cc0".into(), q);
        let bot = roles::responder_bot(&app.db).await.unwrap().unwrap();

        assert_eq!(quota_state(&app, &bot).await, QuotaState::Available, "Fable 桶不管跑 opus 的協調者");
        notify(&app).await;
        assert_ne!(roles::get(&app.db, Role::Responder).await.unwrap().status, "waiting_quota");

        sqlx::query("UPDATE runs SET runtime_model='fable' WHERE id='run-r'").execute(&app.db).await.unwrap();
        assert!(matches!(quota_state(&app, &bot).await, QuotaState::Blocked(_)), "實際在跑 fable 就是它的撞限");
    }

    /// review3 c3 M3：協調者在 `waiting_quota` 時停掉、而額度讀數一直是 Unknown（探測壞了、帳號沒有那兩個窗），
    /// 以前三個條件互相等：看門狗不重啟、`notify` 因為它沒在跑不送、`answered_since` 要它答完一個回合。
    /// 到期的那一次重試要有活著的協調者才發生得了，所以 Unknown＋時間到就照常啟動；明說被擋的照舊不動。
    #[tokio::test]
    async fn a_responder_waiting_on_an_unknown_quota_is_restarted_when_its_retry_is_due() {
        let app = fx::app().await;
        fx::configure_responder(&app).await;
        roles::set_desired_running(&app.db, Role::Responder, true).await.unwrap();
        roles::set_status(&app.db, Role::Responder, "waiting_quota", Some("撞限"), None).await.unwrap();
        roles::set_notify_next(&app.db, Role::Responder, Some("2999-01-01T00:00:00Z")).await.unwrap();
        watchdog_tick(&app).await;
        let row = roles::get(&app.db, Role::Responder).await.unwrap();
        assert_eq!((row.watchdog_attempts, row.watchdog_next_at), (0, None), "時間沒到就不管它");

        roles::set_notify_next(&app.db, Role::Responder, Some("2020-01-01T00:00:00Z")).await.unwrap();
        watchdog_tick(&app).await;
        assert!(roles::get(&app.db, Role::Responder).await.unwrap().watchdog_next_at.is_some(), "排下一次嘗試");
        roles::set_watchdog(&app.db, Role::Responder, 0, Some("2020-01-01T00:00:00Z"), None).await.unwrap();
        watchdog_tick(&app).await;
        assert_eq!(roles::get(&app.db, Role::Responder).await.unwrap().watchdog_attempts, 1, "Unknown＋到期要試一次");

        roles::set_watchdog(&app.db, Role::Responder, 0, None, None).await.unwrap();
        app.quotas.lock().await.insert("claude:cc0".into(), blocked());
        watchdog_tick(&app).await;
        let row = roles::get(&app.db, Role::Responder).await.unwrap();
        assert_eq!((row.watchdog_attempts, row.watchdog_next_at), (0, None), "有可信證據說被擋就別開");
    }

    /// issue #523：看門狗決定要啟動之後、真的啟動之前，使用者按下 stop（`post_stop` 拿的是同一把鎖）。
    /// 拿到鎖一定要重讀意圖，否則那次 stop 會被蓋掉——bot 跑著而 `desired_running=0`，
    /// 下一拍 `plan_for` 因為 `wanted=false` 回 `Idle`，再也沒有人會把它收回去。
    #[tokio::test]
    async fn a_stop_that_lands_while_the_watchdog_waits_for_the_lock_is_not_overridden() {
        let app = fx::app().await;
        fx::configure_responder(&app).await;
        let bot = roles::responder_bot(&app.db).await.unwrap().unwrap();
        roles::set_desired_running(&app.db, Role::Responder, true).await.unwrap();
        roles::set_watchdog(&app.db, Role::Responder, 0, Some("2020-01-01T00:00:00Z"), None).await.unwrap();
        let db = app.db.clone();
        crate::race_point::arm("responder_watchdog_start", &bot.id, move || async move {
            roles::set_desired_running(&db, Role::Responder, false).await.unwrap();
        });
        watchdog_tick(&app).await;

        let row = roles::get(&app.db, Role::Responder).await.unwrap();
        assert_eq!(row.desired_running, 0, "使用者的 stop 不可以被看門狗改回去");
        assert_eq!(row.watchdog_attempts, 0, "這一次自動啟動要放掉，不算一次嘗試");
        assert_eq!(row.watchdog_last_error, None, "連失敗紀錄都不該有：根本不該試");
        assert!(crate::db::active_run(&app.db, &bot.id).await.unwrap().is_none(), "不可以把它啟動起來");
    }

    #[test]
    fn authoritative_codex_week_only_reading_is_complete() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-12T13:55:00Z").unwrap().with_timezone(&chrono::Utc);
        let mut q = available();
        q.updated_at = "2026-10-12T13:55:00Z".into();
        q.five_hour = None;
        q.source = "codex-app-server".into();
        assert!(shared_window_readings_complete(&q, now));
        q.seven_day = None;
        assert!(!shared_window_readings_complete(&q, now), "empty authoritative response is unknown");
        q = available();
        q.updated_at = "2026-10-12T13:55:00Z".into();
        q.five_hour = None;
        for source in ["codex-statusline", "claude-usage", "statusline", "grok-usage", "agy-usage"] {
            q.source = source.into();
            assert!(!shared_window_readings_complete(&q, now), "{source}: do not treat partial readings as a complete account");
        }
    }

    /// 空的、不完整的讀數都不是「可以用」：少的那一格可能正是見底的那一格。
    #[tokio::test]
    async fn empty_or_partial_readings_are_unknown_not_available() {
        let app = fx::app().await;
        fx::configure_responder(&app).await;
        let bot = roles::responder_bot(&app.db).await.unwrap().unwrap();
        let key = "claude:cc0".to_string();
        let w = |used: f64| Some(crate::quota::Window { observed_at: None, used_pct: used, resets_at: Some("2999-01-01T05:00:00Z".into()) });

        let mut empty = available();
        empty.five_hour = None;
        empty.seven_day = None;
        empty.fable = None;
        app.quotas.lock().await.insert(key.clone(), empty);
        assert_eq!(quota_state(&app, &bot).await, QuotaState::Unknown, "有一列但一格讀數都沒有");

        let mut partial = available();
        partial.seven_day = None;
        app.quotas.lock().await.insert(key.clone(), partial);
        assert_eq!(quota_state(&app, &bot).await, QuotaState::Unknown, "只有 5 小時那格");

        let mut partial_bad = available();
        partial_bad.seven_day = None;
        partial_bad.five_hour = w(100.0);
        app.quotas.lock().await.insert(key.clone(), partial_bad);
        assert!(matches!(quota_state(&app, &bot).await, QuotaState::Blocked(_)), "有的那格已經見底就是見底");

        app.quotas.lock().await.insert(key, available());
        assert_eq!(quota_state(&app, &bot).await, QuotaState::Available);
    }

    /// issue #420：協調者的 CLI 沒登入時，liveness 照樣 idle、health 照樣 healthy，核准申請送了 66 次到過期都沒人知道。
    /// 送不出去時看畫面：看得出要登入就標 `needs_login`（health critical、incident 探針開 `role_unavailable`）；
    /// 畫面恢復就解除。另外不管原因，協調者的事件送了 5 次還在 pending 就開 `responder_undeliverable`（一個協調者一筆）。
    #[tokio::test]
    async fn a_responder_that_is_not_logged_in_is_flagged_and_its_stuck_events_open_an_incident() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        store::get_or_init(&app.db).await.unwrap();
        let now = crate::db::now();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,identity,hook_token,created_at) VALUES ('resp',?,'AGM-responder','claude','cc0','tok',?)")
            .bind(&env.project_id).bind(&now).execute(&app.db).await.unwrap();
        roles::set_env(&app.db, Role::Responder, "resp", &env.project_id, "/tmp").await.unwrap();
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,herdr_session,pane_id,started_at) VALUES ('run-r','resp','running','idle','test','pane-r',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        let bot = crate::db::bot(&app.db, "resp").await.unwrap().unwrap();
        let thresholds = crate::supervisor::incidents::Thresholds::from_cfg(&app.cfg.get().await.supervisor);
        let kinds = |p: &crate::supervisor::incidents::Probed| p.seen.iter().map(|o| (o.kind.clone(), o.severity.clone())).collect::<Vec<_>>();

        env.herdr.set_screen("pane-r", "⏺ 已處理。\n\n❯ \n  AGM-responder | Opus 5 H | 5h:80%\n");
        note_login_problem(&app, &bot, "").await;
        assert_eq!(roles::get(&app.db, Role::Responder).await.unwrap().status, "");

        env.herdr.set_screen("pane-r", crate::tui_prompts::screens::NOT_LOGGED_IN);
        note_login_problem(&app, &bot, "").await;
        let row = roles::get(&app.db, Role::Responder).await.unwrap();
        assert_eq!(row.status, "needs_login");
        assert!(row.waiting_since.is_some(), "記下從什麼時候開始等人登入");
        assert!(row.status_detail.as_deref().unwrap_or("").contains("/login"), "{:?}", row.status_detail);
        let status = status_json(&app).await.unwrap();
        assert_eq!(status["status"], "needs_login", "{status}");
        assert_eq!(status["unavailable_reason"], "needs_login", "#454：機器讀這一欄，不要解析 status");
        assert_eq!(crate::supervisor::health::responder_severity(&status), "critical");
        let probed = crate::supervisor::incidents::observe(&app, &thresholds).await;
        assert!(kinds(&probed).contains(&(crate::supervisor::incidents::ROLE_UNAVAILABLE_KIND.into(), "critical".into())), "{:?}", kinds(&probed));

        notify(&app).await;
        assert_eq!(roles::get(&app.db, Role::Responder).await.unwrap().status, "needs_login");
        env.herdr.fail_next("pane.read", crate::testing::Fault::Refuse);
        notify(&app).await;
        assert_eq!(roles::get(&app.db, Role::Responder).await.unwrap().status, "needs_login");
        env.herdr.set_screen("pane-r", "❯ /login\n  ⎿  Login successful\n\n❯ \n  AGM-responder | Opus 5 H | 5h:99%\n");
        notify(&app).await;
        let row = roles::get(&app.db, Role::Responder).await.unwrap();
        assert_eq!((row.status.as_str(), row.status_detail.as_deref(), row.waiting_since), ("", Some("登入已恢復"), None));
        let probed = crate::supervisor::incidents::observe(&app, &thresholds).await;
        assert!(!kinds(&probed).iter().any(|(k, _)| k == crate::supervisor::incidents::ROLE_UNAVAILABLE_KIND));

        let mut ids = Vec::new();
        for k in ["a1", "a2"] {
            ids.push(store::push_inbox(&app.db, k, "approval_requested", None, None, None, &json!({})).await.unwrap().unwrap());
        }
        roles::classify(&app.db).await.unwrap();
        sqlx::query("UPDATE supervisor_inbox SET notify_attempts=4, notify_error='composer_unreadable'").execute(&app.db).await.unwrap();
        let probed = crate::supervisor::incidents::observe(&app, &thresholds).await;
        assert!(!kinds(&probed).iter().any(|(k, _)| k == "responder_undeliverable"), "4 次還不到");
        sqlx::query("UPDATE supervisor_inbox SET notify_attempts=5 WHERE id=?").bind(&ids[0]).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE supervisor_inbox SET notify_attempts=66 WHERE id=?").bind(&ids[1]).execute(&app.db).await.unwrap();
        let probed = crate::supervisor::incidents::observe(&app, &thresholds).await;
        let stuck: Vec<_> = probed.seen.iter().filter(|o| o.kind == "responder_undeliverable").collect();
        assert_eq!(stuck.len(), 1, "一個協調者一筆，不是一則事件一筆");
        let detail: Value = serde_json::from_str(&stuck[0].detail).unwrap();
        assert_eq!((detail["events"].as_i64(), detail["max_attempts"].as_i64()), (Some(2), Some(66)), "{detail}");
        assert_eq!(stuck[0].severity, "critical");
        assert_eq!(roles::route("incident_opened", &json!({"incident": {"kind": "responder_undeliverable"}}), None).role, Role::Patrol);
        let role_unavailable = |resource: &str| {
            json!({"incident": {"kind": crate::supervisor::incidents::ROLE_UNAVAILABLE_KIND, "resource": resource}})
        };
        assert_eq!(roles::route("incident_opened", &role_unavailable("responder"), None).role, Role::Patrol);
        assert_eq!(roles::route("incident_opened", &role_unavailable("patrol"), None).role, Role::Responder);
        assert_eq!(roles::route("incident_resolved", &role_unavailable("patrol"), None).role, Role::Responder);
        assert!(!roles::route("incident_resolved", &role_unavailable("patrol"), None).wake, "恢復不叫醒人");
    }

    /// 送達不是恢復：prompt 成功（`ok`／`unknown`）只代表字進了 pane 或佇列。還沒有回答、或回合
    /// 以錯誤結束，都不能解除等待；協調者**答完**一個沒出錯的回合才算。
    #[tokio::test]
    async fn a_delivered_notify_is_not_a_quota_recovery_until_the_responder_answers() {
        let app = fx::app().await;
        fx::configure_responder(&app).await;
        roles::set_status(&app.db, Role::Responder, "waiting_quota", Some("撞限"), None).await.unwrap();
        sqlx::query("UPDATE supervisor_roles SET waiting_since='2026-09-14T00:00:00Z' WHERE role='responder'").execute(&app.db).await.unwrap();
        let now = crate::db::now();
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES ('run-r','resp','running','working',?)").bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO conversations (id,bot_id,created_at) VALUES ('c-r','resp',?)").bind(&now).execute(&app.db).await.unwrap();

        for (delivery, turn) in [("ok", "t-ok"), ("unknown", "t-unknown")] {
            sqlx::query("UPDATE turns SET status='queued' WHERE run_id='run-r' AND status='in_flight'").execute(&app.db).await.unwrap();
            sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,created_at) VALUES (?,'c-r','run-r','web','in_flight',?)")
                .bind(turn).bind(&now).execute(&app.db).await.unwrap();
            let id = store::push_inbox(&app.db, &format!("k-{turn}"), "approval_requested", None, None, None, &json!({})).await.unwrap().unwrap();
            roles::classify(&app.db).await.unwrap();
            roles::mark_delivered(&app.db, &[id], Role::Responder, turn, delivery).await.unwrap();
            notify(&app).await;
            let row = roles::get(&app.db, Role::Responder).await.unwrap();
            assert_eq!(row.status, "waiting_quota", "delivery={delivery} 但還沒有回答，不算恢復");
        }

        let a_minute_ago = (chrono::Utc::now() - chrono::Duration::seconds(60)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        sqlx::query("UPDATE turns SET status='completed', completed_at=? WHERE id='t-unknown'")
            .bind(&a_minute_ago)
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("UPDATE runs SET turn_error=? WHERE id='run-r'").bind("You've hit your usage limit").execute(&app.db).await.unwrap();
        notify(&app).await;
        assert_eq!(roles::get(&app.db, Role::Responder).await.unwrap().status, "waiting_quota", "撞限收場的回合不是答得動");

        sqlx::query("UPDATE runs SET turn_error=NULL WHERE id='run-r'").execute(&app.db).await.unwrap();
        notify(&app).await;
        let row = roles::get(&app.db, Role::Responder).await.unwrap();
        assert_eq!(row.status, "");
        assert_eq!(row.status_detail.as_deref(), Some("額度已恢復（協調者完成了一個回合）"));
        assert_eq!(row.waiting_since, None);
    }

    /// review3 c3 L2：撞限收場的回合被算成「答得動」。`runs.turn_error` 是整個 run 一格、下一回合開始就清掉，
    /// 而擷取要等回合收掉約 5 秒才寫得進去——剛好落在這幾秒的 tick 會宣告「額度已恢復」，下一刻又撞限。
    #[tokio::test]
    async fn a_turn_that_just_ended_is_not_yet_proof_and_a_noted_error_never_is() {
        let app = fx::app().await;
        fx::configure_responder(&app).await;
        let now = crate::db::now();
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES ('run-r','resp','running','idle',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO conversations (id,bot_id,created_at) VALUES ('c-r','resp',?)").bind(&now).execute(&app.db).await.unwrap();
        let ago = |secs: i64| (chrono::Utc::now() - chrono::Duration::seconds(secs)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,created_at,completed_at) VALUES ('t1','c-r','run-r','web','completed',?,?)")
            .bind(ago(120)).bind(ago(1)).execute(&app.db).await.unwrap();

        assert!(!answered_since(&app.db, "resp", None).await, "回合剛結束的那幾秒不能當證據");

        sqlx::query("UPDATE turns SET completed_at=? WHERE id='t1'").bind(ago(60)).execute(&app.db).await.unwrap();
        assert!(answered_since(&app.db, "resp", None).await);

        sqlx::query("INSERT INTO messages (id,conversation_id,turn_id,role,content,source,incomplete,created_at) VALUES ('m1','c-r','t1','system','You''ve hit your session limit','system',1,?)")
            .bind(&now).execute(&app.db).await.unwrap();
        assert!(!answered_since(&app.db, "resp", None).await, "撞限收場的回合不是答得動");

        sqlx::query("DELETE FROM messages WHERE id='m1'").execute(&app.db).await.unwrap();
        sqlx::query("UPDATE runs SET turn_error='You''ve hit your session limit' WHERE id='run-r'").execute(&app.db).await.unwrap();
        assert!(!answered_since(&app.db, "resp", None).await, "那格還是這一回合的");
        sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,created_at) VALUES ('t2','c-r','run-r','web','in_flight',?)")
            .bind(crate::db::now()).execute(&app.db).await.unwrap();
        assert!(answered_since(&app.db, "resp", None).await, "更晚開始的回合才是那格的主人");
    }

    /// 查不到協調者屬於哪台主機（專案列不見了）時，不能退回本機、拿本機帳號的數字來判斷。
    #[tokio::test]
    async fn an_unresolvable_host_is_unknown_not_the_local_accounts_reading() {
        let app = fx::app().await;
        fx::configure_responder(&app).await;
        app.quotas.lock().await.insert("claude:cc0".into(), blocked());
        app.quotas.lock().await.insert("claude".into(), blocked());
        sqlx::query("INSERT INTO projects (id,path,label,host,created_at) VALUES ('p-remote','/srv','remote','box-a',?)")
            .bind(crate::db::now()).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE bots SET project_id='p-remote' WHERE id='resp'").execute(&app.db).await.unwrap();
        let bot = roles::responder_bot(&app.db).await.unwrap().unwrap();
        assert_eq!(quota_state(&app, &bot).await, QuotaState::Unknown, "遠端的協調者不借本機的讀數");
        sqlx::query("UPDATE projects SET host='' WHERE id='p-remote'").execute(&app.db).await.unwrap();
        assert_eq!(quota_state(&app, &bot).await, QuotaState::Unknown, "主機不確定就是不知道");
    }

    /// 佇列清空之後**有明確可用讀數**才算恢復；只有「查不到讀數」不能拿來宣稱額度回來了。
    /// 兩件事分開：只在有待辦要送時才重算，佇列清空後 `waiting_quota` 會永遠掛著（看門狗把它
    /// 當成不是故障，協調者再也不會被拉起來）；但把「沒有讀數」當成恢復是謊報。
    #[tokio::test]
    async fn an_empty_queue_clears_the_wait_only_with_a_real_reading() {
        let app = fx::app().await;
        fx::configure_responder(&app).await;
        roles::set_status(&app.db, Role::Responder, "waiting_quota", Some("撞限"), Some("2999-01-01T00:00:00Z")).await.unwrap();
        roles::set_notify_next(&app.db, Role::Responder, Some("2999-01-01T00:00:00Z")).await.unwrap();
        let events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox").fetch_one(&app.db).await.unwrap();
        assert_eq!(events, 0, "佇列是空的");

        notify(&app).await;
        let row = roles::get(&app.db, Role::Responder).await.unwrap();
        assert_eq!(row.status, "waiting_quota", "查不到額度不等於額度回來了");
        assert_eq!(row.notify_next_at.as_deref(), Some("2999-01-01T00:00:00Z"), "也不動它的重試時間");

        app.quotas.lock().await.insert("claude:cc0".into(), available());
        notify(&app).await;
        let row = roles::get(&app.db, Role::Responder).await.unwrap();
        assert_eq!(row.status, "");
        assert_eq!(row.notify_next_at, None);
        assert_eq!(row.status_detail.as_deref(), Some("額度已恢復"));
    }

    /// 撞限期間每個 tick 重算 `iso_in(cap)` 的話，重試時間會永遠往後飄，而且每 10 秒寫一次 DB、
    /// 推一次 SSE。沒有新讀數就不要動它。
    #[tokio::test]
    async fn a_blocked_responder_does_not_push_its_retry_time_every_tick() {
        let app = fx::app().await;
        fx::configure_responder(&app).await;
        bot_requests::intercept(&app, "patrol", "w1", "請核准重建", Some("r1"), &[], true, "api", bot_requests::ReplyMark::default()).await.unwrap().unwrap();
        age_everything(&app).await;
        app.quotas.lock().await.insert("claude:cc0".into(), blocked());

        notify(&app).await;
        let first = roles::get(&app.db, Role::Responder).await.unwrap();
        assert_eq!(first.status, "waiting_quota");
        let retry = first.notify_next_at.clone().expect("排了下一次重試");

        notify(&app).await;
        notify(&app).await;
        let later = roles::get(&app.db, Role::Responder).await.unwrap();
        assert_eq!(later.notify_next_at, Some(retry), "沒有新讀數就不推移重試時間");
        assert_eq!(later.updated_at, first.updated_at, "也不再寫 DB／推事件");
    }

    /// 設定值是 setup 當下的快照；`runtime` 才是它現在實際跑在什麼上面。
    #[tokio::test]
    async fn the_status_reports_the_running_model_next_to_the_configured_one() {
        let app = fx::app().await;
        fx::configure_responder(&app).await;
        sqlx::query(
            "INSERT INTO runs (id,bot_id,state,agent_status,runtime_model,runtime_effort,started_at)
             VALUES ('run-r','resp','running','idle','opus','medium',?)",
        )
        .bind(crate::db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let out = status_json(&app).await.unwrap();
        assert_eq!(out["model"], "opus");
        assert_eq!(out["effort"], "high", "設定值");
        assert_eq!(out["runtime"]["model"], "opus");
        assert_eq!(out["runtime"]["effort"], "medium", "實際跑的強度跟設定不一樣時要看得出來");
    }

    /// setup 的欄位會變成 CLI 的 argv：旗標、空白、超長字串都不能寫進去。
    #[test]
    fn the_model_field_only_takes_a_cli_alias() {
        for ok in ["opus", "fable", "claude-opus-5", "gpt-5.6-luna", "o3"] {
            assert!(valid_model(ok), "{ok}");
        }
        for bad in ["", " ", "--dangerously-skip-permissions", "opus --effort xhigh", "Opus", "模型", &"x".repeat(41)] {
            assert!(!valid_model(bad), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn a_stopped_responder_leaves_everything_pending_across_a_daemon_restart() {
        let app = fx::app().await;
        fx::configure_responder(&app).await;
        bot_requests::intercept(&app, "patrol", "w1", "請協調 ownership", Some("r2"), &[], true, "api", bot_requests::ReplyMark::default()).await.unwrap().unwrap();
        age_everything(&app).await;
        notify(&app).await;
        let db_path = app.data_dir.join("test.sqlite");
        let data_dir = app.data_dir.clone();
        crate::testing::release_scratch(&data_dir);
        drop(app);

        let db = crate::app_ports_p1::open(&db_path).await.unwrap();
        let due = roles::due_for(&db, Role::Responder, true, "2999-01-01T00:00:00Z", 0).await.unwrap();
        assert_eq!(due.len(), 1, "重啟之後還在，下一次協調者起來就收得到");
        assert_eq!(due[0].state, "pending");
        assert_eq!(roles::get(&db, Role::Responder).await.unwrap().wakes, 0);
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    /// 巡檢與協調者（或 UI）同時決定同一筆核准：只有一個寫得進去，另一個拿到 None → 409。
    #[tokio::test]
    async fn two_roles_approving_the_same_request_decide_it_once() {
        let app = fx::app().await;
        let a = store::create_approval(&app.db, "w1", "rebuild", "release", Some("abc"), None, None).await.unwrap().approval;
        let (x, y) = tokio::join!(
            store::decide_approval_from(&app.db, &a.id, "pending", "approved", "AGM:responder", None, None),
            store::decide_approval_from(&app.db, &a.id, "pending", "denied", "AGM:patrol", None, None),
        );
        let wins: Vec<_> = [x.unwrap(), y.unwrap()].into_iter().flatten().collect();
        assert_eq!(wins.len(), 1, "只能有一個裁示");
        let now = store::approval(&app.db, &a.id).await.unwrap().unwrap();
        assert_eq!(now.status, wins[0].0.status);
        let history = store::approval_decisions(&app.db).await.unwrap();
        assert_eq!(history.get(&a.id).map(Vec::len), Some(1));
    }
}

/// #251：協調者看門狗放棄的通知，寫不進 inbox 時不能永遠少一則。
mod gave_up_report_tests {
    use super::*;
    use crate::testing as tt;

    #[tokio::test]
    async fn a_failed_inbox_write_is_retried_until_exactly_one_event_exists() {
        let env = tt::env().await;
        let app = env.app.clone();
        roles::get(&app.db, Role::Responder).await.unwrap();

        tt::make_table_unreadable(&app, "supervisor_inbox").await;
        report_gave_up(&app, "boom").await;
        tt::make_table_readable(&app, "supervisor_inbox").await;
        let at = roles::get(&app.db, Role::Responder).await.unwrap().watchdog_gave_up_at.expect("標記已經寫下");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE kind='responder_watchdog_gave_up'").fetch_one(&app.db).await.unwrap();
        assert_eq!(n, 0);

        for _ in 0..3 {
            report_gave_up(&app, "boom").await;
        }
        let keys: Vec<String> = sqlx::query_scalar("SELECT event_key FROM supervisor_inbox WHERE kind='responder_watchdog_gave_up'")
            .fetch_all(&app.db)
            .await
            .unwrap();
        assert_eq!(keys, vec![format!("responder_watchdog:gave_up:{at}")]);
    }

    /// 去重 key／kind／payload 是合約：快照。
    #[tokio::test]
    async fn responder_gave_up_event_key_kind_and_payload_snapshot() {
        let env = tt::env().await;
        let app = env.app.clone();
        roles::get(&app.db, Role::Responder).await.unwrap();
        report_gave_up(&app, "boom").await;
        let at = roles::get(&app.db, Role::Responder).await.unwrap().watchdog_gave_up_at.unwrap();
        let (key, kind, payload): (String, String, String) =
            sqlx::query_as("SELECT event_key, kind, payload_json FROM supervisor_inbox WHERE kind='responder_watchdog_gave_up'").fetch_one(&app.db).await.unwrap();
        assert_eq!(key, format!("responder_watchdog:gave_up:{at}"));
        assert_eq!(kind, "responder_watchdog_gave_up");
        let want = json!({"why": "boom", "gave_up_at": at, "action": "`bin/agm responder-start`；bot 申請留在 inbox，等超過 5 分鐘的核准會改派給巡檢裁示"});
        assert_eq!(serde_json::from_str::<Value>(&payload).unwrap(), want);
    }

    /// 協調者目錄的檔案要換新 inode（原子寫），不能原地截斷：用 hard link 分辨兩者。
    /// 原地截斷會把 hard link 另一端（這裡當成「正在被別人讀的舊檔」）一起改掉。
    #[tokio::test]
    async fn responder_files_are_replaced_not_truncated_in_place() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let d = dir(&app);
        std::fs::create_dir_all(d.join("bin")).unwrap();
        let outside = d.parent().unwrap().join("responder-keep");
        std::fs::create_dir_all(&outside).unwrap();
        let (keep_persona, keep_agm) = (outside.join("persona.md"), outside.join("agm"));
        std::fs::write(&keep_persona, "KEEP").unwrap();
        std::fs::write(&keep_agm, "KEEP").unwrap();
        for (keep, link) in [(&keep_persona, d.join("persona.md")), (&keep_agm, d.join("bin").join("agm"))] {
            let _ = std::fs::remove_file(&link);
            std::fs::hard_link(keep, &link).unwrap();
        }

        deploy_files(&app, "bot-x", None, "新人設").unwrap();

        let md = std::fs::read_to_string(d.join("CLAUDE.md")).unwrap();
        assert_eq!(md, crate::supervisor::setup::shared_agent_md_for_test());
        assert!(!md.contains("bot-x"));
        assert!(!md.contains(&d.to_string_lossy().to_string()));

        assert_eq!(std::fs::read_to_string(&keep_persona).unwrap(), "KEEP", "persona 不能被原地改寫");
        assert_eq!(std::fs::read_to_string(&keep_agm).unwrap(), "KEEP", "bin/agm 不能被原地改寫");
        assert_eq!(std::fs::read_to_string(d.join("persona.md")).unwrap(), "新人設");
        assert_eq!(std::fs::read_to_string(d.join("bin").join("agm")).unwrap(), crate::supervisor::setup::AGM_CLI);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(d.join("bin").join("agm")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }
        let leftovers: Vec<_> = std::fs::read_dir(&d).unwrap().chain(std::fs::read_dir(d.join("bin")).unwrap())
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "殘留暫存檔：{leftovers:?}");
    }

    #[tokio::test]
    async fn a_responder_start_over_a_stopping_run_is_a_conflict() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        store::get_or_init(&app.db).await.unwrap();
        let now = crate::db::now();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,identity,hook_token,created_at) VALUES ('resp',?,'AGM-responder','claude','cc0','tok',?)")
            .bind(&env.project_id).bind(&now).execute(&app.db).await.unwrap();
        roles::set_env(&app.db, Role::Responder, "resp", &env.project_id, "/tmp").await.unwrap();
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,herdr_session,pane_id,started_at) VALUES ('run-r','resp','stopping','idle','test','pane-r',?)")
            .bind(&now).execute(&app.db).await.unwrap();

        let err = start(&app, None).await.unwrap_err();
        let LcError::Conflict(v) = &err else { panic!("要是 409：{err:?}") };
        assert_eq!(v["reason"], "run_stopping");
    }
}
